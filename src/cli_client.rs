use std::{fmt::Display, num::NonZeroU64, process::ExitCode, str::FromStr, time::Duration};

use anyhow::Result;
use clap::{Args, Subcommand, ValueEnum};
use gafctl_api::{CommandId, ControlPreset, DeviceCommand, DeviceId, QuickConnectMode};
use gafctl_client::{Client, ClientError, ClientOptions, ServerUrl};
use serde::Serialize;

#[derive(Clone, Copy, Debug, Default, ValueEnum)]
pub(crate) enum OutputFormat {
    #[default]
    Text,
    Json,
}

impl OutputFormat {
    pub(crate) fn write(&self, value: &impl Serialize, text: impl Display) -> Result<()> {
        match self {
            Self::Text => crate::output::write_stdout(|output| writeln!(output, "{text}")),
            Self::Json => {
                let mut bytes = serde_json::to_vec(value)?;
                bytes.push(b'\n');
                crate::output::write_stdout(|output| output.write_all(&bytes))
            }
        }
    }

    fn error(&self, error: &ClientError) -> Result<ExitCode> {
        self.failure(
            error.kind(),
            error.to_string(),
            error.request_id(),
            error.http_status(),
        )
    }

    pub(crate) fn failure(
        &self,
        kind: &'static str,
        message: String,
        request_id: Option<&CommandId>,
        http_status: Option<u16>,
    ) -> Result<ExitCode> {
        #[derive(Serialize)]
        struct ErrorDetails<'a> {
            kind: &'static str,
            message: String,
            request_id: Option<&'a CommandId>,
            http_status: Option<u16>,
        }
        #[derive(Serialize)]
        struct ErrorResponse<'a> {
            error: ErrorDetails<'a>,
        }
        match self {
            Self::Text => eprintln!("{message}"),
            Self::Json => self.write(
                &ErrorResponse {
                    error: ErrorDetails {
                        kind,
                        message,
                        request_id,
                        http_status,
                    },
                },
                "",
            )?,
        }
        Ok(ExitCode::FAILURE)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DeadlineSeconds(NonZeroU64);

impl DeadlineSeconds {
    fn get(self) -> u64 {
        self.0.get()
    }
}

impl FromStr for DeadlineSeconds {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let seconds = value
            .parse::<NonZeroU64>()
            .map_err(|_| "deadline must be a positive integer")?;
        std::time::Instant::now()
            .checked_add(Duration::from_secs(seconds.get()))
            .ok_or("deadline is too large for this platform")?;
        Ok(Self(seconds))
    }
}

#[derive(Debug, Args)]
pub(crate) struct ServiceOptions {
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

