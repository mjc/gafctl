use updraft_api::DeviceCommand;
use updraft_protocol::{
    AutomaticThresholds, ControlCommand, HumidityTenthsPercent, Minutes, TemperatureTenthsF,
};

pub(crate) fn prepare_control(
    command: DeviceCommand,
    thresholds: Option<AutomaticThresholds>,
) -> Option<ControlCommand> {
    match command {
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
        DeviceCommand::QuickConnectMode { .. }
        | DeviceCommand::QuickConnectTargets { .. }
        | DeviceCommand::QuickConnectTimerDuration { .. } => None,
    }
}

pub(crate) const fn needs_threshold_read(command: DeviceCommand) -> bool {
    match command {
        DeviceCommand::LegacyAutomaticTemperature { .. }
        | DeviceCommand::LegacyAutomaticHumidity { .. } => true,
        DeviceCommand::LegacyPreset { .. }
        | DeviceCommand::LegacyTimer { .. }
        | DeviceCommand::QuickConnectMode { .. }
        | DeviceCommand::QuickConnectTargets { .. }
        | DeviceCommand::QuickConnectTimerDuration { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use updraft_protocol::{HumidityTenthsPercent, TemperatureTenthsF};

    fn thresholds(temperature: u16, humidity: u16) -> AutomaticThresholds {
        AutomaticThresholds {
            temperature: TemperatureTenthsF::new(temperature),
            humidity: HumidityTenthsPercent::new(humidity),
        }
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
            prepare_control(temperature, Some(thresholds(1051, 301))),
            Some(ControlCommand::SetAutomaticThresholds(thresholds(
                1100, 301
            )))
        );
        assert_eq!(
            prepare_control(humidity, Some(thresholds(1051, 301))),
            Some(ControlCommand::SetAutomaticThresholds(thresholds(
                1051, 400
            )))
        );
        assert_eq!(
            prepare_control(temperature, Some(thresholds(1051, 1000))),
            Some(ControlCommand::SetAutomaticThresholds(thresholds(
                1100, 1000
            )))
        );
        assert!(prepare_control(temperature, None).is_none());
        assert!(prepare_control(humidity, None).is_none());
        for invalid in [0, 299, 801, 999, 1001, u16::MAX] {
            assert!(prepare_control(temperature, Some(thresholds(1051, invalid))).is_none());
        }
        for invalid in [0, 899, 1201, u16::MAX] {
            assert!(prepare_control(humidity, Some(thresholds(invalid, 301))).is_none());
        }
    }

    #[test]
    fn timers_need_no_threshold_read_and_use_original_app_wire_units() {
        for minutes in [0, 1, 60, 360] {
            let command = DeviceCommand::LegacyTimer {
                minutes: minutes.try_into().unwrap(),
            };
            let prepared = prepare_control(command, None).unwrap();
            assert_eq!(
                prepared,
                ControlCommand::SetTimer(updraft_protocol::Minutes::new(minutes))
            );
            assert_eq!(
                prepared.frame().as_bytes(),
                format!("#tms{minutes:04X}\n").as_bytes()
            );
        }
    }
}
