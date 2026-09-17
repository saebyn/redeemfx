use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::govee::SceneRef;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceRef {
    pub sku: String,
    pub device: String,
}

impl DeviceRef {
    pub fn parse(value: &str) -> Result<Self> {
        let Some((sku, device)) = value.split_once('/') else {
            bail!("Govee device must use the canonical SKU/device form");
        };
        if sku.trim().is_empty() || device.trim().is_empty() {
            bail!("Govee device SKU and device ID must not be empty");
        }
        Ok(Self {
            sku: sku.to_owned(),
            device: device.to_owned(),
        })
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub device: DeviceRef,
    pub default_scene: String,
    pub rewards: HashMap<String, RewardConfig>,
    pub effect_duration_seconds: u64,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RewardConfig {
    pub title: String,
    pub cost: u64,
    pub color: String,
    pub scene: String,
    pub max_per_stream: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    govee_device: String,
    default_scene: String,
    rewards: HashMap<String, RewardConfig>,
    effect_duration_seconds: u64,
}

impl Config {
    pub async fn load(path: &Path) -> Result<Self> {
        let contents = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read configuration {}", path.display()))?;
        Self::parse(&contents)
    }

    pub fn parse(contents: &str) -> Result<Self> {
        let raw: RawConfig = toml::from_str(contents).context("invalid configuration TOML")?;
        if raw.effect_duration_seconds == 0 {
            bail!("effect_duration_seconds must be greater than zero");
        }
        if raw.rewards.is_empty() {
            bail!("at least one reward mapping is required");
        }

        let device = DeviceRef::parse(&raw.govee_device)?;
        SceneRef::parse(&raw.default_scene).context("invalid default_scene")?;
        let mut titles = HashSet::new();
        for (key, reward) in &raw.rewards {
            if key.trim().is_empty() {
                bail!("reward keys must not be empty");
            }
            if reward.title.trim().is_empty() || reward.title.chars().count() > 45 {
                bail!("reward {key} title must contain between 1 and 45 characters");
            }
            if !titles.insert(reward.title.as_str()) {
                bail!(
                    "reward titles must be unique; duplicate title {}",
                    reward.title
                );
            }
            if reward.cost == 0 {
                bail!("reward {key} cost must be greater than zero");
            }
            if reward.max_per_stream == 0 {
                bail!("reward {key} max_per_stream must be greater than zero");
            }
            if !is_hex_color(&reward.color) {
                bail!("reward {key} color must use #RRGGBB format");
            }
            SceneRef::parse(&reward.scene)
                .with_context(|| format!("invalid scene for reward {key}"))?;
        }

        Ok(Self {
            device,
            default_scene: raw.default_scene,
            rewards: raw.rewards,
            effect_duration_seconds: raw.effect_duration_seconds,
        })
    }
}

fn is_hex_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::govee::SceneRef;

    fn scene(value: serde_json::Value) -> String {
        SceneRef {
            capability_type: "devices.capabilities.dynamic_scene".into(),
            instance: "lightScene".into(),
            value,
        }
        .encode()
        .unwrap()
    }

    #[test]
    fn parses_valid_configuration() {
        let scene = scene(json!({"id": 2, "paramId": 1}));
        let input = format!(
            "govee_device = \"H6061/AA:BB\"\ndefault_scene = \"{scene}\"\neffect_duration_seconds = 60\n[rewards.red]\ntitle = \"Go Red\"\ncost = 51\ncolor = \"#FF0000\"\nscene = \"{scene}\"\nmax_per_stream = 99\n"
        );
        let config = Config::parse(&input).unwrap();
        assert_eq!(config.device.sku, "H6061");
        assert_eq!(config.effect_duration_seconds, 60);
    }

    #[test]
    fn rejects_zero_duration() {
        let scene = scene(json!(1));
        let input = format!(
            "govee_device = \"H1/device\"\ndefault_scene = \"{scene}\"\neffect_duration_seconds = 0\n[rewards.a]\ntitle = \"A\"\ncost = 1\ncolor = \"#000000\"\nscene = \"{scene}\"\nmax_per_stream = 1\n"
        );
        assert!(Config::parse(&input).is_err());
    }

    #[test]
    fn rejects_unknown_fields() {
        let scene = scene(json!(1));
        let input = format!(
            "govee_device = \"H1/device\"\ndefault_scene = \"{scene}\"\neffect_duration_seconds = 1\nextra = true\n[rewards.a]\ntitle = \"A\"\ncost = 1\ncolor = \"#000000\"\nscene = \"{scene}\"\nmax_per_stream = 1\n"
        );
        assert!(Config::parse(&input).is_err());
    }

    #[test]
    fn rejects_invalid_reward_color() {
        let scene = scene(json!(1));
        let input = format!(
            "govee_device = \"H1/device\"\ndefault_scene = \"{scene}\"\neffect_duration_seconds = 1\n[rewards.a]\ntitle = \"A\"\ncost = 1\ncolor = \"red\"\nscene = \"{scene}\"\nmax_per_stream = 1\n"
        );
        assert!(Config::parse(&input).is_err());
    }
}
