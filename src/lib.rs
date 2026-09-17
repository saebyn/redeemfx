pub mod config;
pub mod govee;
pub mod twitch;

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, watch};
use tracing::{error, info, warn};

use crate::{
    config::Config,
    govee::GoveeClient,
    twitch::{ManagedRewards, Redemption, RedemptionResolution, TwitchClient},
};

#[derive(Debug, Default)]
pub struct RedemptionState {
    active: bool,
}

impl RedemptionState {
    pub fn claim(&mut self) -> bool {
        if self.active {
            return false;
        }
        self.active = true;
        true
    }

    pub fn release(&mut self) {
        self.active = false;
    }
}

pub async fn run_actions(
    config: Arc<Config>,
    govee: GoveeClient,
    twitch: TwitchClient,
    managed: ManagedRewards,
    registry_path: PathBuf,
    mut redemptions: mpsc::Receiver<Redemption>,
    mut shutdown: watch::Receiver<bool>,
    busy: watch::Sender<bool>,
) {
    let reward_ids = managed.ids();
    let mut state = RedemptionState::default();
    let mut restoration: Option<tokio::time::Instant> = None;

    loop {
        tokio::select! {
            biased;

            redemption = redemptions.recv() => {
                let Some(redemption) = redemption else { break };
                let Some(reward) = managed.by_id.get(&redemption.reward_id) else {
                    continue;
                };
                if redemption.status != "unfulfilled" {
                    warn!(
                        redemption_id = redemption.id,
                        reward_id = redemption.reward_id,
                        status = redemption.status,
                        "ignored redemption that is not queued"
                    );
                    continue;
                }
                if !state.claim() {
                    resolve_until_success(&twitch, &redemption, "CANCELED").await;
                    continue;
                }
                busy.send(true).ok();

                info!(
                    redemption_id = redemption.id,
                    reward_id = redemption.reward_id,
                    reward_key = reward.key,
                    scene = reward.scene,
                    "accepted redemption"
                );
                if let Err(error) = twitch.set_paused(&reward_ids, true).await {
                    error!(%error, "failed to pause all Twitch rewards");
                    resolve_until_success(&twitch, &redemption, "CANCELED").await;
                    restoration = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                    continue;
                }
                if let Err(error) = twitch
                    .store_pending(&registry_path, &redemption, &reward.scene, false)
                    .await
                {
                    error!(%error, "failed to persist accepted redemption");
                    resolve_until_success(&twitch, &redemption, "CANCELED").await;
                    restoration = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                    continue;
                }
                if let Err(error) = govee.activate(&config.device, &reward.scene).await {
                    error!(scene = reward.scene, %error, "failed to activate redemption scene");
                    resolve_until_success(&twitch, &redemption, "CANCELED").await;
                    clear_pending_until_success(&twitch, &registry_path).await;
                    restoration = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                    continue;
                }

                info!(scene = reward.scene, "activated redemption scene");
                let deadline = tokio::time::Instant::now()
                    + Duration::from_secs(config.effect_duration_seconds);
                store_delivered_until_success(
                    &twitch,
                    &registry_path,
                    &redemption,
                    &reward.scene,
                ).await;
                resolve_until_success(&twitch, &redemption, "FULFILLED").await;
                clear_pending_until_success(&twitch, &registry_path).await;
                restoration = Some(deadline);
            }
            _ = async {
                match restoration {
                    Some(deadline) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending().await,
                }
            } => {
                restoration = None;
                if restore_and_unpause(&config, &govee, &twitch, &reward_ids).await {
                    state.release();
                    busy.send(false).ok();
                } else {
                    restoration = Some(tokio::time::Instant::now() + Duration::from_secs(5));
                }
            }
            changed = shutdown.changed() => {
                if changed.is_ok() && *shutdown.borrow() {
                    if let Err(error) = twitch.set_paused(&reward_ids, true).await {
                        error!(%error, "failed to pause Twitch rewards during shutdown");
                    }
                    if state.active {
                        restore_default(&config, &govee).await;
                    }
                }
                break;
            }
        }
    }
}

