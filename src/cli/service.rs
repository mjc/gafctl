use std::{process::ExitCode, str::FromStr, time::Duration};

use crate::client::{Client, ClientError, ClientOptions, ServerUrl};
use crate::model::{
    AutomaticHumidityPercent, AutomaticTemperatureF, CommandId, DeviceCommand, DeviceId,
    QuickConnectMode,
};
use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use serde::Serialize;

use crate::arguments::DeadlineSeconds;

use super::{Preset, output::OutputFormat};

#[derive(Debug, Args)]
pub(super) struct ServiceOptions {
    /// Base HTTP/HTTPS service URL; may contain a reverse-proxy path prefix.
    #[arg(
        long,
        global = true,
        env = "GAFCTL_SERVER_URL",
        default_value = "http://127.0.0.1:8787"
    )]
    server: ServerUrl,
    #[arg(long, global = true, value_enum, default_value = "text")]
    format: OutputFormat,
    /// Override request deadline in seconds (reads default to 10, controls to 300).
    #[arg(long, global = true)]
    timeout_seconds: Option<DeadlineSeconds>,
}

impl ServiceOptions {
    fn client(&self) -> std::result::Result<Client, ClientError> {
        let options = match self.timeout_seconds {
            Some(timeout) => ClientOptions {
                read_timeout: Duration::from_secs(timeout.get()),
                control_timeout: Duration::from_secs(timeout.get()),
            },
            None => ClientOptions::default(),
        };
        Client::new(self.server.clone(), options)
    }

