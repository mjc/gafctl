use std::{sync::Arc, time::Duration};

use serde::{Deserialize, Deserializer, Serialize, de};
use updraft_protocol::{
    AutomaticThresholds, ControlCommand, HumidityTenthsPercent, Minutes, OperatingMode,
    TemperatureTenthsF, TimerState,
};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CommandId(Arc<str>);

impl CommandId {
    #[cfg(test)]
    pub(crate) fn parse(value: impl Into<Arc<str>>) -> Option<Self> {
        let value = value.into();
        Self::is_valid(&value).then_some(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
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
                CommandId::from_str(value)
                    .ok_or_else(|| E::custom("invalid MQTT control request ID"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                CommandId::from_str(value)
                    .ok_or_else(|| E::custom("invalid MQTT control request ID"))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                CommandId::from_string(value)
                    .ok_or_else(|| E::custom("invalid MQTT control request ID"))
            }
        }

        deserializer.deserialize_str(CommandIdVisitor)
    }
}

const MAX_COMMAND_AGE: Duration = Duration::from_secs(30);
const MAX_COMMAND_CLOCK_SKEW: Duration = Duration::from_secs(5);

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ControlRequest {
    request_id: CommandId,
    preset: ControlPreset,
    issued_at_unix_ms: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct FreshControlRequest(ControlRequest);

impl ControlRequest {
    pub(crate) fn new(
        request_id: CommandId,
        preset: ControlPreset,
        issued_at_unix_ms: u64,
    ) -> Self {
        Self {
            request_id,
            preset,
            issued_at_unix_ms,
        }
    }

    pub(crate) fn request_id(&self) -> &CommandId {
        &self.request_id
    }

    pub(crate) const fn preset(&self) -> ControlPreset {
        self.preset
    }

    pub(crate) fn validate_fresh_at(self, now_unix_ms: u64) -> Result<FreshControlRequest, Self> {
        if self.is_fresh_at(now_unix_ms) {
            Ok(FreshControlRequest(self))
        } else {
            Err(self)
        }
    }

    fn is_fresh_at(&self, now_unix_ms: u64) -> bool {
        let max_future_ms = MAX_COMMAND_CLOCK_SKEW.as_millis() as u64;
        let max_age_ms = MAX_COMMAND_AGE.as_millis() as u64;
        self.issued_at_unix_ms <= now_unix_ms.saturating_add(max_future_ms)
            && now_unix_ms.saturating_sub(self.issued_at_unix_ms) <= max_age_ms
    }
}

impl FreshControlRequest {
    pub(crate) fn request_id(&self) -> &CommandId {
        &self.0.request_id
    }

    pub(crate) const fn preset(&self) -> ControlPreset {
        self.0.preset
    }

    pub(crate) fn is_fresh_at(&self, now_unix_ms: u64) -> bool {
        self.0.is_fresh_at(now_unix_ms)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ControlPreset {
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
    pub(crate) fn command(self) -> ControlCommand {
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

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Automatic105F30Percent => "automatic105_f30_percent",
            Self::Automatic105_1F30_1Percent => "automatic105_1_f30_1_percent",
            Self::TimerClear => "timer_clear",
            Self::TimerOneMinute => "timer_one_minute",
        }
    }

    pub(crate) fn from_readback(
        mode: OperatingMode,
        thresholds: AutomaticThresholds,
        timer: TimerState,
    ) -> Option<Self> {
        match mode {
            OperatingMode::Automatic
                if thresholds.temperature.value() == 1050 && thresholds.humidity.value() == 300 =>
            {
                Some(Self::Automatic105F30Percent)
            }
            OperatingMode::Automatic
                if thresholds.temperature.value() == 1051 && thresholds.humidity.value() == 301 =>
            {
                Some(Self::Automatic105_1F30_1Percent)
            }
            OperatingMode::Timer if timer.remaining.value() == 0 && timer.original.value() == 0 => {
                Some(Self::TimerClear)
            }
            OperatingMode::Timer if timer.original.value() == 1 => Some(Self::TimerOneMinute),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_control_request_must_be_fresh_before_it_can_be_executed() {
        let request = ControlRequest {
            request_id: CommandId::parse("ha-123".to_owned()).unwrap(),
            preset: ControlPreset::TimerClear,
            issued_at_unix_ms: 1_000_000,
        };

        let accepted = request.validate_fresh_at(1_000_000).unwrap();
        assert_eq!(accepted.preset(), ControlPreset::TimerClear);
        assert_eq!(accepted.request_id().as_str(), "ha-123");
        assert!(accepted.is_fresh_at(1_030_000));
        assert!(!accepted.is_fresh_at(1_030_001));

        let latest = ControlRequest {
            request_id: CommandId::parse("ha-latest".to_owned()).unwrap(),
            preset: ControlPreset::TimerClear,
            issued_at_unix_ms: 1_000_000,
        };
        assert!(latest.validate_fresh_at(1_030_000).is_ok());

        let clock_skew = ControlRequest {
            request_id: CommandId::parse("ha-clock-skew".to_owned()).unwrap(),
            preset: ControlPreset::TimerClear,
            issued_at_unix_ms: 1_005_000,
        };
        assert!(clock_skew.validate_fresh_at(1_000_000).is_ok());

        let stale = ControlRequest {
            request_id: CommandId::parse("ha-old".to_owned()).unwrap(),
            preset: ControlPreset::TimerClear,
            issued_at_unix_ms: 1_000_000,
        };
        assert!(stale.validate_fresh_at(1_031_000).is_err());

        let future = ControlRequest {
            request_id: CommandId::parse("ha-future".to_owned()).unwrap(),
            preset: ControlPreset::TimerClear,
            issued_at_unix_ms: 1_005_001,
        };
        assert!(future.validate_fresh_at(1_000_000).is_err());
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
}