async fn resolve_until_success(twitch: &TwitchClient, redemption: &Redemption, status: &str) {
    let mut attempt = 1_u64;
    loop {
        match twitch.resolve_redemption(redemption, status).await {
            Ok(RedemptionResolution::Resolved) => {
                info!(
                    redemption_id = redemption.id,
                    reward_id = redemption.reward_id,
                    status,
                    "resolved Twitch redemption"
                );
                return;
            }
            Ok(RedemptionResolution::Terminal(current)) => {
                warn!(
                    redemption_id = redemption.id,
                    reward_id = redemption.reward_id,
                    requested_status = status,
                    current_status = current,
                    "redemption was already resolved with a different status"
                );
                return;
            }
            Err(error) => {
                warn!(
                    redemption_id = redemption.id,
                    reward_id = redemption.reward_id,
                    status,
                    attempt,
                    %error,
                    "failed to resolve Twitch redemption; keeping rewards locked"
                );
                attempt += 1;
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

async fn store_delivered_until_success(
    twitch: &TwitchClient,
    registry_path: &std::path::Path,
    redemption: &Redemption,
    scene: &str,
) {
    while let Err(error) = twitch
        .store_pending(registry_path, redemption, scene, true)
        .await
    {
        error!(%error, "failed to persist delivered redemption; keeping rewards locked");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn clear_pending_until_success(twitch: &TwitchClient, registry_path: &std::path::Path) {
    while let Err(error) = twitch.clear_pending(registry_path).await {
        error!(%error, "failed to clear resolved redemption state; keeping rewards locked");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

pub async fn recover_pending(
    config: &Config,
    govee: &GoveeClient,
    twitch: &TwitchClient,
    registry_path: &std::path::Path,
) -> Result<()> {
    let Some(pending) = twitch.pending_redemption(registry_path).await? else {
        return Ok(());
    };
    let status = twitch.redemption_status_for(&pending.redemption).await?;
    if status == "CANCELED" {
        twitch.clear_pending(registry_path).await?;
        return Ok(());
    }
    govee
        .activate(&config.device, &pending.scene)
        .await
        .context("failed to replay interrupted redemption scene")?;
    twitch
        .store_pending(registry_path, &pending.redemption, &pending.scene, true)
        .await?;
    if status == "UNFULFILLED" {
        let resolution = twitch
            .resolve_redemption(&pending.redemption, "FULFILLED")
            .await
            .context("failed to fulfill interrupted redemption")?;
        if resolution != RedemptionResolution::Resolved {
            warn!(
                ?resolution,
                "interrupted redemption changed status during recovery"
            );
        }
    }
    twitch.clear_pending(registry_path).await?;
    tokio::time::sleep(Duration::from_secs(config.effect_duration_seconds)).await;
    Ok(())
}

async fn restore_default(config: &Config, govee: &GoveeClient) -> bool {
    if let Err(error) = govee.activate(&config.device, &config.default_scene).await {
        error!(scene = config.default_scene, %error, "failed to restore default scene");
        return false;
    }
    info!(scene = config.default_scene, "restored default scene");
    true
}

async fn restore_and_unpause(
    config: &Config,
    govee: &GoveeClient,
    twitch: &TwitchClient,
    reward_ids: &[String],
) -> bool {
    if !restore_default(config, govee).await {
        return false;
    }
    if let Err(error) = twitch.set_paused(reward_ids, false).await {
        error!(%error, "failed to unpause all Twitch rewards");
        return false;
    }
    info!("unpaused Twitch rewards");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_redemption_can_claim_the_active_slot() {
        let mut state = RedemptionState::default();
        assert!(state.claim());
        assert!(!state.claim());
        state.release();
        assert!(state.claim());
    }
}