    pub(super) async fn devices(self) -> Result<ExitCode> {
        let result = async { self.client()?.devices().await }.await;
        match result {
            Ok(result) => {
                let text = if result.devices.is_empty() {
                    "No registered devices.".to_owned()
                } else {
                    result
                        .devices
                        .iter()
                        .map(|device| {
                            format!(
                                "{}: {} ({:?})\n  capabilities: {:?}",
                                device.id, device.name, device.backend, device.capabilities
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                self.format.write(&result, text)?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => self.format.error(&error),
        }
    }
}

#[derive(Debug, Args)]
pub(super) struct StateOptions {
    device_id: DeviceId,
    #[command(flatten)]
    service: ServiceOptions,
}

impl StateOptions {
    pub(super) async fn run(self) -> Result<ExitCode> {
        let result = async { self.service.client()?.state(&self.device_id).await }.await;
        match result {
            Ok(result) => {
                let mut text = format!(
                    "Device: {}\nAvailable: {}\nInventory: {:?}\nCached state:\n{}",
                    result.id,
                    result.available,
                    result.inventory_status,
                    serde_json::to_string_pretty(&result.state)?
                );
                if let Some(error) = &result.last_error {
                    use std::fmt::Write;
                    write!(text, "\nLast error: {error}")?;
                }
                self.service.format.write(&result, text)?;
                Ok(ExitCode::SUCCESS)
            }
            Err(error) => self.service.format.error(&error),
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Mode {
    Off,
    Automatic,
    Timer,
    Manual,
}

impl From<Mode> for QuickConnectMode {
    fn from(value: Mode) -> Self {
        match value {
            Mode::Off => Self::Off,
            Mode::Automatic => Self::Automatic,
            Mode::Timer => Self::Timer,
            Mode::Manual => Self::Manual,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct TargetTemperatureF(AutomaticTemperatureF);
impl FromStr for TargetTemperatureF {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .ok()
            .and_then(|value| AutomaticTemperatureF::try_from(value).ok())
            .map(Self)
            .ok_or("temperature must be an integer from 90 to 120 °F")
    }
}

#[derive(Clone, Copy, Debug)]
struct TargetHumidityPercent(AutomaticHumidityPercent);
impl FromStr for TargetHumidityPercent {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .ok()
            .and_then(|value| AutomaticHumidityPercent::try_from(value).ok())
            .map(Self)
            .ok_or("humidity must be an integer from 30 to 80 percent")
    }
}

#[derive(Clone, Copy, Debug)]
struct TimerDurationMinutes(u16);
impl FromStr for TimerDurationMinutes {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .ok()
            .filter(|value| (30..=360).contains(value) && value.is_multiple_of(30))
            .map(Self)
            .ok_or("duration must be 30 to 360 minutes in 30-minute steps")
    }
}

#[derive(Debug, Subcommand)]
enum ServiceControl {
    /// Apply a verified legacy BLE preset through the service.
    Preset {
        #[arg(value_enum)]
        preset: Preset,
    },
    /// Select an advertised QuickConnect mode.
    Mode {
        #[arg(value_enum)]
        mode: Mode,
    },
    /// Set both QuickConnect automatic targets (integer values).
    Targets {
        #[arg(long)]
        temperature_f: TargetTemperatureF,
        #[arg(long)]
        humidity_percent: TargetHumidityPercent,
    },
    /// Set the configured QuickConnect timer duration.
    TimerDuration { minutes: TimerDurationMinutes },
}

impl From<ServiceControl> for DeviceCommand {
    fn from(value: ServiceControl) -> Self {
        match value {
            ServiceControl::Preset { preset } => Self::LegacyPreset {
                preset: preset.into(),
            },
            ServiceControl::Mode { mode } => Self::QuickConnectMode { mode: mode.into() },
            ServiceControl::Targets {
                temperature_f,
                humidity_percent,
            } => Self::QuickConnectTargets {
                temperature_f: temperature_f.0.value(),
                humidity_percent: humidity_percent.0.value(),
            },
            ServiceControl::TimerDuration { minutes } => {
                Self::QuickConnectTimerDuration { minutes: minutes.0 }
            }
        }
    }
}

#[derive(Debug, Args)]
pub(super) struct ControlOptions {
    device_id: DeviceId,
    #[command(flatten)]
    service: ServiceOptions,
    /// Correlation ID; replay is limited to the running service's in-memory cache.
    #[arg(long, global = true)]
    request_id: Option<CommandId>,
    #[command(subcommand)]
    command: ServiceControl,
}

impl ControlOptions {
    pub(super) async fn run(self) -> Result<ExitCode> {
        let request_id = match self.request_id {
            Some(id) => id,
            None => uuid::Uuid::new_v4()
                .simple()
                .to_string()
                .parse()
                .map_err(anyhow::Error::msg)?,
        };
        let result = async {
            let client = self.service.client()?;
            client
                .prepare_control(&self.device_id, self.command.into())
                .await?
                .submit(request_id)
                .await
        }
        .await;
        match result {
            Ok(result) => {
                #[derive(Serialize)]
                struct ControlOutput<'a> {
                    #[serde(flatten)]
                    response: &'a crate::model::DeviceControlV2Response,
                    http_status: u16,
                }
                self.service.format.write(
                    &ControlOutput {
                        response: result.response(),
                        http_status: result.http_status(),
                    },
                    format!(
                        "Request: {}\nOutcome: {} (HTTP {})",
                        result.response().request_id,
                        result.response().status,
                        result.http_status()
                    ),
                )?;
                Ok(if result.is_confirmed() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                })
            }
            Err(error) => self.service.format.error(&error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serde_json::json;

    #[derive(Parser)]
    struct ControlParser {
        #[command(flatten)]
        options: ControlOptions,
    }

    #[derive(Parser)]
    struct ServiceParser {
        #[command(flatten)]
        options: ServiceOptions,
    }

    #[test]
    fn every_service_control_maps_to_the_existing_wire_contract() {
        for (args, expected) in [
            (
                "preset automatic-105-f-30-percent".split_whitespace(),
                json!({"kind":"legacy_preset","preset":"automatic105_f30_percent"}),
            ),
            (
                "preset automatic-105-1-f-30-1-percent".split_whitespace(),
                json!({"kind":"legacy_preset","preset":"automatic105_1_f30_1_percent"}),
            ),
            (
                "preset timer-clear".split_whitespace(),
                json!({"kind":"legacy_preset","preset":"timer_clear"}),
            ),
            (
                "preset timer-one-minute".split_whitespace(),
                json!({"kind":"legacy_preset","preset":"timer_one_minute"}),
            ),
            (
                "mode off".split_whitespace(),
                json!({"kind":"quick_connect_mode","mode":"off"}),
            ),
            (
                "mode automatic".split_whitespace(),
                json!({"kind":"quick_connect_mode","mode":"automatic"}),
            ),
            (
                "mode timer".split_whitespace(),
                json!({"kind":"quick_connect_mode","mode":"timer"}),
            ),
            (
                "mode manual".split_whitespace(),
                json!({"kind":"quick_connect_mode","mode":"manual"}),
            ),
            (
                "targets --temperature-f 90 --humidity-percent 80".split_whitespace(),
                json!({"kind":"quick_connect_targets","temperature_f":90,"humidity_percent":80}),
            ),
            (
                "timer-duration 360".split_whitespace(),
                json!({"kind":"quick_connect_timer_duration","minutes":360}),
            ),
        ] {
            let parsed = ControlParser::try_parse_from(
                ["gafctl", "local", "--format", "json"]
                    .into_iter()
                    .chain(args)
                    .chain(["--request-id", "chosen", "--timeout-seconds", "8"]),
            )
            .unwrap();
            assert_eq!(parsed.options.request_id.unwrap().as_str(), "chosen");
            assert_eq!(parsed.options.service.timeout_seconds.unwrap().get(), 8);
            assert_eq!(
                serde_json::to_value(DeviceCommand::from(parsed.options.command)).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn cli_targets_carry_validated_api_types() {
        let temperature: crate::model::AutomaticTemperatureF =
            "105".parse::<TargetTemperatureF>().unwrap().0;
        let humidity: crate::model::AutomaticHumidityPercent =
            "40".parse::<TargetHumidityPercent>().unwrap().0;
        assert_eq!(temperature.value(), 105);
        assert_eq!(humidity.value(), 40);
        assert_eq!(
            "121".parse::<TargetTemperatureF>().unwrap_err(),
            "temperature must be an integer from 90 to 120 °F"
        );
        assert_eq!(
            "81".parse::<TargetHumidityPercent>().unwrap_err(),
            "humidity must be an integer from 30 to 80 percent"
        );
    }

    #[test]
    fn service_defaults_and_numeric_boundaries_are_explicit() {
        let defaults = ServiceParser::try_parse_from(["gafctl"]).unwrap();
        assert_eq!(
            defaults.options.server.as_url().as_str(),
            "http://127.0.0.1:8787/"
        );
        assert_eq!(
            ClientOptions::default().read_timeout,
            Duration::from_secs(10)
        );
        assert_eq!(
            ClientOptions::default().control_timeout,
            Duration::from_secs(300)
        );
        for temperature in ["89", "121", "105.5", "-1"] {
            assert!(temperature.parse::<TargetTemperatureF>().is_err());
        }
        for humidity in ["29", "81", "40.1"] {
            assert!(humidity.parse::<TargetHumidityPercent>().is_err());
        }
        for timer in ["0", "29", "31", "361"] {
            assert!(timer.parse::<TimerDurationMinutes>().is_err());
        }
        for temperature in ["90", "120"] {
            assert!(temperature.parse::<TargetTemperatureF>().is_ok());
        }
        for humidity in ["30", "80"] {
            assert!(humidity.parse::<TargetHumidityPercent>().is_ok());
        }
        for timer in ["30", "360"] {
            assert!(timer.parse::<TimerDurationMinutes>().is_ok());
        }
    }
}
