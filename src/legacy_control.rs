use crate::model::{DeviceCommand, LegacyControlMode};
use crate::protocol::{
    AutomaticThresholds, ControlCommand, HumidityTenthsPercent, Minutes, TemperatureTenthsF,
};

pub(crate) fn prepare_control(
    command: DeviceCommand,
    thresholds: Option<AutomaticThresholds>,
    timer: Option<Minutes>,
) -> Option<ControlCommand> {
    match command {
        DeviceCommand::LegacyMode { mode } => match mode {
            LegacyControlMode::Automatic => automatic_command(thresholds?),
            LegacyControlMode::Timer => {
                let minutes = timer?.value();
                if minutes == 0 {
                    automatic_command(thresholds?)
                } else {
                    (minutes <= 360).then_some(ControlCommand::SetTimer(Minutes::new(minutes)))
                }
            }
            LegacyControlMode::Off => Some(ControlCommand::SetTimer(Minutes::new(0))),
        },
        DeviceCommand::LegacyPreset { preset } => Some(preset.command()),
        DeviceCommand::LegacyTimer { minutes } => {
            Some(ControlCommand::SetTimer(Minutes::new(minutes.value())))
        }
        DeviceCommand::LegacyAutomaticTemperature { temperature_f } => {
            let current = thresholds?;
            let humidity = current.humidity.value();
            ((300..=800).contains(&humidity) || humidity == 1000).then_some(
                ControlCommand::SetAutomaticThresholds(AutomaticThresholds {
                    temperature: TemperatureTenthsF::new(temperature_f.value() * 10),
                    humidity: current.humidity,
                }),
            )
        }
        DeviceCommand::LegacyAutomaticHumidity { humidity_percent } => {
            let current = thresholds?;
            (900..=1200)
                .contains(&current.temperature.value())
                .then_some(ControlCommand::SetAutomaticThresholds(
                    AutomaticThresholds {
                        temperature: current.temperature,
                        humidity: HumidityTenthsPercent::new(humidity_percent.value() * 10),
                    },
                ))
        }
        DeviceCommand::LegacyTimerDuration { .. }
        | DeviceCommand::QuickConnectMode { .. }
        | DeviceCommand::QuickConnectConditionalOff { .. }
        | DeviceCommand::QuickConnectTargets { .. }
        | DeviceCommand::QuickConnectAutomaticTemperature { .. }
        | DeviceCommand::QuickConnectAutomaticHumidity { .. }
        | DeviceCommand::QuickConnectTimerDuration { .. } => None,
    }
}

fn automatic_command(current: AutomaticThresholds) -> Option<ControlCommand> {
    let humidity = current.humidity.value();
    ((900..=1200).contains(&current.temperature.value())
        && ((300..=800).contains(&humidity) || humidity == 1000))
        .then_some(ControlCommand::SetAutomaticThresholds(current))
}

