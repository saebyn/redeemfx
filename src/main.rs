use std::{env, path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use redeemfx::{
    config::{Config, DeviceRef},
    govee::GoveeClient,
    recover_pending, run_actions,
    twitch::{self, TwitchClient},
};
use tokio::sync::{mpsc, watch};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[arg(
        long,
        env = "REDEEMFX_CONFIG",
        default_value = "/etc/redeemfx.toml",
        global = true
    )]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Perform one-time Twitch Device Code authorization.
    Auth,
    /// List Govee devices and their capabilities.
    ListDevices,
    /// List scenes for a device, using the configured device by default.
    ListScenes {
        #[arg(long)]
        device: Option<String>,
    },
    /// List Twitch rewards and their stable IDs.
    ListRewards,
    /// Activate a scene, using the configured device by default.
    ActivateScene {
        scene_reference: String,
        #[arg(long)]
        device: Option<String>,
    },
    /// Run the EventSub listener and restoration worker.
    Run,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let is_service = matches!(cli.command, Command::Run);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if is_service {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .json()
            .with_current_span(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }

    match cli.command {
        Command::Auth => {
            let (client_id, client_secret) = twitch_credentials()?;
            TwitchClient::authorize(client_id, client_secret, &oauth_state_path()?).await
        }
        Command::ListDevices => {
            let govee = govee_client()?;
            for device in govee.devices().await? {
                println!(
                    "{}\n  reference: {}/{}\n  capabilities:",
                    if device.name.is_empty() {
                        "(unnamed)"
                    } else {
                        &device.name
                    },
                    device.sku,
                    device.device
                );
                for capability in device.capabilities {
                    println!("    {}/{}", capability.capability_type, capability.instance);
                }
            }
            Ok(())
        }
        Command::ListScenes { device } => {
            let selected = selected_device(device, cli.config).await?;
            let govee = govee_client()?;
            for scene in govee.scenes(&selected).await? {
                println!(
                    "{}\n  capability: {}/{}\n  reference: {}",
                    scene.name,
                    scene.reference.capability_type,
                    scene.reference.instance,
                    scene.reference.encode()?
                );
            }
            Ok(())
        }
        Command::ListRewards => {
            let twitch = load_twitch().await?;
            for reward in twitch.rewards().await? {
                println!("{}\n  id: {}", reward.title, reward.id);
            }
            Ok(())
        }
        Command::ActivateScene {
            scene_reference,
            device,
        } => {
            let selected = selected_device(device, cli.config).await?;
            govee_client()?
                .activate(&selected, &scene_reference)
                .await?;
            println!("Activated {scene_reference}");
            Ok(())
        }
        Command::Run => {
            let config = Arc::new(load_config(cli.config).await?);
            let govee = govee_client()?;
            let twitch = load_twitch().await?;
            let registry_path = reward_registry_path()?;
            let managed = twitch
                .reconcile_rewards(&config, &registry_path)
                .await
                .context("failed to reconcile Twitch rewards")?;
            let reward_ids = managed.ids();
            let all_reward_ids = managed.all_ids();
            let startup = async {
                recover_pending(&config, &govee, &twitch, &registry_path)
                    .await
                    .context("failed to recover an interrupted redemption")?;
                twitch
                    .cancel_unfulfilled(&all_reward_ids)
                    .await
                    .context("failed to refund stale Twitch redemptions")?;
                govee
                    .activate(&config.device, &config.default_scene)
                    .await
                    .context("failed to restore the default scene during startup")
            };
            tokio::pin!(startup);
            tokio::select! {
                result = &mut startup => result?,
                signal = shutdown_signal() => {
                    signal?;
                    twitch.set_paused(&reward_ids, true).await.ok();
                    govee.activate(&config.device, &config.default_scene).await.ok();
                    return Ok(());
                }
            }
            drop(startup);
            let (sender, receiver) = mpsc::channel(32);
            let (shutdown_sender, shutdown_receiver) = watch::channel(false);
            let (busy_sender, busy_receiver) = watch::channel(false);
            let (readiness_sender, mut readiness_receiver) = watch::channel(false);
            let actions = run_actions(
                config.clone(),
                govee.clone(),
                twitch.clone(),
                managed,
                registry_path.clone(),
                receiver,
                shutdown_receiver,
                busy_sender.clone(),
            );
            let listener = twitch::listen(
                twitch.clone(),
                sender,
                reward_ids.clone(),
                readiness_sender,
                busy_receiver,
            );
            tokio::pin!(listener);

            loop {
                tokio::select! {
                    result = &mut listener => {
                        twitch.set_paused(&reward_ids, true).await.ok();
                        govee.activate(&config.device, &config.default_scene).await.ok();
                        return result;
                    },
                    changed = readiness_receiver.changed() => {
                        changed.context("Twitch listener stopped before becoming ready")?;
                        if *readiness_receiver.borrow() {
                            break;
                        }
                    }
                    signal = shutdown_signal() => {
                        signal?;
                        busy_sender.send(true).ok();
                        twitch.set_paused(&reward_ids, true).await.ok();
                        govee.activate(&config.device, &config.default_scene).await.ok();
                        return Ok(());
                    }
                }
            }

            tokio::pin!(actions);
            let (result, actions_finished) = tokio::select! {
                result = &mut listener => {
                    busy_sender.send(true).ok();
                    (result, false)
                },
                _ = &mut actions => {
                    busy_sender.send(true).ok();
                    (Ok(()), true)
                },
                signal = shutdown_signal() => {
                    signal?;
                    tracing::info!("shutting down");
                    busy_sender.send(true).ok();
                    (Ok(()), false)
                }
            };
            shutdown_sender.send(true).ok();
            if !actions_finished
                && tokio::time::timeout(std::time::Duration::from_secs(60), &mut actions)
                    .await
                    .is_err()
            {
                tracing::warn!("timed out waiting for redemption worker shutdown");
            }
            twitch
                .set_paused(&reward_ids, true)
                .await
                .context("failed to pause Twitch rewards during shutdown")?;
            govee
                .activate(&config.device, &config.default_scene)
                .await
                .context("failed to restore default scene during shutdown")?;
            result
        }
    }
}

async fn load_config(path: Option<PathBuf>) -> Result<Config> {
    let path = path.context("--config or REDEEMFX_CONFIG is required")?;
    Config::load(&path).await
}

async fn selected_device(device: Option<String>, config: Option<PathBuf>) -> Result<DeviceRef> {
    match device {
        Some(device) => DeviceRef::parse(&device),
        None => Ok(load_config(config).await?.device),
    }
}

fn govee_client() -> Result<GoveeClient> {
    GoveeClient::new(env::var("GOVEE_API_KEY").context("GOVEE_API_KEY is required")?)
}

fn twitch_credentials() -> Result<(String, Option<String>)> {
    Ok((
        env::var("TWITCH_CLIENT_ID").context("TWITCH_CLIENT_ID is required")?,
        env::var("TWITCH_CLIENT_SECRET").ok(),
    ))
}

async fn load_twitch() -> Result<TwitchClient> {
    let (client_id, client_secret) = twitch_credentials()?;
    TwitchClient::load(client_id, client_secret, oauth_state_path()?).await
}

fn oauth_state_path() -> Result<PathBuf> {
    Ok(state_directory()?.join("twitch-oauth.json"))
}

fn reward_registry_path() -> Result<PathBuf> {
    Ok(state_directory()?.join("twitch-rewards.json"))
}

fn state_directory() -> Result<PathBuf> {
    if let Some(directory) = env::var_os("STATE_DIRECTORY") {
        return Ok(PathBuf::from(directory));
    }
    if let Some(directory) = env::var_os("REDEEMFX_STATE_DIR") {
        return Ok(PathBuf::from(directory));
    }
    if let Some(directory) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(directory).join("redeemfx"));
    }
    let home = env::var_os("HOME").context("HOME is required to locate OAuth state")?;
    Ok(PathBuf::from(home).join(".local/state/redeemfx"))
}

#[cfg(unix)]
async fn shutdown_signal() -> Result<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = signal(SignalKind::terminate()).context("failed to watch SIGTERM")?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("failed to watch SIGINT"),
        _ = terminate.recv() => Ok(()),
    }
}

#[cfg(not(unix))]
async fn shutdown_signal() -> Result<()> {
    tokio::signal::ctrl_c()
        .await
        .context("failed to watch shutdown signal")
}
