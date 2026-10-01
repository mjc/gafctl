use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Deserializer, Serialize, de};
use updraft_protocol::{
    AutomaticThresholds, ControlCommand, HumidityTenthsPercent, Minutes, OperatingMode,
    TemperatureTenthsF, TimerState,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CommandId(Arc<str>);

impl CommandId {
    pub fn parse(value: &str) -> Option<Self> {
        Self::from_str(value)
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

    fn from_str(value: &str) -> Option<Self> {
        Self::is_valid(value).then(|| Self(Arc::from(value)))
    }

    fn from_string(value: String) -> Option<Self> {
        Self::is_valid(&value).then(|| Self(Arc::from(value)))
    }
}

impl<'de> Deserialize<'de> for CommandId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CommandIdVisitor;

        impl<'de> de::Visitor<'de> for CommandIdVisitor {
            type Value = CommandId;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a 1-64 character ASCII request ID")
            }

            fn visit_borrowed_str<E>(self, value: &'de str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                self.visit_str(value)
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                CommandId::from_str(value).ok_or_else(|| E::custom("invalid control request ID"))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                CommandId::from_string(value).ok_or_else(|| E::custom("invalid control request ID"))
            }
        }

        deserializer.deserialize_str(CommandIdVisitor)
    }
}

pub fn unix_millis(timestamp: SystemTime) -> Option<u64> {
    timestamp
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| elapsed.as_millis().try_into().ok())
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ControlRequest {
    request_id: CommandId,
    preset: ControlPreset,
    issued_at_unix_ms: u64,
}

impl ControlRequest {
    pub fn request_id(&self) -> &CommandId {
        &self.request_id
    }

    pub const fn preset(&self) -> ControlPreset {
        self.preset
    }

    pub const fn issued_at_unix_ms(&self) -> u64 {
        self.issued_at_unix_ms
    }
}

pub fn is_fresh_at(
    issued_at_unix_ms: u64,
    now_unix_ms: u64,
    max_age: Duration,
    max_future_skew: Duration,
) -> bool {
    let max_age_ms = u64::try_from(max_age.as_millis()).unwrap_or(u64::MAX);
    let max_future_skew_ms = u64::try_from(max_future_skew.as_millis()).unwrap_or(u64::MAX);
    match issued_at_unix_ms.checked_sub(now_unix_ms) {
        Some(future_ms) => future_ms <= max_future_skew_ms,
        None => now_unix_ms - issued_at_unix_ms <= max_age_ms,
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlPreset {
    #[serde(rename = "automatic105_f30_percent")]
    Automatic105F30Percent,
    #[serde(rename = "automatic105_1_f30_1_percent")]
    Automatic105_1F30_1Percent,
    #[serde(rename = "timer_clear")]
    TimerClear,
    #[serde(rename = "timer_one_minute")]
    TimerOneMinute,
}

impl ControlPreset {
    pub fn command(self) -> ControlCommand {
        match self {
            Self::Automatic105F30Percent => {
                ControlCommand::SetAutomaticThresholds(AutomaticThresholds {
                    temperature: TemperatureTenthsF::new(1050),
                    humidity: HumidityTenthsPercent::new(300),
                })
            }
            Self::Automatic105_1F30_1Percent => {
                ControlCommand::SetAutomaticThresholds(AutomaticThresholds {
                    temperature: TemperatureTenthsF::new(1051),
                    humidity: HumidityTenthsPercent::new(301),
                })
            }
            Self::TimerClear => ControlCommand::SetTimer(Minutes::new(0)),
            Self::TimerOneMinute => ControlCommand::SetTimer(Minutes::new(1)),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Automatic105F30Percent => "automatic105_f30_percent",
            Self::Automatic105_1F30_1Percent => "automatic105_1_f30_1_percent",
            Self::TimerClear => "timer_clear",
            Self::TimerOneMinute => "timer_one_minute",
        }
    }

    pub fn from_readback(
        mode: OperatingMode,
        thresholds: AutomaticThresholds,
        timer: TimerState,
    ) -> Option<Self> {
        match mode {
            OperatingMode::Automatic => {
                match (thresholds.temperature.value(), thresholds.humidity.value()) {
                    (1050, 300) => Some(Self::Automatic105F30Percent),
                    (1051, 301) => Some(Self::Automatic105_1F30_1Percent),
                    _ => None,
                }
            }
            OperatingMode::Timer => match (timer.remaining.value(), timer.original.value()) {
                (0, 0) => Some(Self::TimerClear),
                (1, 1) => Some(Self::TimerOneMinute),
                _ => None,
            },
            OperatingMode::Ota => None,
        }
    }
}

impl Serialize for CommandId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl std::str::FromStr for CommandId {
    type Err = &'static str;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or("invalid control request ID")
    }
}

impl std::fmt::Display for CommandId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_clock_freshness_observes_age_and_future_skew_bounds() {
        let max_age = Duration::from_secs(30);
        let future_skew = Duration::from_secs(5);
        assert!(is_fresh_at(1_000_000, 1_030_000, max_age, future_skew));
        assert!(!is_fresh_at(1_000_000, 1_030_001, max_age, future_skew));
        assert!(is_fresh_at(1_005_000, 1_000_000, max_age, future_skew));
        assert!(!is_fresh_at(1_005_001, 1_000_000, max_age, future_skew));
    }

    #[test]
    fn only_exact_supported_settings_map_to_a_selectable_readback_preset() {
        let thresholds = |temperature, humidity| AutomaticThresholds {
            temperature: TemperatureTenthsF::new(temperature),
            humidity: HumidityTenthsPercent::new(humidity),
        };
        let timer = |remaining, original| TimerState {
            remaining: Minutes::new(remaining),
            original: Minutes::new(original),
        };

        assert_eq!(
            ControlPreset::from_readback(
                OperatingMode::Automatic,
                thresholds(1050, 300),
                timer(0, 0)
            ),
            Some(ControlPreset::Automatic105F30Percent)
        );
        assert_eq!(
            ControlPreset::from_readback(
                OperatingMode::Automatic,
                thresholds(1051, 301),
                timer(0, 0)
            ),
            Some(ControlPreset::Automatic105_1F30_1Percent)
        );
        assert_eq!(
            ControlPreset::from_readback(OperatingMode::Timer, thresholds(1050, 300), timer(0, 0)),
            Some(ControlPreset::TimerClear)
        );
        assert_eq!(
            ControlPreset::from_readback(OperatingMode::Timer, thresholds(1050, 300), timer(1, 1)),
            Some(ControlPreset::TimerOneMinute)
        );
        assert_eq!(
            ControlPreset::from_readback(
                OperatingMode::Automatic,
                thresholds(1052, 301),
                timer(0, 0)
            ),
            None
        );
    }

    #[test]
    fn timer_preset_rejects_expired_and_inconsistent_readbacks() {
        let thresholds = AutomaticThresholds {
            temperature: TemperatureTenthsF::new(1050),
            humidity: HumidityTenthsPercent::new(300),
        };
        [(0, 1), (2, 1), (1, 2), (0, 2)]
            .into_iter()
            .for_each(|(remaining, original)| {
                let timer = TimerState {
                    remaining: Minutes::new(remaining),
                    original: Minutes::new(original),
                };
                assert_eq!(
                    ControlPreset::from_readback(OperatingMode::Timer, thresholds, timer),
                    None,
                    "unsupported timer readback: remaining={remaining}, original={original}",
                );
            });
    }
}
