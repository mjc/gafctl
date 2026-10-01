use serde::{Deserialize, Serialize};

use crate::control::ControlPreset;

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(try_from = "String")]
pub struct DeviceId(String);

impl DeviceId {
    pub fn configured_ble() -> Self {
        Self("configured".to_owned())
    }

    pub fn parse(value: String) -> Option<Self> {
        Self::is_valid(&value).then_some(Self(value))
    }

    pub fn quickconnect() -> Self {
        Self(format!("qc-{}", uuid::Uuid::new_v4().simple()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid(value: &str) -> bool {
        !value.is_empty()
            && value.len() <= 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    }
}

impl TryFrom<String> for DeviceId {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value).ok_or("invalid local device identifier")
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceBackend {
    LegacyBle,
    QuickConnect,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EntitySource {
    Http,
    Mqtt,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeviceCommand {
    LegacyPreset {
        preset: ControlPreset,
    },
    QuickConnectMode {
        mode: QuickConnectMode,
    },
    QuickConnectTargets {
        temperature_f: u16,
        humidity_percent: u16,
    },
    QuickConnectTimerDuration {
        minutes: u16,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickConnectMode {
    Off,
    Automatic,
    Timer,
    Manual,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QuickConnectModeStatus {
    Off,
    Automatic,
    Timer,
    Manual,
    Unknown,
    Conflicting,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CommandCapability {
    LegacyPreset(ControlPreset),
    QuickConnectMode,
    QuickConnectTargets,
    QuickConnectTimerDuration,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceCapabilities {
    pub read_state: bool,
    pub commands: Vec<CommandCapability>,
}

impl DeviceCapabilities {
    pub fn legacy_ble() -> Self {
        Self {
            read_state: true,
            commands: [
                ControlPreset::Automatic105F30Percent,
                ControlPreset::Automatic105_1F30_1Percent,
                ControlPreset::TimerClear,
                ControlPreset::TimerOneMinute,
            ]
            .into_iter()
            .map(CommandCapability::LegacyPreset)
            .collect(),
        }
    }

    pub fn quickconnect_read_only() -> Self {
        Self {
            read_state: true,
            commands: Vec::new(),
        }
    }

    pub fn quickconnect_with_controls() -> Self {
        Self {
            read_state: true,
            commands: vec![
                CommandCapability::QuickConnectMode,
                CommandCapability::QuickConnectTargets,
                CommandCapability::QuickConnectTimerDuration,
            ],
        }
    }

    pub fn supports(&self, command: DeviceCommand) -> bool {
        let capability = match command {
            DeviceCommand::LegacyPreset { preset } => CommandCapability::LegacyPreset(preset),
            DeviceCommand::QuickConnectMode { .. } => CommandCapability::QuickConnectMode,
            DeviceCommand::QuickConnectTargets { .. } => CommandCapability::QuickConnectTargets,
            DeviceCommand::QuickConnectTimerDuration { .. } => {
                CommandCapability::QuickConnectTimerDuration
            }
        };
        self.commands.contains(&capability)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceDescriptor {
    pub id: DeviceId,
    pub name: String,
    pub backend: DeviceBackend,
    pub capabilities: DeviceCapabilities,
    pub state_source: EntitySource,
    pub command_source: EntitySource,
}

impl DeviceDescriptor {
    pub fn configured_ble() -> Self {
        Self {
            id: DeviceId::configured_ble(),
            name: "GAF Wi-Fi Vent".to_owned(),
            backend: DeviceBackend::LegacyBle,
            capabilities: DeviceCapabilities::legacy_ble(),
            state_source: EntitySource::Http,
            command_source: EntitySource::Http,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyMode {
    Automatic,
    Timer,
    Ota,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "backend", rename_all = "snake_case")]
pub enum DeviceSettings {
    LegacyBle {
        mode: Option<LegacyMode>,
        controller_fan_on: Option<bool>,
        automatic_temperature_tenths_f: Option<u16>,
        automatic_humidity_tenths_percent: Option<u16>,
        timer_remaining_minutes: Option<u16>,
        timer_original_minutes: Option<u16>,
    },
    QuickConnect {
        mode: QuickConnectModeStatus,
        automatic_temperature_f: Option<u16>,
        automatic_humidity_percent: Option<u16>,
        timer_duration_minutes: Option<u16>,
        humidity_monitor: Option<bool>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StateProvenance {
    pub backend: DeviceBackend,
    pub fetched_at_unix_ms: Option<u64>,
    pub observed_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeviceState {
    pub temperature_f: Option<f64>,
    pub humidity_percent: Option<f64>,
    pub settings: DeviceSettings,
    pub estimated_running: Option<bool>,
    pub diagnostics: Option<DeviceDiagnostics>,
    pub provenance: StateProvenance,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceDiagnostics {
    pub firmware_version: Option<String>,
    pub signal_strength_raw: Option<String>,
    pub verified_raw: Option<String>,
    pub ota_in_progress: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_keep_legacy_presets_separate_from_cloud_commands() {
        let legacy = DeviceCapabilities::legacy_ble();
        assert!(legacy.supports(DeviceCommand::LegacyPreset {
            preset: ControlPreset::TimerClear,
        }));
        assert!(!legacy.supports(DeviceCommand::QuickConnectMode {
            mode: QuickConnectMode::Automatic,
        }));

        let cloud = DeviceCapabilities::quickconnect_read_only();
        assert!(cloud.read_state);
        assert!(cloud.commands.is_empty());
        assert!(!cloud.supports(DeviceCommand::QuickConnectTimerDuration { minutes: 30 }));
    }

    #[test]
    fn local_identifiers_reject_path_syntax_and_keep_provider_ids_private() {
        assert!(DeviceId::parse("local-device_1".to_owned()).is_some());
        assert!(DeviceId::parse("../provider-secret".to_owned()).is_none());
        assert!(serde_json::from_str::<DeviceId>("\"../provider-secret\"").is_err());
        let local_id = DeviceId::quickconnect();
        assert!(local_id.as_str().starts_with("qc-"));
        assert!(!local_id.as_str().contains("provider-secret"));
    }

    #[test]
    fn state_keeps_missing_measurements_unknown_and_targets_separate() {
        let state = DeviceState {
            temperature_f: Some(101.0),
            humidity_percent: None,
            settings: DeviceSettings::QuickConnect {
                mode: QuickConnectModeStatus::Automatic,
                automatic_temperature_f: Some(105),
                automatic_humidity_percent: Some(40),
                timer_duration_minutes: None,
                humidity_monitor: None,
            },
            estimated_running: None,
            diagnostics: None,
            provenance: StateProvenance {
                backend: DeviceBackend::QuickConnect,
                fetched_at_unix_ms: Some(500),
                observed_at_unix_ms: None,
            },
        };
        let payload = serde_json::to_value(state).unwrap();
        assert_eq!(payload["temperature_f"], 101.0);
        assert!(payload["humidity_percent"].is_null());
        assert_eq!(payload["settings"]["automatic_temperature_f"], 105);
        assert_eq!(
            payload["provenance"]["observed_at_unix_ms"],
            serde_json::Value::Null
        );
    }
}