    pub(crate) async fn devices(self) -> Result<ExitCode> {
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
pub(crate) struct StateOptions {
    device_id: DeviceId,
    #[command(flatten)]
    service: ServiceOptions,
}

impl StateOptions {
    pub(crate) async fn run(self) -> Result<ExitCode> {
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
pub(crate) enum Preset {
    #[value(name = "automatic-105-f-30-percent")]
    Automatic105F30Percent,
    #[value(name = "automatic-105-1-f-30-1-percent")]
    Automatic105_1F30_1Percent,
    TimerClear,
    TimerOneMinute,
}

impl From<Preset> for ControlPreset {
    fn from(value: Preset) -> Self {
        match value {
            Preset::Automatic105F30Percent => Self::Automatic105F30Percent,
            Preset::Automatic105_1F30_1Percent => Self::Automatic105_1F30_1Percent,
            Preset::TimerClear => Self::TimerClear,
            Preset::TimerOneMinute => Self::TimerOneMinute,
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
struct TargetTemperatureF(u16);
impl FromStr for TargetTemperatureF {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .ok()
            .filter(|value| (90..=120).contains(value))
            .map(Self)
            .ok_or("temperature must be an integer from 90 to 120 °F")
    }
}

#[derive(Clone, Copy, Debug)]
struct TargetHumidityPercent(u16);
impl FromStr for TargetHumidityPercent {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        value
            .parse::<u16>()
            .ok()
            .filter(|value| (30..=80).contains(value))
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
                temperature_f: temperature_f.0,
                humidity_percent: humidity_percent.0,
            },
            ServiceControl::TimerDuration { minutes } => {
                Self::QuickConnectTimerDuration { minutes: minutes.0 }
            }
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct ControlOptions {
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
    pub(crate) async fn run(self) -> Result<ExitCode> {
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
                    response: &'a gafctl_api::DeviceControlV2Response,
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

#[derive(Debug, Args)]
pub(crate) struct BleCommand {
    #[command(flatten)]
    settings: BleSettings,
    #[command(subcommand)]
    command: BleOperation,
}

#[derive(Debug, Args)]
pub(crate) struct BleSettings {
    #[arg(long, global = true, default_value = "6")]
    pub(crate) scan_seconds: DeadlineSeconds,
    /// Seconds for GATT setup, command writes, and responses; platform calls allow at least 40s.
    #[arg(long, global = true, default_value = "3")]
    pub(crate) timeout_seconds: DeadlineSeconds,
    #[arg(long, global = true, value_enum, default_value = "text")]
    pub(crate) format: OutputFormat,
    /// Include raw identity bytes that may contain a private identifier.
    #[arg(long, global = true)]
    pub(crate) show_identity: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PeripheralId(String);
impl FromStr for PeripheralId {
    type Err = &'static str;
    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        if value.trim().is_empty() {
            Err("peripheral ID must not be empty")
        } else {
            Ok(Self(value.to_owned()))
        }
    }
}

#[derive(Debug, Subcommand)]
enum BleOperation {
    /// Discover BLE advertisements without connecting or reading the protocol.
    Scan,
    /// Query a fan directly; auto-selection requires one unambiguous candidate.
    State {
        #[arg(long)]
        device_id: Option<PeripheralId>,
    },
    /// Apply a tested preset to the selected fan.
    Control {
        #[arg(long)]
        device_id: PeripheralId,
        #[command(subcommand)]
        command: BleControl,
    },
}

#[derive(Debug, Subcommand)]
enum BleControl {
    Preset {
        #[arg(value_enum)]
        preset: Preset,
    },
}

impl BleCommand {
    fn into_probe(
        self,
    ) -> (
        crate::cli_ble::BleIntent,
        BleSettings,
        gafctl_bluetooth::ProbeOptions,
    ) {
        use crate::cli_ble::BleIntent;
        use gafctl_bluetooth::{ProbeMode, ProbeOptions};
        let settings = self.settings;
        let (intent, mode) = match self.command {
            BleOperation::Scan => (BleIntent::Scan, ProbeMode::Scan),
            BleOperation::State { device_id } => (
                BleIntent::Read,
                ProbeMode::Query {
                    device_id: device_id.map(|id| id.0),
                    control_command: None,
                },
            ),
            BleOperation::Control {
                device_id,
                command: BleControl::Preset { preset },
            } => (
                BleIntent::Control,
                ProbeMode::Query {
                    device_id: Some(device_id.0),
                    control_command: Some(ControlPreset::from(preset).command()),
                },
            ),
        };
        let options = ProbeOptions {
            scan_duration: Duration::from_secs(settings.scan_seconds.get()),
            response_timeout: Duration::from_secs(settings.timeout_seconds.get()),
            mode,
        };
        (intent, settings, options)
    }

    pub(crate) async fn run(self) -> Result<ExitCode> {
        let (intent, settings, options) = self.into_probe();
        crate::cli_ble::run(intent, settings, options).await
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
    struct BleParser {
        #[command(flatten)]
        options: BleCommand,
    }
    #[derive(Parser)]
    struct ServiceParser {
        #[command(flatten)]
        options: ServiceOptions,
    }

    #[test]
    fn unrepresentable_deadlines_are_rejected_before_transport_access() {
        assert!(
            BleParser::try_parse_from([
                "gafctl",
                "state",
                "--timeout-seconds",
                "18446744073709551615"
            ])
            .is_err()
        );
        assert!(
            ServiceParser::try_parse_from(["gafctl", "--timeout-seconds", "18446744073709551615"])
                .is_err()
        );
    }

    #[test]
    fn every_service_control_maps_to_the_existing_wire_contract() {
        for (args, expected) in [
            (
                vec!["preset", "automatic-105-f-30-percent"],
                json!({"kind":"legacy_preset","preset":"automatic105_f30_percent"}),
            ),
            (
                vec!["preset", "automatic-105-1-f-30-1-percent"],
                json!({"kind":"legacy_preset","preset":"automatic105_1_f30_1_percent"}),
            ),
            (
                vec!["preset", "timer-clear"],
                json!({"kind":"legacy_preset","preset":"timer_clear"}),
            ),
            (
                vec!["preset", "timer-one-minute"],
                json!({"kind":"legacy_preset","preset":"timer_one_minute"}),
            ),
            (
                vec!["mode", "off"],
                json!({"kind":"quick_connect_mode","mode":"off"}),
            ),
            (
                vec!["mode", "automatic"],
                json!({"kind":"quick_connect_mode","mode":"automatic"}),
            ),
            (
                vec!["mode", "timer"],
                json!({"kind":"quick_connect_mode","mode":"timer"}),
            ),
            (
                vec!["mode", "manual"],
                json!({"kind":"quick_connect_mode","mode":"manual"}),
            ),
            (
                vec![
                    "targets",
                    "--temperature-f",
                    "90",
                    "--humidity-percent",
                    "80",
                ],
                json!({"kind":"quick_connect_targets","temperature_f":90,"humidity_percent":80}),
            ),
            (
                vec!["timer-duration", "360"],
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
    fn ble_shared_flags_work_before_and_after_operation_subcommands() {
        assert!(
            BleParser::try_parse_from([
                "gafctl",
                "--format",
                "json",
                "--scan-seconds",
                "7",
                "scan"
            ])
            .is_ok()
        );
        assert!(
            BleParser::try_parse_from([
                "gafctl",
                "--timeout-seconds",
                "4",
                "control",
                "--device-id",
                "id",
                "preset",
                "timer-clear",
                "--format",
                "json"
            ])
            .is_ok()
        );
    }

    #[test]
    fn ble_preset_mapping_preserves_target_and_transport_deadlines() {
        use gafctl_protocol::{
            AutomaticThresholds, ControlCommand, HumidityTenthsPercent, Minutes, TemperatureTenthsF,
        };
        for (name, command) in [
            (
                "automatic-105-f-30-percent",
                ControlCommand::SetAutomaticThresholds(AutomaticThresholds {
                    temperature: TemperatureTenthsF::new(1050),
                    humidity: HumidityTenthsPercent::new(300),
                }),
            ),
            (
                "automatic-105-1-f-30-1-percent",
                ControlCommand::SetAutomaticThresholds(AutomaticThresholds {
                    temperature: TemperatureTenthsF::new(1051),
                    humidity: HumidityTenthsPercent::new(301),
                }),
            ),
            ("timer-clear", ControlCommand::SetTimer(Minutes::new(0))),
            (
                "timer-one-minute",
                ControlCommand::SetTimer(Minutes::new(1)),
            ),
        ] {
            let parsed = BleParser::try_parse_from([
                "gafctl",
                "control",
                "--device-id",
                "platform/id",
                "preset",
                name,
                "--scan-seconds",
                "7",
                "--timeout-seconds",
                "4",
            ])
            .unwrap();
            let (intent, _, options) = parsed.options.into_probe();
            assert!(match intent {
                crate::cli_ble::BleIntent::Control => true,
                crate::cli_ble::BleIntent::Read | crate::cli_ble::BleIntent::Scan => false,
            });
            assert_eq!(options.scan_duration, Duration::from_secs(7));
            assert_eq!(options.response_timeout, Duration::from_secs(4));
            match options.mode {
                gafctl_bluetooth::ProbeMode::Query {
                    device_id,
                    control_command,
                } => {
                    assert_eq!(device_id.as_deref(), Some("platform/id"));
                    assert_eq!(control_command, Some(command));
                }
                gafctl_bluetooth::ProbeMode::Scan => unreachable!("control mapped to scan"),
            }
        }
    }

    #[test]
    fn ble_controls_require_explicit_target_and_only_offer_verified_presets() {
        for preset in [
            "automatic-105-f-30-percent",
            "automatic-105-1-f-30-1-percent",
            "timer-clear",
            "timer-one-minute",
        ] {
            assert!(
                BleParser::try_parse_from([
                    "gafctl",
                    "control",
                    "--device-id",
                    "platform/id",
                    "preset",
                    preset,
                    "--format",
                    "json"
                ])
                .is_ok()
            );
        }
        for args in [
            vec!["control", "preset", "timer-clear"],
            vec!["control", "--device-id", "id", "mode", "off"],
            vec!["scan", "--device-id", "id"],
            vec!["state", "--timeout-seconds", "0"],
        ] {
            assert!(BleParser::try_parse_from(["gafctl"].into_iter().chain(args)).is_err());
        }
        assert!(BleParser::try_parse_from(["gafctl", "state"]).is_ok());
        assert!("platform/id".parse::<PeripheralId>().is_ok());
        assert!("platform/id".parse::<DeviceId>().is_err());
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
