use serde::{Deserialize, Serialize};

use crate::control::ControlPreset;
use crate::{AutomaticHumidityPercent, AutomaticTemperatureF, LegacyTimerMinutes};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(try_from = "uuid::Uuid", into = "uuid::Uuid")]
pub struct ProxyId(uuid::Uuid);

impl Default for ProxyId {
    fn default() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl TryFrom<uuid::Uuid> for ProxyId {
    type Error = &'static str;

    fn try_from(value: uuid::Uuid) -> Result<Self, Self::Error> {
        (value.get_version() == Some(uuid::Version::Random)
            && value.get_variant() == uuid::Variant::RFC4122)
            .then_some(Self(value))
            .ok_or("invalid proxy identity")
    }
}

impl From<ProxyId> for uuid::Uuid {
    fn from(value: ProxyId) -> Self {
        value.0
    }
}

impl std::fmt::Display for ProxyId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EntitySources {
    pub state_source: EntitySource,
    pub command_source: EntitySource,
}

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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeviceCommand {
    LegacyMode {
        mode: LegacyControlMode,
    },
    LegacyPreset {
        preset: ControlPreset,
    },
    LegacyAutomaticTemperature {
        temperature_f: AutomaticTemperatureF,
    },
    LegacyAutomaticHumidity {
        humidity_percent: AutomaticHumidityPercent,
    },
    LegacyTimerDuration {
        minutes: LegacyTimerMinutes,
    },
    LegacyTimer {
        minutes: LegacyTimerMinutes,
    },
    QuickConnectMode {
        mode: QuickConnectMode,
    },
    QuickConnectConditionalOff {
        only_if_current: QuickConnectMode,
    },
    QuickConnectTargets {
        temperature_f: u16,
        humidity_percent: u16,
    },
    QuickConnectAutomaticTemperature {
        temperature_f: AutomaticTemperatureF,
    },
    QuickConnectAutomaticHumidity {
        humidity_percent: AutomaticHumidityPercent,
    },
    QuickConnectTimerDuration {
        minutes: u16,
    },
}

impl DeviceCommand {
    pub const fn required_capability(self) -> CommandCapability {
        match self {
            DeviceCommand::LegacyMode { .. } => CommandCapability::LegacyMode,
            DeviceCommand::LegacyPreset { preset } => CommandCapability::LegacyPreset(preset),
            DeviceCommand::LegacyAutomaticTemperature { .. } => {
                CommandCapability::LegacyAutomaticTemperature
            }
            DeviceCommand::LegacyAutomaticHumidity { .. } => {
                CommandCapability::LegacyAutomaticHumidity
            }
            DeviceCommand::LegacyTimerDuration { .. } => CommandCapability::LegacyTimerDuration,
            DeviceCommand::LegacyTimer { .. } => CommandCapability::LegacyTimer,
            DeviceCommand::QuickConnectMode { .. }
            | DeviceCommand::QuickConnectConditionalOff { .. } => {
                CommandCapability::QuickConnectMode
            }
            DeviceCommand::QuickConnectTargets { .. }
            | DeviceCommand::QuickConnectAutomaticTemperature { .. }
            | DeviceCommand::QuickConnectAutomaticHumidity { .. } => {
                CommandCapability::QuickConnectTargets
            }
            DeviceCommand::QuickConnectTimerDuration { .. } => {
                CommandCapability::QuickConnectTimerDuration
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyControlMode {
    Automatic,
    Timer,
    Off,
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum CommandCapability {
    LegacyMode,
    LegacyPreset(ControlPreset),
    LegacyAutomaticTemperature,
    LegacyAutomaticHumidity,
    LegacyTimer,
    LegacyTimerDuration,
    QuickConnectMode,
    QuickConnectTargets,
    QuickConnectTimerDuration,
}

impl CommandCapability {
    pub const fn backend(self) -> DeviceBackend {
        match self {
            Self::LegacyMode
            | Self::LegacyPreset(_)
            | Self::LegacyAutomaticTemperature
            | Self::LegacyAutomaticHumidity
            | Self::LegacyTimer
            | Self::LegacyTimerDuration => DeviceBackend::LegacyBle,
            Self::QuickConnectMode
            | Self::QuickConnectTargets
            | Self::QuickConnectTimerDuration => DeviceBackend::QuickConnect,
        }
    }
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
            .chain([
                CommandCapability::LegacyMode,
                CommandCapability::LegacyAutomaticTemperature,
                CommandCapability::LegacyAutomaticHumidity,
                CommandCapability::LegacyTimer,
                CommandCapability::LegacyTimerDuration,
            ])
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
        self.commands.contains(&command.required_capability())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceDescriptor {
    pub proxy_id: ProxyId,
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
            proxy_id: ProxyId::default(),
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
        #[serde(deserialize_with = "crate::response::required_option")]
        mode: Option<LegacyMode>,
        #[serde(deserialize_with = "crate::response::required_option")]
        controller_fan_on: Option<bool>,
        #[serde(deserialize_with = "crate::response::required_option")]
        automatic_temperature_tenths_f: Option<u16>,
        #[serde(deserialize_with = "crate::response::required_option")]
        automatic_humidity_tenths_percent: Option<u16>,
        #[serde(deserialize_with = "crate::response::required_option")]
        timer_remaining_minutes: Option<u16>,
        #[serde(deserialize_with = "crate::response::required_option")]
        timer_original_minutes: Option<u16>,
    },
    QuickConnect {
        mode: QuickConnectModeStatus,
        #[serde(deserialize_with = "crate::response::required_option")]
        automatic_temperature_f: Option<u16>,
        #[serde(deserialize_with = "crate::response::required_option")]
        automatic_humidity_percent: Option<u16>,
        #[serde(deserialize_with = "crate::response::required_option")]
        timer_duration_minutes: Option<u16>,
        #[serde(deserialize_with = "crate::response::required_option")]
        humidity_monitor: Option<bool>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StateProvenance {
    pub backend: DeviceBackend,
    #[serde(deserialize_with = "crate::response::required_option")]
    pub fetched_at_unix_ms: Option<u64>,
    #[serde(deserialize_with = "crate::response::required_option")]
    pub observed_at_unix_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeviceState {
    #[serde(deserialize_with = "crate::response::required_option")]
    pub temperature_f: Option<f64>,
    #[serde(deserialize_with = "crate::response::required_option")]
    pub humidity_percent: Option<f64>,
    pub settings: DeviceSettings,
    #[serde(deserialize_with = "crate::response::required_option")]
    pub estimated_running: Option<bool>,
    #[serde(deserialize_with = "crate::response::required_option")]
    pub diagnostics: Option<DeviceDiagnostics>,
    pub provenance: StateProvenance,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceDiagnostics {
    #[serde(deserialize_with = "crate::response::required_option")]
    pub firmware_version: Option<String>,
    #[serde(deserialize_with = "crate::response::required_option")]
    pub signal_strength_raw: Option<String>,
    #[serde(deserialize_with = "crate::response::required_option")]
    pub verified_raw: Option<String>,
    #[serde(deserialize_with = "crate::response::required_option")]
    pub ota_in_progress: Option<bool>,
}

impl std::str::FromStr for DeviceId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value.to_owned())
    }
}

impl std::fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_capabilities_cover_every_command_family() {
        let read_only = DeviceCapabilities::quickconnect_read_only();
        assert!(read_only.read_state);
        assert!(read_only.commands.is_empty());
        let temperature = AutomaticTemperatureF::try_from(105).unwrap();
        let humidity = AutomaticHumidityPercent::try_from(40).unwrap();
        let cases = [
            (
                DeviceCommand::LegacyPreset {
                    preset: ControlPreset::TimerClear,
                },
                CommandCapability::LegacyPreset(ControlPreset::TimerClear),
                DeviceBackend::LegacyBle,
            ),
            (
                DeviceCommand::LegacyAutomaticTemperature {
                    temperature_f: temperature,
                },
                CommandCapability::LegacyAutomaticTemperature,
                DeviceBackend::LegacyBle,
            ),
            (
                DeviceCommand::LegacyAutomaticHumidity {
                    humidity_percent: humidity,
                },
                CommandCapability::LegacyAutomaticHumidity,
                DeviceBackend::LegacyBle,
            ),
            (
                DeviceCommand::LegacyTimer {
                    minutes: LegacyTimerMinutes::try_from(1).unwrap(),
                },
                CommandCapability::LegacyTimer,
                DeviceBackend::LegacyBle,
            ),
            (
                DeviceCommand::QuickConnectMode {
                    mode: QuickConnectMode::Automatic,
                },
                CommandCapability::QuickConnectMode,
                DeviceBackend::QuickConnect,
            ),
            (
                DeviceCommand::QuickConnectConditionalOff {
                    only_if_current: QuickConnectMode::Automatic,
                },
                CommandCapability::QuickConnectMode,
                DeviceBackend::QuickConnect,
            ),
            (
                DeviceCommand::QuickConnectTargets {
                    temperature_f: 105,
                    humidity_percent: 40,
                },
                CommandCapability::QuickConnectTargets,
                DeviceBackend::QuickConnect,
            ),
            (
                DeviceCommand::QuickConnectAutomaticTemperature {
                    temperature_f: temperature,
                },
                CommandCapability::QuickConnectTargets,
                DeviceBackend::QuickConnect,
            ),
            (
                DeviceCommand::QuickConnectAutomaticHumidity {
                    humidity_percent: humidity,
                },
                CommandCapability::QuickConnectTargets,
                DeviceBackend::QuickConnect,
            ),
            (
                DeviceCommand::QuickConnectTimerDuration { minutes: 1 },
                CommandCapability::QuickConnectTimerDuration,
                DeviceBackend::QuickConnect,
            ),
        ];
        cases
            .into_iter()
            .for_each(|(command, capability, backend)| {
                assert_eq!(command.required_capability(), capability);
                assert_eq!(capability.backend(), backend);
                let specific = DeviceCapabilities {
                    read_state: true,
                    commands: vec![capability],
                };
                assert!(specific.supports(command), "{command:?}");
                assert_eq!(
                    DeviceCapabilities::legacy_ble().supports(command),
                    backend == DeviceBackend::LegacyBle
                );
                assert_eq!(
                    DeviceCapabilities::quickconnect_with_controls().supports(command),
                    backend == DeviceBackend::QuickConnect
                );
                assert!(!DeviceCapabilities::quickconnect_read_only().supports(command));
            });
    }

    #[test]
    fn legacy_presets_require_the_matching_preset_capability() {
        let presets = [
            ControlPreset::Automatic105F30Percent,
            ControlPreset::Automatic105_1F30_1Percent,
            ControlPreset::TimerClear,
            ControlPreset::TimerOneMinute,
        ];
        presets.into_iter().for_each(|enabled| {
            let capabilities = DeviceCapabilities {
                read_state: true,
                commands: vec![CommandCapability::LegacyPreset(enabled)],
            };
            presets.into_iter().for_each(|requested| {
                assert_eq!(
                    capabilities.supports(DeviceCommand::LegacyPreset { preset: requested }),
                    enabled == requested
                );
            });
        });
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