pub(crate) fn needs_state_read(command: DeviceCommand) -> bool {
    match command {
        DeviceCommand::LegacyMode {
            mode: LegacyControlMode::Automatic | LegacyControlMode::Timer,
        }
        | DeviceCommand::LegacyAutomaticTemperature { .. }
        | DeviceCommand::LegacyAutomaticHumidity { .. } => true,
        DeviceCommand::LegacyMode {
            mode: LegacyControlMode::Off,
        }
        | DeviceCommand::LegacyTimerDuration { .. }
        | DeviceCommand::QuickConnectMode { .. }
        | DeviceCommand::QuickConnectConditionalOff { .. }
        | DeviceCommand::QuickConnectTargets { .. }
        | DeviceCommand::QuickConnectAutomaticTemperature { .. }
        | DeviceCommand::QuickConnectAutomaticHumidity { .. }
        | DeviceCommand::QuickConnectTimerDuration { .. } => false,
        DeviceCommand::LegacyTimer { minutes } => minutes.value() > 0,
        DeviceCommand::LegacyPreset { preset } => match preset.command() {
            ControlCommand::SetTimer(minutes) => minutes.value() > 0,
            ControlCommand::SetAutomaticThresholds(_) => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{HumidityTenthsPercent, TemperatureTenthsF};

    fn thresholds(temperature: u16, humidity: u16) -> AutomaticThresholds {
        AutomaticThresholds {
            temperature: TemperatureTenthsF::new(temperature),
            humidity: HumidityTenthsPercent::new(humidity),
        }
    }

    #[test]
    fn mode_changes_preserve_thresholds_and_use_bounded_timer_duration() {
        let automatic = DeviceCommand::LegacyMode {
            mode: LegacyControlMode::Automatic,
        };
        let timer = DeviceCommand::LegacyMode {
            mode: LegacyControlMode::Timer,
        };
        let off = DeviceCommand::LegacyMode {
            mode: LegacyControlMode::Off,
        };
        assert!(needs_state_read(automatic));
        assert!(needs_state_read(timer));
        assert!(!needs_state_read(off));
        for current in [thresholds(1051, 301), thresholds(1050, 1000)] {
            assert_eq!(
                prepare_control(automatic, Some(current), None),
                Some(ControlCommand::SetAutomaticThresholds(current))
            );
        }
        for current in [
            None,
            Some(thresholds(0, 301)),
            Some(thresholds(1050, 0)),
            Some(thresholds(1201, 300)),
            Some(thresholds(1050, 801)),
        ] {
            assert_eq!(prepare_control(automatic, current, None), None);
        }
        for minutes in [1, 60, 360] {
            assert_eq!(
                prepare_control(timer, None, Some(Minutes::new(minutes))),
                Some(ControlCommand::SetTimer(Minutes::new(minutes)))
            );
        }
        assert_eq!(prepare_control(timer, None, None), None);
        assert_eq!(
            prepare_control(timer, Some(thresholds(1051, 301)), Some(Minutes::new(0))),
            Some(ControlCommand::SetAutomaticThresholds(thresholds(
                1051, 301
            )))
        );
        assert_eq!(prepare_control(timer, None, Some(Minutes::new(0))), None);
        assert_eq!(prepare_control(timer, None, Some(Minutes::new(600))), None);
        assert_eq!(
            prepare_control(off, None, None),
            Some(ControlCommand::SetTimer(Minutes::new(0)))
        );
    }

    #[test]
    fn threshold_changes_preserve_other_raw_tenths_and_reject_unknown_values() {
        let temperature = DeviceCommand::LegacyAutomaticTemperature {
            temperature_f: 110.try_into().unwrap(),
        };
        let humidity = DeviceCommand::LegacyAutomaticHumidity {
            humidity_percent: 40.try_into().unwrap(),
        };
        assert_eq!(
            prepare_control(temperature, Some(thresholds(1051, 301)), None),
            Some(ControlCommand::SetAutomaticThresholds(thresholds(
                1100, 301
            )))
        );
        assert_eq!(
            prepare_control(humidity, Some(thresholds(1051, 301)), None),
            Some(ControlCommand::SetAutomaticThresholds(thresholds(
                1051, 400
            )))
        );
        assert_eq!(
            prepare_control(temperature, Some(thresholds(1051, 1000)), None),
            Some(ControlCommand::SetAutomaticThresholds(thresholds(
                1100, 1000
            )))
        );
        assert!(prepare_control(temperature, None, None).is_none());
        assert!(prepare_control(humidity, None, None).is_none());
        for invalid in [0, 299, 801, 999, 1001, u16::MAX] {
            assert!(prepare_control(temperature, Some(thresholds(1051, invalid)), None).is_none());
        }
        for invalid in [0, 899, 1201, u16::MAX] {
            assert!(prepare_control(humidity, Some(thresholds(invalid, 301)), None).is_none());
        }
    }

    #[test]
    fn timers_use_original_app_wire_units() {
        for minutes in [0, 1, 60, 360] {
            let command = DeviceCommand::LegacyTimer {
                minutes: minutes.try_into().unwrap(),
            };
            let prepared = prepare_control(command, None, None).unwrap();
            assert_eq!(
                prepared,
                ControlCommand::SetTimer(crate::protocol::Minutes::new(minutes))
            );
            assert_eq!(
                prepared.frame().as_bytes(),
                format!("#tms{minutes:04X}\n").as_bytes()
            );
        }
    }
}
