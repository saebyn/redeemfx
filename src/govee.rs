use std::{collections::HashSet, fmt, time::Duration};

use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;
use uuid::Uuid;

use crate::config::DeviceRef;

const API_BASE: &str = "https://openapi.api.govee.com/router/api/v1";

#[derive(Clone)]
pub struct GoveeClient {
    client: Client,
    api_key: String,
    api_base: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Device {
    pub sku: String,
    pub device: String,
    #[serde(rename = "deviceName", default)]
    pub name: String,
    #[serde(default)]
    pub capabilities: Vec<Capability>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Capability {
    #[serde(rename = "type")]
    pub capability_type: String,
    pub instance: String,
    #[serde(default)]
    pub parameters: Value,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SceneRef {
    #[serde(rename = "type")]
    pub capability_type: String,
    pub instance: String,
    pub value: Value,
}

impl SceneRef {
    pub fn encode(&self) -> Result<String> {
        let json = serde_json::to_vec(self).context("failed to encode scene reference")?;
        Ok(format!("govee-scene:{}", URL_SAFE_NO_PAD.encode(json)))
    }

    pub fn parse(reference: &str) -> Result<Self> {
        let encoded = reference
            .strip_prefix("govee-scene:")
            .context("scene reference must start with govee-scene:")?;
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .context("scene reference has invalid base64url data")?;
        let scene: Self =
            serde_json::from_slice(&bytes).context("scene reference has invalid JSON data")?;
        if scene.capability_type.trim().is_empty() || scene.instance.trim().is_empty() {
            bail!("scene capability type and instance must not be empty");
        }
        if scene.value.is_null() {
            bail!("scene value must not be null");
        }
        Ok(scene)
    }
}

impl fmt::Display for DeviceRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}/{}", self.sku, self.device)
    }
}

#[derive(Debug)]
pub struct Scene {
    pub name: String,
    pub reference: SceneRef,
}

#[derive(Deserialize)]
struct DevicesResponse {
    code: i64,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Vec<Device>,
}

#[derive(Deserialize)]
struct CapabilitiesResponse {
    code: i64,
    #[serde(default, alias = "message")]
    msg: String,
    #[serde(default)]
    payload: CapabilityPayload,
}

#[derive(Default, Deserialize)]
struct CapabilityPayload {
    #[serde(default)]
    capabilities: Vec<Capability>,
}

impl GoveeClient {
    pub fn new(api_key: String) -> Result<Self> {
        Self::with_base(api_key, API_BASE.to_owned())
    }

    fn with_base(api_key: String, api_base: String) -> Result<Self> {
        if api_key.trim().is_empty() {
            bail!("GOVEE_API_KEY must not be empty");
        }
        Ok(Self {
            client: Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .context("failed to build Govee HTTP client")?,
            api_key,
            api_base,
        })
    }

    pub async fn devices(&self) -> Result<Vec<Device>> {
        let response = self
            .client
            .get(format!("{}/user/devices", self.api_base))
            .header("Govee-API-Key", &self.api_key)
            .send()
            .await
            .context("Govee device request failed")?
            .error_for_status()
            .context("Govee device request returned an HTTP error")?
            .json::<DevicesResponse>()
            .await
            .context("invalid Govee device response")?;
        if response.code != 200 {
            bail!(
                "Govee device request failed: code {}: {}",
                response.code,
                response.message
            );
        }
        Ok(response.data)
    }

    pub async fn scenes(&self, device: &DeviceRef) -> Result<Vec<Scene>> {
        let devices = self.devices().await?;
        let selected = devices
            .into_iter()
            .find(|candidate| candidate.sku == device.sku && candidate.device == device.device)
            .with_context(|| format!("Govee device {device} was not returned by the API"))?;

        let mut capabilities = selected.capabilities;
        for endpoint in ["device/scenes", "device/diy-scenes"] {
            match self.scene_capabilities(endpoint, device).await {
                Ok(mut discovered) => capabilities.append(&mut discovered),
                Err(error) => warn!(endpoint, %error, "Govee scene endpoint unavailable"),
            }
        }
        Ok(extract_scenes(capabilities))
    }

