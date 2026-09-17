use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch, Mutex};
use tokio_tungstenite::{connect_async, tungstenite::Message, MaybeTlsStream, WebSocketStream};
use tracing::{error, info, warn};

use crate::config::{Config, RewardConfig};

const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const EVENTSUB_URL: &str = "wss://eventsub.wss.twitch.tv/ws?keepalive_timeout_seconds=30";
const SCOPE: &str = "channel:manage:redemptions";

#[derive(Clone)]
pub struct TwitchClient {
    http: Client,
    client_id: String,
    client_secret: Option<String>,
    state_path: PathBuf,
    tokens: Arc<Mutex<TokenState>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TokenState {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    scope: Vec<String>,
}

#[derive(Deserialize)]
struct DeviceAuthorization {
    device_code: String,
    expires_in: u64,
    interval: u64,
    user_code: String,
    verification_uri: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    scope: Vec<String>,
}

#[derive(Deserialize)]
struct OAuthError {
    #[serde(default)]
    message: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Validation {
    pub client_id: String,
    pub user_id: String,
    pub login: String,
    pub scopes: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Reward {
    pub id: String,
    pub title: String,
}

#[derive(Deserialize)]
struct RewardsResponse {
    data: Vec<Reward>,
}

#[derive(Debug, Deserialize)]
struct RedemptionsResponse {
    data: Vec<RedemptionResponse>,
    #[serde(default)]
    pagination: Pagination,
}

#[derive(Debug, Deserialize)]
struct RedemptionResponse {
    id: String,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Default, Deserialize)]
struct Pagination {
    cursor: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Redemption {
    pub id: String,
    pub reward_id: String,
    pub status: String,
}

#[derive(Debug, Eq, PartialEq)]
pub enum RedemptionResolution {
    Resolved,
    Terminal(String),
}

#[derive(Clone, Debug)]
pub struct ManagedReward {
    pub key: String,
    pub scene: String,
}

#[derive(Clone, Debug)]
pub struct ManagedRewards {
    pub by_id: HashMap<String, ManagedReward>,
    retired_ids: Vec<String>,
}

impl ManagedRewards {
    pub fn ids(&self) -> Vec<String> {
        self.by_id.keys().cloned().collect()
    }

    pub fn all_ids(&self) -> Vec<String> {
        self.by_id
            .keys()
            .cloned()
            .chain(self.retired_ids.iter().cloned())
            .collect()
    }
}

#[derive(Default, Deserialize, Serialize)]
struct RewardRegistry {
    rewards: HashMap<String, String>,
    #[serde(default)]
    pending: Option<PendingRedemption>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PendingRedemption {
    pub redemption: Redemption,
    pub scene: String,
    pub delivered: bool,
}

#[derive(Deserialize)]
struct EventMessage {
    metadata: EventMetadata,
    #[serde(default)]
    payload: Value,
}

#[derive(Deserialize)]
struct EventMetadata {
    #[serde(default)]
    message_id: String,
    message_type: String,
    #[serde(default)]
    subscription_type: String,
}

impl TwitchClient {
    pub async fn load(
        client_id: String,
        client_secret: Option<String>,
        state_path: PathBuf,
    ) -> Result<Self> {
        if client_id.trim().is_empty() {
            bail!("TWITCH_CLIENT_ID must not be empty");
        }
        let bytes = tokio::fs::read(&state_path).await.with_context(|| {
            format!(
                "failed to read Twitch OAuth state {}; run `redeemfx auth` first",
                state_path.display()
            )
        })?;
        let tokens = serde_json::from_slice(&bytes).context("invalid Twitch OAuth state")?;
        Ok(Self {
            http: http_client()?,
            client_id,
            client_secret,
            state_path,
            tokens: Arc::new(Mutex::new(tokens)),
        })
    }

