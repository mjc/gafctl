use std::{fmt, process::ExitCode, str::FromStr, time::Duration};

use anyhow::Result;
use clap::{Args, Subcommand};
use gafctl_api::{ControlPreset, DeviceState};
use gafctl_bluetooth::{
    DisconnectOutcome, DiscoveredDevice, ProbeOptions, ProbeResult, QueryResult, probe,
};
use gafctl_protocol::{
    Acknowledgement, ControlOutcome, ControlReadback, ModeReadback, ReadbackMatch,
};
use serde::Serialize;

use crate::arguments::DeadlineSeconds;

use super::{Preset, output::OutputFormat};

#[derive(Debug, Args)]
pub(super) struct BleCommand {
    #[command(flatten)]
    settings: BleSettings,
    #[command(subcommand)]
    command: BleOperation,
}

#[derive(Debug, Args)]
struct BleSettings {
    #[arg(long, global = true, default_value = "5")]
    scan_seconds: DeadlineSeconds,
    /// Seconds for GATT setup, command writes, and responses; platform calls allow at least 40s.
    #[arg(long, global = true, default_value = "3")]
    timeout_seconds: DeadlineSeconds,
    #[arg(long, global = true, value_enum, default_value = "text")]
    format: OutputFormat,
    /// Include raw identity bytes that may contain a private identifier.
    #[arg(long, global = true)]
    show_identity: bool,
}

#[derive(Clone, Debug)]
struct PeripheralId(String);
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
    fn into_probe(self) -> (BleIntent, BleSettings, gafctl_bluetooth::ProbeOptions) {
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
            control_deadline: None,
            mode,
        };
        (intent, settings, options)
    }

    pub(super) async fn run(self) -> Result<ExitCode> {
        let (intent, settings, options) = self.into_probe();
        run(intent, settings, options).await
    }
}

#[derive(Clone, Copy)]
enum BleIntent {
    Scan,
    Read,
    Control,
}

#[derive(Serialize)]
struct Peripheral {
    peripheral_id: String,
    name: Option<String>,
    rssi_dbm: Option<i16>,
}

