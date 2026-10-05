use serde::Serialize;
use thiserror::Error;

use crate::{DeviceModeStatus, QuickConnectSettings};

/// Operating modes accepted by QuickConnect mode commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuickConnectCommandMode {
    Off,
    Automatic,
    Timer,
    Manual,
}

/// A QuickConnect settings change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuickConnectCommand {
    SetMode {
        mode: QuickConnectCommandMode,
    },
    ClearMode {
        mode: QuickConnectCommandMode,
    },
    SetAutomaticTargets {
        temperature_f: Option<u16>,
        humidity_percent: Option<u16>,
    },
    SetTimerDuration {
        duration_minutes: u16,
    },
}

/// Validation failures for a typed QuickConnect settings command.
#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum QuickConnectCommandError {
    #[error("automatic target command must change at least one target")]
    NoTargets,
    #[error("current QuickConnect mode is unknown or conflicting")]
    UnknownCurrentMode,
    #[error("current settings are missing a value that must be preserved")]
    MissingPreservedSetting,
    #[error("QuickConnect value is outside the supported range")]
    OutOfRange,
    #[error("timer duration must use a 30-minute step")]
    InvalidTimerStep,
    #[error("the selected mode is already inactive")]
    ModeAlreadyInactive,
}

/// Exact settings payload accepted by the reference service.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum QuickConnectSettingsBody {
    SetMode(SetModeBody),
    SetAutomaticTargets(SetAutomaticTargetsBody),
    SetTimerDuration(SetTimerDurationBody),
}