    pub async fn authorize(
        client_id: String,
        client_secret: Option<String>,
        state_path: &Path,
    ) -> Result<()> {
        if client_id.trim().is_empty() {
            bail!("TWITCH_CLIENT_ID must not be empty");
        }
        let http = http_client()?;
        let authorization = http
            .post(DEVICE_URL)
            .form(&[("client_id", client_id.as_str()), ("scopes", SCOPE)])
            .send()
            .await
            .context("failed to start Twitch device authorization")?
            .error_for_status()
            .context("Twitch rejected device authorization")?
            .json::<DeviceAuthorization>()
            .await
            .context("invalid Twitch device authorization response")?;

        println!("Open: {}", authorization.verification_uri);
        println!("Enter code: {}", authorization.user_code);
        let deadline = Instant::now() + Duration::from_secs(authorization.expires_in);
        loop {
            if Instant::now() >= deadline {
                bail!("Twitch device authorization expired; run auth again");
            }
            tokio::time::sleep(Duration::from_secs(authorization.interval.max(1))).await;
            let mut form = vec![
                ("client_id", client_id.as_str()),
                ("scopes", SCOPE),
                ("device_code", authorization.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ];
            if let Some(secret) = client_secret.as_deref() {
                form.push(("client_secret", secret));
            }
            let response = http
                .post(TOKEN_URL)
                .form(&form)
                .send()
                .await
                .context("Twitch token request failed")?;
            if response.status().is_success() {
                let token = response
                    .json::<TokenResponse>()
                    .await
                    .context("invalid Twitch token response")?;
                write_token_state(state_path, token.into())?;
                println!("Twitch authorization stored in {}", state_path.display());
                return Ok(());
            }
            let status = response.status();
            let error = response.json::<OAuthError>().await.unwrap_or(OAuthError {
                message: "unknown OAuth error".into(),
            });
            if status == StatusCode::BAD_REQUEST && error.message == "authorization_pending" {
                continue;
            }
            bail!("Twitch authorization failed: {}: {}", status, error.message);
        }
    }

    pub async fn ensure_valid(&self) -> Result<Validation> {
        let access_token = self.tokens.lock().await.access_token.clone();
        let response = self
            .http
            .get(VALIDATE_URL)
            .bearer_auth(&access_token)
            .send()
            .await
            .context("Twitch token validation request failed")?;
        if response.status() == StatusCode::UNAUTHORIZED {
            self.refresh().await?;
            return self.validate_current().await;
        }
        parse_validation(response, &self.client_id).await
    }

    async fn validate_current(&self) -> Result<Validation> {
        let access_token = self.tokens.lock().await.access_token.clone();
        let response = self
            .http
            .get(VALIDATE_URL)
            .bearer_auth(access_token)
            .send()
            .await
            .context("Twitch token validation request failed")?;
        parse_validation(response, &self.client_id).await
    }

    async fn refresh(&self) -> Result<()> {
        let refresh_token = self.tokens.lock().await.refresh_token.clone();
        let mut form = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", self.client_id.as_str()),
        ];
        if let Some(secret) = self.client_secret.as_deref() {
            form.push(("client_secret", secret));
        }
        let response = self
            .http
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .context("Twitch token refresh request failed")?
            .error_for_status()
            .context("Twitch token refresh was rejected; run `redeemfx auth` again")?
            .json::<TokenResponse>()
            .await
            .context("invalid Twitch token refresh response")?;
        let state = TokenState::from(response);
        write_token_state(&self.state_path, state.clone())?;
        *self.tokens.lock().await = state;
        info!("refreshed Twitch access token");
        Ok(())
    }

    async fn access_token(&self) -> String {
        self.tokens.lock().await.access_token.clone()
    }