impl From<&DiscoveredDevice> for Peripheral {
    fn from(device: &DiscoveredDevice) -> Self {
        Self {
            peripheral_id: device.id.to_string(),
            name: device.name.clone(),
            rssi_dbm: device.rssi,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum BleReport {
    NoDevices,
    Discovered {
        devices: Vec<Peripheral>,
    },
    Ambiguous {
        devices: Vec<Peripheral>,
    },
    DiscoveryIncomplete {
        devices: Vec<Peripheral>,
        failures: Vec<DiscoveryError>,
    },
    Queried {
        device: Peripheral,
        query: Box<QueryReport>,
    },
}

impl fmt::Display for BleReport {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDevices => output.write_str("No BLE devices found."),
            Self::Discovered { devices }
            | Self::Ambiguous { devices }
            | Self::DiscoveryIncomplete { devices, .. } => {
                writeln!(output, "BLE devices:")?;
                devices.iter().try_for_each(|device| {
                    writeln!(
                        output,
                        "  {}: {} (RSSI {:?} dBm)",
                        device.peripheral_id,
                        device.name.as_deref().unwrap_or("unknown"),
                        device.rssi_dbm
                    )
                })?;
                match self {
                    Self::Discovered { .. } => {
                        output.write_str("Scan only; no connection or protocol request.")
                    }
                    Self::Ambiguous { .. } => {
                        output.write_str("Device selection is ambiguous; specify --device-id.")
                    }
                    Self::DiscoveryIncomplete { failures, .. } => write!(
                        output,
                        "Discovery incomplete: {}",
                        serde_json::to_string(failures).map_err(|_| fmt::Error)?
                    ),
                    _ => Ok(()),
                }
            }
            Self::Queried { device, query } => {
                writeln!(output, "Queried BLE device: {}", device.peripheral_id)?;
                writeln!(output, "Controller state (fan flag is not airflow proof):")?;
                output.write_str(&serde_json::to_string_pretty(query).map_err(|_| fmt::Error)?)
            }
        }
    }
}

#[derive(Serialize)]
struct DiscoveryError {
    peripheral_id: String,
    message: String,
}

#[derive(Serialize)]
struct FieldError {
    field: &'static str,
    message: String,
}

#[derive(Serialize)]
struct QueryReport {
    state: Option<DeviceState>,
    field_errors: Vec<FieldError>,
    control: Option<ControlReport>,
    state_error: Option<String>,
    discovery_failures: Vec<DiscoveryError>,
    disconnect: DisconnectReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    identity_payload_hex: Option<String>,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum DisconnectReport {
    Retained,
    Disconnected,
    Failed { message: String },
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum AcknowledgementReport {
    Accepted,
    Unrecognized,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum ReadbackStatus {
    Matches,
    Differs,
    DecodeError,
    Unavailable,
    FanFlagDiffers,
    UnverifiedTimerExpiry,
}

#[derive(Serialize)]
struct ReadbackReport {
    status: ReadbackStatus,
    message: String,
}

#[derive(Serialize)]
struct ControlReport {
    confirmed: bool,
    acknowledgement: AcknowledgementReport,
    readback: ReadbackReport,
    mode_readback: ReadbackReport,
}

fn project_control(control: &ControlOutcome) -> ControlReport {
    let readback_status = match control.readback() {
        ControlReadback::Thresholds(Ok(readback)) => match readback.comparison {
            ReadbackMatch::Matches => ReadbackStatus::Matches,
            ReadbackMatch::Differs => ReadbackStatus::Differs,
        },
        ControlReadback::Timer(Ok(readback)) => match readback.comparison {
            ReadbackMatch::Matches => ReadbackStatus::Matches,
            ReadbackMatch::Differs => ReadbackStatus::Differs,
        },
        ControlReadback::Thresholds(Err(_)) | ControlReadback::Timer(Err(_)) => {
            ReadbackStatus::DecodeError
        }
        ControlReadback::Unavailable => ReadbackStatus::Unavailable,
    };
    let mode = control.mode_readback();
    let mode_status = match mode {
        ModeReadback::Matches(_) => ReadbackStatus::Matches,
        ModeReadback::Differs(_) => ReadbackStatus::Differs,
        ModeReadback::FanFlagDiffers { .. } => ReadbackStatus::FanFlagDiffers,
        ModeReadback::UnverifiedTimerExpiry(_) => ReadbackStatus::UnverifiedTimerExpiry,
        ModeReadback::Unrecognized(_) => ReadbackStatus::DecodeError,
        ModeReadback::Unavailable => ReadbackStatus::Unavailable,
    };
    ControlReport {
        confirmed: control.is_confirmed(),
        acknowledgement: match control.acknowledgement() {
            Acknowledgement::Accepted => AcknowledgementReport::Accepted,
            Acknowledgement::Unrecognized => AcknowledgementReport::Unrecognized,
        },
        readback: ReadbackReport {
            status: readback_status,
            message: crate::control_display::ControlReadbackDisplay(control.readback()).to_string(),
        },
        mode_readback: ReadbackReport {
            status: mode_status,
            message: crate::control_display::ModeReadbackDisplay(mode).to_string(),
        },
    }
}

fn discovery_errors(failures: &[gafctl_bluetooth::DiscoveryFailure]) -> Vec<DiscoveryError> {
    failures
        .iter()
        .map(|failure| DiscoveryError {
            peripheral_id: failure.device_id.to_string(),
            message: failure.reason.clone(),
        })
        .collect()
}

fn project_query(result: &QueryResult, show_identity: bool) -> QueryReport {
    let mut field_errors = Vec::new();
    let state = result.snapshot.as_ref().map(|snapshot| {
        field_errors = [
            ("identity", snapshot.identity.decoded().err()),
            ("mode", snapshot.mode.decoded().err()),
            ("sensors", snapshot.sensors.decoded().err()),
            ("thresholds", snapshot.thresholds.decoded().err()),
            (
                "timer",
                snapshot
                    .timer
                    .as_ref()
                    .and_then(|timer| timer.decoded().err()),
            ),
        ]
        .into_iter()
        .filter_map(|(field, error)| {
            error.map(|error| FieldError {
                field,
                message: error.to_string(),
            })
        })
        .collect();
        crate::legacy_projection::project_snapshot(snapshot)
    });
    QueryReport {
        state,
        field_errors,
        control: result.control.as_ref().map(project_control),
        state_error: result.state_error.clone(),
        discovery_failures: discovery_errors(&result.discovery_failures),
        disconnect: match &result.disconnect {
            DisconnectOutcome::Retained => DisconnectReport::Retained,
            DisconnectOutcome::Disconnected => DisconnectReport::Disconnected,
            DisconnectOutcome::Failed(message) => DisconnectReport::Failed {
                message: message.clone(),
            },
        },
        identity_payload_hex: if show_identity {
            result.snapshot.as_ref().map(|snapshot| {
                snapshot
                    .identity
                    .frame()
                    .payload()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect()
            })
        } else {
            None
        },
    }
}

fn project_result(
    result: &ProbeResult,
    intent: BleIntent,
    show_identity: bool,
) -> (BleReport, ExitCode) {
    let (report, success) = match result {
        ProbeResult::NoDevices => (
            BleReport::NoDevices,
            match intent {
                BleIntent::Scan => true,
                BleIntent::Read | BleIntent::Control => false,
            },
        ),
        ProbeResult::Discovered { devices } => (
            BleReport::Discovered {
                devices: devices
                    .iter()
                    .map(|candidate| Peripheral::from(candidate.device()))
                    .collect(),
            },
            match intent {
                BleIntent::Scan => true,
                BleIntent::Read | BleIntent::Control => false,
            },
        ),
        ProbeResult::Ambiguous { devices } => (
            BleReport::Ambiguous {
                devices: devices
                    .iter()
                    .map(|candidate| Peripheral::from(candidate.device()))
                    .collect(),
            },
            false,
        ),
        ProbeResult::DiscoveryIncomplete { devices, failures } => (
            BleReport::DiscoveryIncomplete {
                devices: devices
                    .iter()
                    .map(|candidate| Peripheral::from(candidate.device()))
                    .collect(),
                failures: discovery_errors(failures),
            },
            false,
        ),
        ProbeResult::Queried { device, result } => {
            let query = project_query(result, show_identity);
            let success = query_succeeded(&query, intent);
            (
                BleReport::Queried {
                    device: Peripheral::from(device),
                    query: Box::new(query),
                },
                success,
            )
        }
    };
    (
        report,
        if success {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        },
    )
}

fn query_succeeded(query: &QueryReport, intent: BleIntent) -> bool {
    match intent {
        BleIntent::Scan => false,
        BleIntent::Read => {
            query.state.is_some() && query.field_errors.is_empty() && query.state_error.is_none()
        }
        BleIntent::Control => query
            .control
            .as_ref()
            .is_some_and(|control| control.confirmed),
    }
}

async fn run(intent: BleIntent, settings: BleSettings, options: ProbeOptions) -> Result<ExitCode> {
    match probe(options).await {
        Ok(result) => {
            let (report, code) = project_result(&result, intent, settings.show_identity);
            settings.format.write(&report, &report)?;
            Ok(code)
        }
        Err(error) => settings
            .format
            .failure("ble", error.to_string(), None, None),
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use gafctl_api::ControlPreset;
    use gafctl_bluetooth::{DisconnectOutcome, QueryResult};
    use gafctl_protocol::{ControlOutcome, DeviceSnapshot, Frame};

    use super::*;
    use clap::Parser;
    use gafctl_api::DeviceId;

    #[derive(clap::Parser)]
    struct BleParser {
        #[command(flatten)]
        options: BleCommand,
    }

    fn frame(wire: &'static [u8]) -> Frame<'static> {
        Frame::from_bytes(Bytes::from_static(wire)).unwrap()
    }

    fn query(mode: &'static [u8]) -> QueryResult {
        QueryResult {
            snapshot: Some(
                DeviceSnapshot::from_frames(
                    frame(b"#idr030000private-identity\n"),
                    frame(mode),
                    frame(b"#sdr03ca00aa\n"),
                    frame(b"#atr041a012c\n"),
                    frame(b"#ttr00000000\n"),
                )
                .unwrap(),
            ),
            state_error: None,
            discovery_failures: Vec::new(),
            control: None,
            disconnect: DisconnectOutcome::Disconnected,
        }
    }

    #[test]
    fn ble_projection_keeps_controller_flag_and_identity_opt_in_separate() {
        let result = query(b"#dmraf\n");
        let value = serde_json::to_value(project_query(&result, false)).unwrap();
        assert_eq!(value["state"]["temperature_f"], 97.0);
        assert_eq!(value["state"]["settings"]["controller_fan_on"], false);
        assert!(value["state"]["estimated_running"].is_null());
        assert!(value.get("identity_payload_hex").is_none());
        let exposed = serde_json::to_value(project_query(&result, true)).unwrap();
        assert!(
            exposed["identity_payload_hex"]
                .as_str()
                .unwrap()
                .contains("70726976617465")
        );
    }

    #[test]
    fn ble_projection_retains_good_measurements_when_one_field_is_malformed() {
        let result = query(b"#dmrxx\n");
        let value = serde_json::to_value(project_query(&result, false)).unwrap();
        assert_eq!(value["state"]["temperature_f"], 97.0);
        assert!(value["state"]["settings"]["mode"].is_null());
        assert_eq!(value["field_errors"][0]["field"], "mode");
    }

    #[test]
    fn unsafe_discovery_outcomes_fail_and_explain_selection_in_both_formats() {
        use super::{BleIntent, project_result};
        use gafctl_bluetooth::ProbeResult;
        use std::process::ExitCode;

        for (result, status, explanation) in [
            (
                ProbeResult::Ambiguous {
                    devices: Vec::new(),
                },
                "ambiguous",
                "Device selection is ambiguous; specify --device-id.",
            ),
            (
                ProbeResult::DiscoveryIncomplete {
                    devices: Vec::new(),
                    failures: Vec::new(),
                },
                "discovery_incomplete",
                "Discovery incomplete:",
            ),
        ] {
            for intent in [BleIntent::Scan, BleIntent::Read, BleIntent::Control] {
                let (report, code) = project_result(&result, intent, false);
                assert_eq!(code, ExitCode::FAILURE);
                let value = serde_json::to_value(&report).unwrap();
                assert_eq!(value["status"], status);
                assert_eq!(value["devices"], serde_json::json!([]));
                assert!(report.to_string().contains(explanation));
            }
        }
    }

    #[test]
    fn ambiguous_output_lists_candidate_ids_for_explicit_selection() {
        let report = super::BleReport::Ambiguous {
            devices: ["first-peripheral", "second-peripheral"]
                .into_iter()
                .map(|id| super::Peripheral {
                    peripheral_id: id.to_owned(),
                    name: Some("Attic fan".to_owned()),
                    rssi_dbm: Some(-65),
                })
                .collect(),
        };
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["status"], "ambiguous");
        assert_eq!(value["devices"][0]["peripheral_id"], "first-peripheral");
        assert_eq!(value["devices"][1]["peripheral_id"], "second-peripheral");
        let text = report.to_string();
        for id in ["first-peripheral", "second-peripheral"] {
            assert!(text.contains(id));
        }
        assert!(text.contains("specify --device-id"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn incomplete_discovery_preserves_peripheral_and_property_error() {
        use super::{BleIntent, project_result};
        use gafctl_bluetooth::{DiscoveryFailure, ProbeResult};
        use std::process::ExitCode;

        let peripheral_id = uuid::Uuid::from_u128(42);
        let result = ProbeResult::DiscoveryIncomplete {
            devices: Vec::new(),
            failures: vec![DiscoveryFailure {
                device_id: peripheral_id.into(),
                reason: "advertisement properties unavailable".to_owned(),
            }],
        };
        let (report, code) = project_result(&result, BleIntent::Read, false);
        assert_eq!(code, ExitCode::FAILURE);
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(
            value["failures"][0]["peripheral_id"],
            peripheral_id.to_string()
        );
        assert_eq!(
            value["failures"][0]["message"],
            "advertisement properties unavailable"
        );
        let text = report.to_string();
        assert!(text.contains(&peripheral_id.to_string()));
        assert!(text.contains("advertisement properties unavailable"));
    }

    #[test]
    fn acknowledged_write_without_readback_is_unconfirmed_and_keeps_cleanup_error() {
        let mut result = query(b"#dmraf\n");
        result.snapshot = None;
        result.control = Some(
            ControlOutcome::from_response(
                ControlPreset::TimerClear.command(),
                frame(b"#tmr0\n"),
                None,
            )
            .unwrap(),
        );
        result.state_error = Some("state read timed out".to_owned());
        result.disconnect = DisconnectOutcome::Failed("disconnect failed".to_owned());
        let value = serde_json::to_value(project_query(&result, false)).unwrap();
        assert_eq!(value["control"]["acknowledgement"], "accepted");
        assert_eq!(value["control"]["confirmed"], false);
        assert_eq!(value["control"]["readback"]["status"], "unavailable");
        assert_eq!(value["state_error"], "state read timed out");
        assert_eq!(value["disconnect"]["status"], "failed");
    }
    #[test]
    fn direct_outcomes_distinguish_empty_discovery_from_missing_read_and_confirmed_control() {
        use super::{BleIntent, project_result, query_succeeded};
        use gafctl_bluetooth::ProbeResult;
        use std::process::ExitCode;
        let (_, code) = project_result(&ProbeResult::NoDevices, BleIntent::Scan, false);
        assert_eq!(code, ExitCode::SUCCESS);
        for intent in [BleIntent::Read, BleIntent::Control] {
            assert_eq!(
                project_result(&ProbeResult::NoDevices, intent, false).1,
                ExitCode::FAILURE
            );
        }
        for (mode, acknowledgement, confirmed) in [
            (b"#dmrtf\n".as_slice(), b"#tmr0\n".as_slice(), true),
            (b"#dmrtn\n".as_slice(), b"#tmr0\n".as_slice(), false),
            (b"#dmrtf\n".as_slice(), b"#tmr1\n".as_slice(), false),
        ] {
            let mut result = query(mode);
            result.control = Some(
                ControlOutcome::from_response(
                    ControlPreset::TimerClear.command(),
                    frame(acknowledgement),
                    result.snapshot.as_ref(),
                )
                .unwrap(),
            );
            result.disconnect = DisconnectOutcome::Failed("cleanup warning".to_owned());
            let report = project_query(&result, false);
            assert_eq!(query_succeeded(&report, BleIntent::Control), confirmed);
            assert!(query_succeeded(&report, BleIntent::Read));
            let value = serde_json::to_value(report).unwrap();
            assert_eq!(value["control"]["confirmed"], confirmed);
            assert_eq!(value["disconnect"]["status"], "failed");
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
                "--format",
                "json",
                "--scan-seconds",
                "7",
                "--timeout-seconds",
                "4",
            ])
            .unwrap();
            let (intent, _, options) = parsed.options.into_probe();
            assert!(match intent {
                BleIntent::Control => true,
                BleIntent::Read | BleIntent::Scan => false,
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
}