    async fn scene_capabilities(
        &self,
        endpoint: &str,
        device: &DeviceRef,
    ) -> Result<Vec<Capability>> {
        let response = self
            .client
            .post(format!("{}/{}", self.api_base, endpoint))
            .header("Govee-API-Key", &self.api_key)
            .json(&json!({
                "requestId": Uuid::new_v4().to_string(),
                "payload": { "sku": device.sku, "device": device.device }
            }))
            .send()
            .await
            .with_context(|| format!("Govee {endpoint} request failed"))?;
        if response.status() == StatusCode::NOT_FOUND {
            bail!("not supported by this device or account");
        }
        let response = response
            .error_for_status()
            .with_context(|| format!("Govee {endpoint} request returned an HTTP error"))?
            .json::<CapabilitiesResponse>()
            .await
            .with_context(|| format!("invalid Govee {endpoint} response"))?;
        if response.code != 200 {
            bail!("code {}: {}", response.code, response.msg);
        }
        Ok(response.payload.capabilities)
    }

    pub async fn activate(&self, device: &DeviceRef, reference: &str) -> Result<()> {
        let scene = SceneRef::parse(reference)?;
        let response = self
            .client
            .post(format!("{}/device/control", self.api_base))
            .header("Govee-API-Key", &self.api_key)
            .json(&json!({
                "requestId": Uuid::new_v4().to_string(),
                "payload": {
                    "sku": device.sku,
                    "device": device.device,
                    "capability": {
                        "type": scene.capability_type,
                        "instance": scene.instance,
                        "value": scene.value
                    }
                }
            }))
            .send()
            .await
            .context("Govee control request failed")?
            .error_for_status()
            .context("Govee control request returned an HTTP error")?
            .json::<Value>()
            .await
            .context("invalid Govee control response")?;
        if response.get("code").and_then(Value::as_i64) != Some(200) {
            bail!(
                "Govee control request failed: code {}; message: {}",
                response.get("code").unwrap_or(&Value::Null),
                response
                    .get("msg")
                    .or_else(|| response.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error")
            );
        }
        Ok(())
    }
}

fn extract_scenes(capabilities: Vec<Capability>) -> Vec<Scene> {
    let mut seen = HashSet::new();
    let mut scenes = Vec::new();
    for capability in capabilities {
        let is_scene = capability.capability_type.contains("scene")
            || matches!(
                capability.instance.as_str(),
                "lightScene" | "diyScene" | "snapshot"
            );
        if !is_scene {
            continue;
        }
        let Some(options) = capability
            .parameters
            .get("options")
            .and_then(Value::as_array)
        else {
            continue;
        };
        for option in options {
            let (Some(name), Some(value)) = (
                option.get("name").and_then(Value::as_str),
                option.get("value"),
            ) else {
                continue;
            };
            let reference = SceneRef {
                capability_type: capability.capability_type.clone(),
                instance: capability.instance.clone(),
                value: value.clone(),
            };
            if let Ok(encoded) = reference.encode() {
                if seen.insert(encoded) {
                    scenes.push(Scene {
                        name: name.to_owned(),
                        reference,
                    });
                }
            }
        }
    }
    scenes.sort_by(|left, right| left.name.cmp(&right.name));
    scenes
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEVICES_FIXTURE: &str = include_str!("../tests/fixtures/govee-devices.json");
    const SCENES_FIXTURE: &str = include_str!("../tests/fixtures/govee-scenes.json");

    #[test]
    fn scene_reference_round_trip_preserves_object_value() {
        let scene = SceneRef {
            capability_type: "devices.capabilities.dynamic_scene".into(),
            instance: "lightScene".into(),
            value: json!({"paramId": 4280, "id": 3853}),
        };
        assert_eq!(SceneRef::parse(&scene.encode().unwrap()).unwrap(), scene);
    }

    #[test]
    fn rejects_malformed_scene_reference() {
        assert!(SceneRef::parse("lightScene:3853").is_err());
        assert!(SceneRef::parse("govee-scene:not-base64!").is_err());
    }

    #[test]
    fn parses_device_response_fixture() {
        let response: DevicesResponse = serde_json::from_str(DEVICES_FIXTURE).unwrap();
        assert_eq!(response.data[0].name, "Wall Lights");
        assert_eq!(response.data[0].capabilities[0].instance, "powerSwitch");
    }

    #[test]
    fn parses_scene_response_fixture() {
        let response: CapabilitiesResponse = serde_json::from_str(SCENES_FIXTURE).unwrap();
        let scenes = extract_scenes(response.payload.capabilities);
        assert_eq!(scenes.len(), 2);
        assert_eq!(scenes[0].name, "Rainbow");
        assert_eq!(
            scenes[1].reference.value,
            json!({"id": 3853, "paramId": 4280})
        );
    }
}