    async fn subscribe(&self, session_id: &str, broadcaster_id: &str) -> Result<()> {
        for attempt in 0..2 {
            let response = self
                .http
                .post("https://api.twitch.tv/helix/eventsub/subscriptions")
                .header("Client-Id", &self.client_id)
                .bearer_auth(self.access_token().await)
                .json(&json!({
                    "type": "channel.channel_points_custom_reward_redemption.add",
                    "version": "1",
                    "condition": { "broadcaster_user_id": broadcaster_id },
                    "transport": { "method": "websocket", "session_id": session_id }
                }))
                .send()
                .await
                .context("Twitch EventSub subscription request failed")?;
            if response.status() == StatusCode::UNAUTHORIZED && attempt == 0 {
                self.refresh().await?;
                continue;
            }
            response
                .error_for_status()
                .context("Twitch rejected the EventSub subscription")?;
            info!("created Twitch redemption subscription");
            return Ok(());
        }
        unreachable!()
    }

    pub async fn rewards(&self) -> Result<Vec<Reward>> {
        let validation = self.ensure_valid().await?;
        let response = self
            .http
            .get("https://api.twitch.tv/helix/channel_points/custom_rewards")
            .query(&[("broadcaster_id", validation.user_id)])
            .header("Client-Id", &self.client_id)
            .bearer_auth(self.access_token().await)
            .send()
            .await
            .context("Twitch reward request failed")?
            .error_for_status()
            .context("Twitch rejected the reward request")?
            .json::<RewardsResponse>()
            .await
            .context("invalid Twitch reward response")?;
        Ok(response.data)
    }

    pub async fn reconcile_rewards(
        &self,
        config: &Config,
        registry_path: &Path,
    ) -> Result<ManagedRewards> {
        let validation = self.ensure_valid().await?;
        let broadcaster_id = validation.user_id;
        let available = self.manageable_rewards(&broadcaster_id).await?;
        let by_id: HashMap<_, _> = available
            .iter()
            .map(|reward| (reward.id.as_str(), reward))
            .collect();
        let registry = load_reward_registry(registry_path).await?;
        let RewardRegistry {
            rewards: previous_rewards,
            pending,
        } = registry;
        let mut next_registry = RewardRegistry {
            rewards: HashMap::new(),
            pending,
        };
        let mut claimed = HashSet::new();
        let mut managed = HashMap::new();

        for (key, desired) in &config.rewards {
            let stored = previous_rewards
                .get(key)
                .and_then(|id| by_id.get(id.as_str()))
                .filter(|reward| !claimed.contains(reward.id.as_str()))
                .copied();
            let matching = available.iter().find(|reward| {
                reward.title == desired.title && !claimed.contains(reward.id.as_str())
            });
            let reward = match stored.or(matching) {
                Some(reward) => {
                    self.update_reward(&broadcaster_id, &reward.id, desired, true)
                        .await?;
                    reward.clone()
                }
                None => self.create_reward(&broadcaster_id, desired).await?,
            };
            claimed.insert(reward.id.clone());
            next_registry.rewards.insert(key.clone(), reward.id.clone());
            managed.insert(
                reward.id,
                ManagedReward {
                    key: key.clone(),
                    scene: desired.scene.clone(),
                },
            );
        }

        let mut retired_ids = Vec::new();
        for (key, reward_id) in previous_rewards {
            if !config.rewards.contains_key(&key)
                && !claimed.contains(&reward_id)
                && by_id.contains_key(reward_id.as_str())
            {
                self.disable_reward(&broadcaster_id, &reward_id).await?;
                info!(
                    reward_key = key,
                    reward_id, "disabled removed Twitch reward"
                );
                retired_ids.push(reward_id.clone());
                next_registry.rewards.insert(key, reward_id);
            }
        }

        write_reward_registry(registry_path, &next_registry)?;
        Ok(ManagedRewards {
            by_id: managed,
            retired_ids,
        })
    }