impl QuickConnectSettingsBody {
    /// Compare a readback with requested values and settings the request preserves.
    pub fn matches_readback(
        &self,
        before: &QuickConnectSettings,
        after: &QuickConnectSettings,
    ) -> bool {
        match self {
            Self::SetMode(body) => {
                after.mode == body.mode()
                    && after.automatic_temperature_f == Some(body.desired_temp)
                    && after.automatic_humidity_percent == Some(body.desired_humidity)
                    && after.timer_duration_minutes == Some(body.timer_value)
                    && after.humidity_monitor == before.humidity_monitor
            }
            Self::SetAutomaticTargets(body) => {
                after.mode == before.mode
                    && after.automatic_temperature_f == Some(body.desired_temp)
                    && after.automatic_humidity_percent == Some(body.desired_humidity)
                    && after.timer_duration_minutes == before.timer_duration_minutes
                    && after.humidity_monitor == before.humidity_monitor
            }
            Self::SetTimerDuration(body) => {
                after.mode == before.mode
                    && after.automatic_temperature_f == before.automatic_temperature_f
                    && after.automatic_humidity_percent == before.automatic_humidity_percent
                    && after.timer_duration_minutes == Some(body.timer_value)
                    && after.humidity_monitor == before.humidity_monitor
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetModeBody {
    automatic_mode: bool,
    desired_temp: u16,
    desired_humidity: u16,
    timer_mode: bool,
    timer_value: u16,
    fan_mode: bool,
}

impl SetModeBody {
    fn mode(&self) -> DeviceModeStatus {
        DeviceModeStatus::from_flags(
            [self.automatic_mode, self.timer_mode, self.fan_mode].map(Some),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetAutomaticTargetsBody {
    automatic_mode: bool,
    desired_temp: u16,
    desired_humidity: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SetTimerDurationBody {
    timer_mode: bool,
    timer_value: u16,
}

/// Build a validated request body without performing network I/O.
pub fn build_settings_body(
    command: &QuickConnectCommand,
    current: &QuickConnectSettings,
) -> Result<QuickConnectSettingsBody, QuickConnectCommandError> {
    match command {
        QuickConnectCommand::ClearMode { mode } => {
            if current_mode(current)? != command_mode_status(*mode) {
                return Err(QuickConnectCommandError::ModeAlreadyInactive);
            }
            mode_body(QuickConnectCommandMode::Off, current)
        }
        QuickConnectCommand::SetMode { mode } => mode_body(*mode, current),
        QuickConnectCommand::SetAutomaticTargets {
            temperature_f,
            humidity_percent,
        } => {
            if temperature_f.is_none() && humidity_percent.is_none() {
                return Err(QuickConnectCommandError::NoTargets);
            }
            let automatic_mode = current_mode(current)? == DeviceModeStatus::Automatic;
            let desired_temp = temperature_f
                .map(validate_temperature)
                .transpose()?
                .map_or_else(|| preserved_temperature(current), Ok)?;
            let desired_humidity = humidity_percent
                .map(validate_humidity)
                .transpose()?
                .map_or_else(|| preserved_humidity(current), Ok)?;
            Ok(QuickConnectSettingsBody::SetAutomaticTargets(
                SetAutomaticTargetsBody {
                    automatic_mode,
                    desired_temp,
                    desired_humidity,
                },
            ))
        }
        QuickConnectCommand::SetTimerDuration { duration_minutes } => {
            let timer_mode = current_mode(current)? == DeviceModeStatus::Timer;
            let timer_value = validate_duration(*duration_minutes)?;
            Ok(QuickConnectSettingsBody::SetTimerDuration(
                SetTimerDurationBody {
                    timer_mode,
                    timer_value,
                },
            ))
        }
    }
}

fn mode_body(
    mode: QuickConnectCommandMode,
    current: &QuickConnectSettings,
) -> Result<QuickConnectSettingsBody, QuickConnectCommandError> {
    current_mode(current)?;
    let (automatic_mode, timer_mode, fan_mode) = mode_flags(mode);
    Ok(QuickConnectSettingsBody::SetMode(SetModeBody {
        automatic_mode,
        desired_temp: preserved_temperature(current)?,
        desired_humidity: preserved_humidity(current)?,
        timer_mode,
        timer_value: preserved_duration(current)?,
        fan_mode,
    }))
}

const fn command_mode_status(mode: QuickConnectCommandMode) -> DeviceModeStatus {
    match mode {
        QuickConnectCommandMode::Off => DeviceModeStatus::Off,
        QuickConnectCommandMode::Automatic => DeviceModeStatus::Automatic,
        QuickConnectCommandMode::Timer => DeviceModeStatus::Timer,
        QuickConnectCommandMode::Manual => DeviceModeStatus::Manual,
    }
}

fn current_mode(
    current: &QuickConnectSettings,
) -> Result<DeviceModeStatus, QuickConnectCommandError> {
    match current.mode {
        mode @ (DeviceModeStatus::Off
        | DeviceModeStatus::Automatic
        | DeviceModeStatus::Timer
        | DeviceModeStatus::Manual) => Ok(mode),
        DeviceModeStatus::Unknown | DeviceModeStatus::Conflicting => {
            Err(QuickConnectCommandError::UnknownCurrentMode)
        }
    }
}

fn mode_flags(mode: QuickConnectCommandMode) -> (bool, bool, bool) {
    match mode {
        QuickConnectCommandMode::Off => (false, false, false),
        QuickConnectCommandMode::Automatic => (true, false, false),
        QuickConnectCommandMode::Timer => (false, true, false),
        QuickConnectCommandMode::Manual => (false, false, true),
    }
}

fn preserved_temperature(current: &QuickConnectSettings) -> Result<u16, QuickConnectCommandError> {
    current
        .automatic_temperature_f
        .ok_or(QuickConnectCommandError::MissingPreservedSetting)
        .and_then(validate_temperature)
}

fn preserved_humidity(current: &QuickConnectSettings) -> Result<u16, QuickConnectCommandError> {
    current
        .automatic_humidity_percent
        .ok_or(QuickConnectCommandError::MissingPreservedSetting)
        .and_then(validate_humidity)
}

fn preserved_duration(current: &QuickConnectSettings) -> Result<u16, QuickConnectCommandError> {
    current
        .timer_duration_minutes
        .ok_or(QuickConnectCommandError::MissingPreservedSetting)
        .and_then(validate_duration)
}

fn validate_temperature(value: u16) -> Result<u16, QuickConnectCommandError> {
    (90..=120)
        .contains(&value)
        .then_some(value)
        .ok_or(QuickConnectCommandError::OutOfRange)
}

fn validate_humidity(value: u16) -> Result<u16, QuickConnectCommandError> {
    (30..=80)
        .contains(&value)
        .then_some(value)
        .ok_or(QuickConnectCommandError::OutOfRange)
}

fn validate_duration(value: u16) -> Result<u16, QuickConnectCommandError> {
    if !(30..=360).contains(&value) {
        return Err(QuickConnectCommandError::OutOfRange);
    }
    value
        .is_multiple_of(30)
        .then_some(value)
        .ok_or(QuickConnectCommandError::InvalidTimerStep)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::{
        DeviceModeStatus, QuickConnectCommand, QuickConnectCommandMode, QuickConnectSettings,
        build_settings_body,
    };

    fn current_settings() -> QuickConnectSettings {
        QuickConnectSettings {
            mode: DeviceModeStatus::Automatic,
            automatic_temperature_f: Some(105),
            automatic_humidity_percent: Some(42),
            timer_duration_minutes: Some(60),
            humidity_monitor: Some(true),
        }
    }

    #[test]
    fn conditional_off_uses_current_mode_and_preserves_settings() {
        let command = QuickConnectCommand::ClearMode {
            mode: QuickConnectCommandMode::Automatic,
        };
        let body = build_settings_body(&command, &current_settings()).unwrap();
        let body = serde_json::to_value(body).unwrap();
        assert_eq!(
            body,
            json!({"automaticMode":false,"timerMode":false,"fanMode":false,"desiredTemp":105,"desiredHumidity":42,"timerValue":60})
        );
        let mut current = current_settings();
        current.mode = DeviceModeStatus::Timer;
        assert_eq!(
            build_settings_body(&command, &current).unwrap_err(),
            super::QuickConnectCommandError::ModeAlreadyInactive
        );
        current.mode = DeviceModeStatus::Unknown;
        assert!(build_settings_body(&command, &current).is_err());
    }

    #[test]
    fn temperature_change_has_exact_integer_fields_and_preserves_humidity() {
        let command = QuickConnectCommand::SetAutomaticTargets {
            temperature_f: Some(111),
            humidity_percent: None,
        };

        let body = build_settings_body(&command, &current_settings()).unwrap();

        assert_eq!(
            serde_json::to_value(body).unwrap(),
            json!({
                "automaticMode": true,
                "desiredTemp": 111,
                "desiredHumidity": 42
            })
        );
    }

    #[test]
    fn mode_change_has_six_exact_fields_and_preserves_current_settings() {
        let command = QuickConnectCommand::SetMode {
            mode: QuickConnectCommandMode::Manual,
        };

        let body = build_settings_body(&command, &current_settings()).unwrap();

        assert_eq!(
            serde_json::to_value(body).unwrap(),
            json!({
                "automaticMode": false,
                "desiredTemp": 105,
                "desiredHumidity": 42,
                "timerMode": false,
                "timerValue": 60,
                "fanMode": true
            })
        );
    }

    #[test]
    fn every_mode_uses_one_exclusive_flag_tuple() {
        let modes = [
            (QuickConnectCommandMode::Off, [false, false, false]),
            (QuickConnectCommandMode::Automatic, [true, false, false]),
            (QuickConnectCommandMode::Timer, [false, true, false]),
            (QuickConnectCommandMode::Manual, [false, false, true]),
        ];
        let bodies = modes.map(|(mode, _)| {
            build_settings_body(&QuickConnectCommand::SetMode { mode }, &current_settings())
                .unwrap()
        });
        let flags = bodies.map(|body| {
            let body = serde_json::to_value(body).unwrap();
            [
                body["automaticMode"].as_bool().unwrap(),
                body["timerMode"].as_bool().unwrap(),
                body["fanMode"].as_bool().unwrap(),
            ]
        });
        let expected = modes.map(|(_, flags)| flags);

        assert_eq!(flags, expected);
    }

    #[test]
    fn timer_duration_preserves_mode_and_validates_boundaries() {
        use crate::QuickConnectCommandError::{InvalidTimerStep, OutOfRange};
        for (mode, duration_minutes, expected) in [
            (
                DeviceModeStatus::Automatic,
                90,
                Ok(json!({"timerMode":false,"timerValue":90})),
            ),
            (
                DeviceModeStatus::Timer,
                360,
                Ok(json!({"timerMode":true,"timerValue":360})),
            ),
            (
                DeviceModeStatus::Timer,
                30,
                Ok(json!({"timerMode":true,"timerValue":30})),
            ),
            (DeviceModeStatus::Timer, 31, Err(InvalidTimerStep)),
            (DeviceModeStatus::Timer, 361, Err(OutOfRange)),
        ] {
            let current = QuickConnectSettings {
                mode,
                ..current_settings()
            };
            let actual = build_settings_body(
                &QuickConnectCommand::SetTimerDuration { duration_minutes },
                &current,
            )
            .map(|body| serde_json::to_value(body).unwrap());
            assert_eq!(actual, expected, "{mode:?} duration={duration_minutes}");
        }
    }

    #[test]
    fn missing_preserved_values_and_unusable_modes_reject_commands() {
        let missing_humidity = QuickConnectSettings {
            automatic_humidity_percent: None,
            ..current_settings()
        };
        assert_eq!(
            build_settings_body(
                &QuickConnectCommand::SetAutomaticTargets {
                    temperature_f: Some(110),
                    humidity_percent: None,
                },
                &missing_humidity,
            ),
            Err(crate::QuickConnectCommandError::MissingPreservedSetting)
        );
        let unknown_mode = QuickConnectSettings {
            mode: DeviceModeStatus::Conflicting,
            ..current_settings()
        };
        assert_eq!(
            build_settings_body(
                &QuickConnectCommand::SetTimerDuration {
                    duration_minutes: 90
                },
                &unknown_mode,
            ),
            Err(crate::QuickConnectCommandError::UnknownCurrentMode)
        );
        assert_eq!(
            build_settings_body(
                &QuickConnectCommand::SetAutomaticTargets {
                    temperature_f: None,
                    humidity_percent: None,
                },
                &current_settings(),
            ),
            Err(crate::QuickConnectCommandError::NoTargets)
        );

        let unusable_modes = [DeviceModeStatus::Unknown, DeviceModeStatus::Conflicting];
        let results = unusable_modes.map(|mode| {
            let current = QuickConnectSettings {
                mode,
                ..current_settings()
            };
            build_settings_body(
                &QuickConnectCommand::SetMode {
                    mode: QuickConnectCommandMode::Off,
                },
                &current,
            )
        });
        assert_eq!(
            results,
            [
                Err(crate::QuickConnectCommandError::UnknownCurrentMode),
                Err(crate::QuickConnectCommandError::UnknownCurrentMode),
            ]
        );
    }

    #[test]
    fn target_ranges_accept_boundaries_and_reject_out_of_range_values() {
        use crate::QuickConnectCommandError::OutOfRange;
        for (temperature_f, humidity_percent, expected) in [
            (
                Some(90),
                None,
                Ok(json!({"automaticMode":true,"desiredTemp":90,"desiredHumidity":42})),
            ),
            (
                Some(120),
                None,
                Ok(json!({"automaticMode":true,"desiredTemp":120,"desiredHumidity":42})),
            ),
            (
                None,
                Some(30),
                Ok(json!({"automaticMode":true,"desiredTemp":105,"desiredHumidity":30})),
            ),
            (
                None,
                Some(80),
                Ok(json!({"automaticMode":true,"desiredTemp":105,"desiredHumidity":80})),
            ),
            (Some(89), None, Err(OutOfRange)),
            (None, Some(81), Err(OutOfRange)),
        ] {
            let actual = build_settings_body(
                &QuickConnectCommand::SetAutomaticTargets {
                    temperature_f,
                    humidity_percent,
                },
                &current_settings(),
            )
            .map(|body| serde_json::to_value(body).unwrap());
            assert_eq!(
                actual, expected,
                "temperature={temperature_f:?} humidity={humidity_percent:?}"
            );
        }
    }
}