    pub async fn set_paused(&self, reward_ids: &[String], paused: bool) -> Result<()> {
        let broadcaster_id = self.ensure_valid().await?.user_id;
        let mut first_error = None;
        for reward_id in reward_ids {
            if let Err(error) = self
                .patch_reward(&broadcaster_id, reward_id, json!({ "is_paused": paused }))
                .await
            {
                error!(reward_id, paused, %error, "failed to update Twitch reward pause state");
                first_error.get_or_insert(error);
            }
        }
        match first_error {
            Some(error) if !paused => {
                for reward_id in reward_ids {
                    if let Err(rollback_error) = self
                        .patch_reward(&broadcaster_id, reward_id, json!({ "is_paused": true }))
                        .await
                    {
                        error!(reward_id, %rollback_error, "failed to roll back partial Twitch reward unpause");
                    }
                }
                Err(error)
            }
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub async fn pending_redemption(
        &self,
        registry_path: &Path,
    ) -> Result<Option<PendingRedemption>> {
        Ok(load_reward_registry(registry_path).await?.pending)
    }

    pub async fn store_pending(
        &self,
        registry_path: &Path,
        redemption: &Redemption,
        scene: &str,
        delivered: bool,
    ) -> Result<()> {
        let mut registry = load_reward_registry(registry_path).await?;
        registry.pending = Some(PendingRedemption {
            redemption: redemption.clone(),
            scene: scene.to_owned(),
            delivered,
        });
        write_reward_registry(registry_path, &registry)
    }

    pub async fn clear_pending(&self, registry_path: &Path) -> Result<()> {
        let mut registry = load_reward_registry(registry_path).await?;
        registry.pending = None;
        write_reward_registry(registry_path, &registry)
    }

    pub async fn cancel_unfulfilled(&self, reward_ids: &[String]) -> Result<()> {
        let broadcaster_id = self.ensure_valid().await?.user_id;
        for reward_id in reward_ids {
            let mut after: Option<String> = None;
            loop {
                let mut request = self
                    .http
                    .get("https://api.twitch.tv/helix/channel_points/custom_rewards/redemptions")
                    .query(&[
                        ("broadcaster_id", broadcaster_id.as_str()),
                        ("reward_id", reward_id.as_str()),
                        ("status", "UNFULFILLED"),
                        ("sort", "OLDEST"),
                        ("first", "50"),
                    ])
                    .header("Client-Id", &self.client_id)
                    .bearer_auth(self.access_token().await);
                if let Some(cursor) = after.as_deref() {
                    request = request.query(&[("after", cursor)]);
                }
                let response = request
                    .send()
                    .await
                    .context("Twitch redemption request failed")?
                    .error_for_status()
                    .context("Twitch rejected the redemption request")?
                    .json::<RedemptionsResponse>()
                    .await
                    .context("invalid Twitch redemption response")?;
                let ids: Vec<_> = response.data.into_iter().map(|item| item.id).collect();
                if !ids.is_empty() {
                    self.update_redemptions(&broadcaster_id, reward_id, &ids, "CANCELED")
                        .await?;
                    info!(
                        reward_id,
                        count = ids.len(),
                        "canceled stale Twitch redemptions"
                    );
                }
                let Some(cursor) = response.pagination.cursor else {
                    break;
                };
                after = Some(cursor);
            }
        }
        Ok(())
    }

    pub async fn resolve_redemption(
        &self,
        redemption: &Redemption,
        status: &str,
    ) -> Result<RedemptionResolution> {
        let broadcaster_id = self.ensure_valid().await?.user_id;
        let result = self
            .update_redemptions(
                &broadcaster_id,
                &redemption.reward_id,
                std::slice::from_ref(&redemption.id),
                status,
            )
            .await;
        if result.is_ok() {
            return Ok(RedemptionResolution::Resolved);
        }
        match self
            .redemption_status(&broadcaster_id, &redemption.reward_id, &redemption.id)
            .await
        {
            Ok(current) if current == status => Ok(RedemptionResolution::Resolved),
            Ok(current) if current != "UNFULFILLED" => Ok(RedemptionResolution::Terminal(current)),
            _ => {
                result?;
                unreachable!()
            }
        }
    }

    pub async fn redemption_status_for(&self, redemption: &Redemption) -> Result<String> {
        let broadcaster_id = self.ensure_valid().await?.user_id;
        self.redemption_status(&broadcaster_id, &redemption.reward_id, &redemption.id)
            .await
    }

    async fn manageable_rewards(&self, broadcaster_id: &str) -> Result<Vec<Reward>> {
        let response = self
            .http
            .get("https://api.twitch.tv/helix/channel_points/custom_rewards")
            .query(&[
                ("broadcaster_id", broadcaster_id),
                ("only_manageable_rewards", "true"),
            ])
            .header("Client-Id", &self.client_id)
            .bearer_auth(self.access_token().await)
            .send()
            .await
            .context("Twitch manageable reward request failed")?
            .error_for_status()
            .context("Twitch rejected the manageable reward request")?
            .json::<RewardsResponse>()
            .await
            .context("invalid Twitch manageable reward response")?;
        Ok(response.data)
    }

    async fn create_reward(&self, broadcaster_id: &str, desired: &RewardConfig) -> Result<Reward> {
        let response = self
            .http
            .post("https://api.twitch.tv/helix/channel_points/custom_rewards")
            .query(&[("broadcaster_id", broadcaster_id)])
            .header("Client-Id", &self.client_id)
            .bearer_auth(self.access_token().await)
            .json(&json!({
                "title": desired.title,
                "cost": desired.cost,
                "background_color": desired.color,
                "is_enabled": false,
                "is_max_per_stream_enabled": true,
                "max_per_stream": desired.max_per_stream,
                "should_redemptions_skip_request_queue": false
            }))
            .send()
            .await
            .context("Twitch reward creation request failed")?
            .error_for_status()
            .context("Twitch rejected reward creation")?
            .json::<RewardsResponse>()
            .await
            .context("invalid Twitch reward creation response")?;
        let reward = response
            .data
            .into_iter()
            .next()
            .context("Twitch omitted created reward")?;
        self.update_reward(broadcaster_id, &reward.id, desired, true)
            .await?;
        info!(
            reward_id = reward.id,
            title = desired.title,
            "created Twitch reward"
        );
        Ok(reward)
    }

    async fn update_reward(
        &self,
        broadcaster_id: &str,
        reward_id: &str,
        desired: &RewardConfig,
        paused: bool,
    ) -> Result<()> {
        self.patch_reward(
            broadcaster_id,
            reward_id,
            json!({
                "title": desired.title,
                "cost": desired.cost,
                "background_color": desired.color,
                "is_enabled": true,
                "is_paused": paused,
                "is_max_per_stream_enabled": true,
                "max_per_stream": desired.max_per_stream,
                "should_redemptions_skip_request_queue": false
            }),
        )
        .await
    }

    async fn disable_reward(&self, broadcaster_id: &str, reward_id: &str) -> Result<()> {
        self.patch_reward(broadcaster_id, reward_id, json!({ "is_enabled": false }))
            .await
    }

    async fn patch_reward(&self, broadcaster_id: &str, reward_id: &str, body: Value) -> Result<()> {
        self.http
            .patch("https://api.twitch.tv/helix/channel_points/custom_rewards")
            .query(&[("broadcaster_id", broadcaster_id), ("id", reward_id)])
            .header("Client-Id", &self.client_id)
            .bearer_auth(self.access_token().await)
            .json(&body)
            .send()
            .await
            .context("Twitch reward update request failed")?
            .error_for_status()
            .context("Twitch rejected the reward update")?;
        Ok(())
    }

    async fn update_redemptions(
        &self,
        broadcaster_id: &str,
        reward_id: &str,
        redemption_ids: &[String],
        status: &str,
    ) -> Result<()> {
        let mut request = self
            .http
            .patch("https://api.twitch.tv/helix/channel_points/custom_rewards/redemptions")
            .query(&[("broadcaster_id", broadcaster_id), ("reward_id", reward_id)]);
        for id in redemption_ids {
            request = request.query(&[("id", id)]);
        }
        request
            .header("Client-Id", &self.client_id)
            .bearer_auth(self.access_token().await)
            .json(&json!({ "status": status }))
            .send()
            .await
            .context("Twitch redemption update request failed")?
            .error_for_status()
            .context("Twitch rejected the redemption update")?;
        Ok(())
    }

    async fn redemption_status(
        &self,
        broadcaster_id: &str,
        reward_id: &str,
        redemption_id: &str,
    ) -> Result<String> {
        let response = self
            .http
            .get("https://api.twitch.tv/helix/channel_points/custom_rewards/redemptions")
            .query(&[
                ("broadcaster_id", broadcaster_id),
                ("reward_id", reward_id),
                ("id", redemption_id),
            ])
            .header("Client-Id", &self.client_id)
            .bearer_auth(self.access_token().await)
            .send()
            .await
            .context("Twitch redemption status request failed")?
            .error_for_status()
            .context("Twitch rejected the redemption status request")?
            .json::<RedemptionsResponse>()
            .await
            .context("invalid Twitch redemption status response")?;
        response
            .data
            .into_iter()
            .next()
            .map(|redemption| redemption.status)
            .context("Twitch omitted requested redemption")
    }
}

impl From<TokenResponse> for TokenState {
    fn from(response: TokenResponse) -> Self {
        Self {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            scope: response.scope,
        }
    }
}

async fn parse_validation(response: reqwest::Response, client_id: &str) -> Result<Validation> {
    let validation = response
        .error_for_status()
        .context("Twitch token is invalid; run `redeemfx auth` again")?
        .json::<Validation>()
        .await
        .context("invalid Twitch validation response")?;
    if validation.client_id != client_id {
        bail!("stored Twitch token belongs to a different client ID; run auth again");
    }
    if !validation.scopes.iter().any(|scope| scope == SCOPE) {
        bail!("stored Twitch token lacks {SCOPE}; run auth again");
    }
    Ok(validation)
}

fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("failed to build Twitch HTTP client")
}

fn write_token_state(path: &Path, state: TokenState) -> Result<()> {
    let parent = path.parent().context("OAuth state path has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create OAuth state directory {}",
            parent.display()
        )
    })?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("failed to create OAuth state {}", temporary.display()))?;
    serde_json::to_writer(&mut file, &state).context("failed to encode OAuth state")?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to install OAuth state {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

async fn load_reward_registry(path: &Path) -> Result<RewardRegistry> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid Twitch reward registry"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RewardRegistry::default()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to read reward registry {}", path.display()))
        }
    }
}

fn write_reward_registry(path: &Path, registry: &RewardRegistry) -> Result<()> {
    let parent = path
        .parent()
        .context("reward registry path has no parent")?;
    fs::create_dir_all(parent).with_context(|| {
        format!(
            "failed to create reward registry directory {}",
            parent.display()
        )
    })?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let temporary = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .with_context(|| format!("failed to create reward registry {}", temporary.display()))?;
    serde_json::to_writer(&mut file, registry).context("failed to encode reward registry")?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(&temporary, path)
        .with_context(|| format!("failed to install reward registry {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

pub async fn listen(
    client: TwitchClient,
    redemptions: mpsc::Sender<Redemption>,
    reward_ids: Vec<String>,
    readiness: watch::Sender<bool>,
    busy: watch::Receiver<bool>,
) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    let mut seen = HashSet::new();
    let mut seen_order = VecDeque::new();

    loop {
        match listen_once(
            &client,
            &redemptions,
            &reward_ids,
            &readiness,
            &busy,
            &mut seen,
            &mut seen_order,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(error) => {
                readiness.send(false).ok();
                if let Err(pause_error) = client.set_paused(&reward_ids, true).await {
                    error!(%pause_error, "failed to pause Twitch rewards after EventSub disconnect");
                }
                warn!(
                    %error,
                    cause = %error.root_cause(),
                    retry_seconds = backoff.as_secs(),
                    "Twitch connection lost"
                );
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

async fn listen_once(
    client: &TwitchClient,
    redemptions: &mpsc::Sender<Redemption>,
    reward_ids: &[String],
    readiness: &watch::Sender<bool>,
    busy: &watch::Receiver<bool>,
    seen: &mut HashSet<String>,
    seen_order: &mut VecDeque<String>,
) -> Result<()> {
    let validation = client.ensure_valid().await?;
    info!(login = validation.login, "connecting to Twitch EventSub");
    let (mut socket, mut keepalive) =
        connect_session(client, EVENTSUB_URL, &validation.user_id, false).await?;
    if !*busy.borrow() {
        client.set_paused(reward_ids, false).await?;
    }
    readiness.send(true).ok();
    let mut validation_interval = tokio::time::interval(Duration::from_secs(60 * 60));
    validation_interval.tick().await;

    loop {
        tokio::select! {
            message = tokio::time::timeout(Duration::from_secs(keepalive + 10), socket.next()) => {
                let message = message.context("Twitch keepalive timed out")?
                    .context("Twitch WebSocket closed")??;
                match message {
                    Message::Ping(data) => socket.send(Message::Pong(data)).await?,
                    Message::Close(frame) => bail!("Twitch closed WebSocket: {frame:?}"),
                    other => {
                        let Some(event) = parse_text_message(other)? else { continue };
                        if let Some(reconnect_url) = handle_event(
                            event,
                            redemptions,
                            seen,
                            seen_order,
                        ).await? {
                            info!("Twitch requested EventSub reconnect");
                            let handoff = connect_session(
                                client,
                                &reconnect_url,
                                &validation.user_id,
                                true,
                            );
                            tokio::pin!(handoff);
                            loop {
                                tokio::select! {
                                    result = &mut handoff => {
                                        (socket, keepalive) = result?;
                                        break;
                                    }
                                    old = socket.next() => {
                                        let Some(old) = old else {
                                            (socket, keepalive) = handoff.await?;
                                            break;
                                        };
                                        match old? {
                                            Message::Ping(data) => socket.send(Message::Pong(data)).await?,
                                            Message::Close(_) => {
                                                (socket, keepalive) = handoff.await?;
                                                break;
                                            }
                                            other => {
                                                if let Some(event) = parse_text_message(other)? {
                                                    handle_event(event, redemptions, seen, seen_order).await?;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ = validation_interval.tick() => {
                client.ensure_valid().await.context("hourly Twitch token validation failed")?;
                info!("validated Twitch access token");
            }
        }
    }
}

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_session(
    client: &TwitchClient,
    url: &str,
    broadcaster_id: &str,
    inherited_subscription: bool,
) -> Result<(Socket, u64)> {
    let (mut socket, _) = connect_async(url)
        .await
        .context("failed to connect to Twitch EventSub WebSocket")?;
    let welcome = tokio::time::timeout(Duration::from_secs(15), receive_welcome(&mut socket))
        .await
        .context("timed out waiting for Twitch welcome")??;
    if welcome.metadata.message_type != "session_welcome" {
        bail!(
            "expected Twitch session_welcome, got {}",
            welcome.metadata.message_type
        );
    }
    let session = welcome
        .payload
        .get("session")
        .context("Twitch welcome omitted session")?;
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .context("Twitch welcome omitted session ID")?;
    let keepalive = session
        .get("keepalive_timeout_seconds")
        .and_then(Value::as_u64)
        .unwrap_or(30);
    if !inherited_subscription {
        client.subscribe(session_id, broadcaster_id).await?;
    }
    info!(session_id, "Twitch EventSub connected");
    Ok((socket, keepalive))
}

async fn receive_welcome(socket: &mut Socket) -> Result<EventMessage> {
    loop {
        let message = socket
            .next()
            .await
            .context("Twitch closed before welcome")??;
        match message {
            Message::Ping(data) => socket.send(Message::Pong(data)).await?,
            Message::Close(frame) => bail!("Twitch closed before welcome: {frame:?}"),
            other => {
                if let Some(event) = parse_text_message(other)? {
                    return Ok(event);
                }
            }
        }
    }
}

async fn handle_event(
    event: EventMessage,
    redemptions: &mpsc::Sender<Redemption>,
    seen: &mut HashSet<String>,
    seen_order: &mut VecDeque<String>,
) -> Result<Option<String>> {
    match event.metadata.message_type.as_str() {
        "session_reconnect" => {
            let reconnect_url = event
                .payload
                .get("session")
                .and_then(|session| session.get("reconnect_url"))
                .and_then(Value::as_str)
                .context("Twitch reconnect omitted URL")?;
            Ok(Some(reconnect_url.to_owned()))
        }
        "notification"
            if event.metadata.subscription_type
                == "channel.channel_points_custom_reward_redemption.add" =>
        {
            if !event.metadata.message_id.is_empty()
                && !seen.insert(event.metadata.message_id.clone())
            {
                return Ok(None);
            }
            if !event.metadata.message_id.is_empty() {
                seen_order.push_back(event.metadata.message_id);
                if seen_order.len() > 1024 {
                    if let Some(expired) = seen_order.pop_front() {
                        seen.remove(&expired);
                    }
                }
            }
            let redemption = event
                .payload
                .get("event")
                .context("Twitch redemption omitted event")?;
            let id = redemption
                .get("id")
                .and_then(Value::as_str)
                .context("Twitch redemption omitted redemption ID")?;
            let reward_id = redemption
                .get("reward")
                .and_then(|reward| reward.get("id"))
                .and_then(Value::as_str)
                .context("Twitch redemption omitted reward ID")?;
            let status = redemption
                .get("status")
                .and_then(Value::as_str)
                .context("Twitch redemption omitted status")?;
            redemptions
                .send(Redemption {
                    id: id.to_owned(),
                    reward_id: reward_id.to_owned(),
                    status: status.to_owned(),
                })
                .await
                .context("redemption worker stopped")?;
            Ok(None)
        }
        "revocation" => {
            error!(payload = %event.payload, "Twitch revoked EventSub subscription");
            bail!("Twitch revoked the EventSub subscription")
        }
        _ => Ok(None),
    }
}

fn parse_text_message(message: Message) -> Result<Option<EventMessage>> {
    let text = match message {
        Message::Text(text) => text,
        Message::Binary(bytes) => String::from_utf8(bytes.to_vec())
            .context("Twitch sent non-UTF-8 binary message")?
            .into(),
        _ => return Ok(None),
    };
    match serde_json::from_str(&text) {
        Ok(message) => Ok(Some(message)),
        Err(error) => {
            warn!(%error, "ignored malformed Twitch EventSub message");
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REDEMPTION: &str = include_str!("../tests/fixtures/twitch-redemption.json");

    #[test]
    fn eventsub_uses_secure_websocket_transport() {
        assert!(EVENTSUB_URL.starts_with("wss://"));
    }

    #[test]
    fn parses_redemption_fixture() {
        let message: EventMessage = serde_json::from_str(REDEMPTION).unwrap();
        assert_eq!(message.metadata.message_type, "notification");
        assert_eq!(message.payload["event"]["id"], "redemption-fixture-id");
        assert_eq!(
            message.payload["event"]["reward"]["id"],
            "reward-fixture-id"
        );
    }

    #[tokio::test]
    async fn ignores_duplicate_eventsub_message_ids() {
        let (sender, mut receiver) = mpsc::channel(2);
        let mut seen = HashSet::new();
        let mut seen_order = VecDeque::new();
        for _ in 0..2 {
            let message: EventMessage = serde_json::from_str(REDEMPTION).unwrap();
            handle_event(message, &sender, &mut seen, &mut seen_order)
                .await
                .unwrap();
        }
        assert_eq!(
            receiver.recv().await,
            Some(Redemption {
                id: "redemption-fixture-id".into(),
                reward_id: "reward-fixture-id".into(),
                status: "unfulfilled".into(),
            })
        );
        assert!(receiver.try_recv().is_err());
    }
}
