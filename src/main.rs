use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use updraft_bluetooth::{DiscoveredDevice, ProbeMode, ProbeOptions, ProbeResult, ReadReply, probe};
use updraft_protocol::{
    AutomaticThresholds, ControlCommand, Frame, HumidityTenthsPercent, Minutes, ReadCommand,
    TemperatureTenthsF, TimerState,
};

#[derive(Debug, Parser)]
#[command(name = "updraft", about = "GAF attic fan protocol probe")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect the legacy GAF protocol over a device transport.
    Probe(ProbeCommand),
}

#[derive(Debug, Args)]
struct ProbeCommand {
    #[command(subcommand)]
    transport: ProbeTransport,
}

#[derive(Debug, Subcommand)]
enum ProbeTransport {
    /// Discover and query the GAF BLE service without joining its Wi-Fi AP.
    Ble(BleOptions),
}

#[derive(Debug, Args)]
struct BleOptions {
    /// Scan only. Do not connect or send any protocol request.
    #[arg(
        long,
        conflicts_with_all = ["device_id", "set_auto_thresholds_tenths", "set_timer_minutes"]
    )]
    scan_only: bool,

    /// Peripheral ID printed by a scan-only run. Required if multiple fans are found.
    #[arg(long)]
    device_id: Option<String>,

    /// How long to scan for the GAF service.
    #[arg(long, default_value_t = 6)]
    scan_seconds: u64,

    /// Seconds to wait for each read response.
    #[arg(long, default_value_t = 3)]
    response_timeout_seconds: u64,

    /// Print the raw identity response, which may contain a device identifier.
    #[arg(long)]
    show_identity: bool,

    /// Set automatic thresholds before reading state. Values are tenths: e.g.
    /// 1050 means 105.0°F and 300 means 30.0% humidity. This is a normal
    /// fan-control write, not a firmware operation.
    #[arg(
        long,
        conflicts_with = "set_timer_minutes",
        num_args = 2,
        value_names = ["TEMP_TENTHS_F", "HUMIDITY_TENTHS_PERCENT"]
    )]
    set_auto_thresholds_tenths: Option<Vec<u16>>,

    /// Start timer mode for the given duration in minutes.
    #[arg(long, conflicts_with = "set_auto_thresholds_tenths")]
    set_timer_minutes: Option<u16>,
}

impl BleOptions {
    fn requested_control(&self) -> Option<ControlCommand> {
        self.set_auto_thresholds_tenths
            .as_deref()
            .map(|values| {
                let &[temperature, humidity]: &[u16; 2] = values
                    .try_into()
                    .expect("clap requires exactly two automatic thresholds");
                ControlCommand::SetAutomaticThresholds(AutomaticThresholds {
                    temperature: TemperatureTenthsF::new(temperature),
                    humidity: HumidityTenthsPercent::new(humidity),
                })
            })
            .or_else(|| {
                self.set_timer_minutes
                    .map(Minutes::new)
                    .map(ControlCommand::SetTimer)
            })
    }

    fn into_probe_options(self) -> ProbeOptions {
        let control_command = self.requested_control();
        let mode = if self.scan_only {
            ProbeMode::Scan
        } else {
            ProbeMode::Query {
                device_id: self.device_id,
                control_command,
            }
        };
        ProbeOptions {
            scan_duration: Duration::from_secs(self.scan_seconds),
            response_timeout: Duration::from_secs(self.response_timeout_seconds),
            mode,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Probe(ProbeCommand {
            transport: ProbeTransport::Ble(options),
        }) => run_ble_probe(options).await,
    }
}

async fn run_ble_probe(options: BleOptions) -> Result<()> {
    let show_identity = options.show_identity;
    let requested_control = options.requested_control();
    let result = probe(options.into_probe_options())
        .await
        .context("BLE probe failed")?;
    print_probe_result(result, requested_control, show_identity);
    Ok(())
}

fn print_probe_result(
    result: ProbeResult,
    requested_control: Option<ControlCommand>,
    show_identity: bool,
) {
    match result {
        ProbeResult::NoDevices => {
            println!("No nearby BLE device advertising GAF service 00FF was found.");
        }
        ProbeResult::Discovered { devices } => {
            print_devices(&devices);
            println!("Scan-only mode: no connection or protocol request was made.");
        }
        ProbeResult::Ambiguous { devices } => {
            print_devices(&devices);
            println!(
                "More than one candidate found. Re-run with --device-id <id> to query one fan."
            );
        }
        ProbeResult::Queried {
            devices,
            replies,
            control_reply,
        } => {
            print_devices(&devices);
            if let Some(response) = control_reply {
                print_control_acknowledgement(&response);
            }
            print_replies(&replies, show_identity);
            if let Some(command) = requested_control {
                println!("{}", format_control_readback(command, &replies));
            }
        }
    }
}

fn print_devices(devices: &[DiscoveredDevice]) {
    println!("Found {} GAF BLE device(s):", devices.len());
    devices.iter().enumerate().for_each(|(index, device)| {
        println!(
            "  [{index}] id={} name={} rssi={}",
            device.id,
            device.name.as_deref().unwrap_or("(not advertised)"),
            device
                .rssi
                .map_or_else(|| "(unknown)".to_owned(), |rssi| format!("{rssi} dBm")),
        );
    });
}

fn print_control_acknowledgement(response: &Frame) {
    let acknowledgement = match response.payload() {
        b"0" => "success",
        _ => "unrecognized/error",
    };
    println!(
        "control acknowledgement: {acknowledgement} ({} payload={})",
        String::from_utf8_lossy(&response.command()),
        String::from_utf8_lossy(response.payload()),
    );
}

fn print_replies(replies: &[ReadReply], show_identity: bool) {
    replies.iter().for_each(|reply| {
        let payload = format_reply_payload(reply, show_identity);
        println!(
            "{} -> {} payload_hex={payload}",
            String::from_utf8_lossy(reply.request.frame()).trim_end(),
            String::from_utf8_lossy(&reply.response.command()),
        );
    });
}

fn format_reply_payload(reply: &ReadReply, show_identity: bool) -> String {
    match (reply.request, show_identity) {
        (ReadCommand::Identity, false) => {
            format!("<redacted; {} bytes>", reply.response.payload().len())
        }
        _ => reply
            .response
            .payload()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect(),
    }
}

fn format_control_readback(command: ControlCommand, replies: &[ReadReply]) -> String {
    match command {
        ControlCommand::SetAutomaticThresholds(requested) => format_threshold_readback(
            requested,
            find_readback_payload(replies, ReadCommand::AutoThresholds),
        ),
        ControlCommand::SetTimer(requested) => format_timer_readback(
            requested,
            find_readback_payload(replies, ReadCommand::Timer),
        ),
    }
}

fn find_readback_payload(replies: &[ReadReply], request: ReadCommand) -> Option<&[u8]> {
    replies
        .iter()
        .find(|reply| reply.request == request)
        .map(|reply| reply.response.payload())
}

fn format_threshold_readback(requested: AutomaticThresholds, payload: Option<&[u8]>) -> String {
    let status = match payload.map(AutomaticThresholds::parse) {
        Some(Ok(actual)) => describe_readback_match(actual == requested),
        Some(Err(_)) => "unrecognized payload",
        None => "unavailable",
    };
    format!("automatic threshold readback: {status}")
}

fn format_timer_readback(requested: Minutes, payload: Option<&[u8]>) -> String {
    let timer = payload.and_then(|payload| TimerState::parse(payload).ok());
    match timer {
        Some(actual) => {
            format!(
                "timer readback: remaining={} minute(s), original={} minute(s); {}",
                actual.remaining.value(),
                actual.original.value(),
                describe_readback_match(actual.matches_requested_duration(requested)),
            )
        }
        None => "timer readback: unrecognized payload".to_owned(),
    }
}

fn describe_readback_match(matches: bool) -> &'static str {
    if matches {
        "matches request"
    } else {
        "differs from request"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_report_distinguishes_values_from_missing_or_invalid_data() {
        let requested = AutomaticThresholds {
            temperature: TemperatureTenthsF::new(1050),
            humidity: HumidityTenthsPercent::new(300),
        };
        [
            (Some(b"041a012c".as_slice()), "matches request"),
            (Some(b"041b012c".as_slice()), "differs from request"),
            (Some(b"invalid!".as_slice()), "unrecognized payload"),
            (None, "unavailable"),
        ]
        .into_iter()
        .for_each(|(payload, status)| {
            assert_eq!(
                format_threshold_readback(requested, payload),
                format!("automatic threshold readback: {status}"),
            );
        });
    }

    #[test]
    fn timer_report_accepts_elapsed_time_but_detects_inconsistent_duration() {
        let requested = Minutes::new(5);
        [
            (b"00030005", 3, 5, "matches request"),
            (b"00060005", 6, 5, "differs from request"),
            (b"00030004", 3, 4, "differs from request"),
        ]
        .into_iter()
        .for_each(|(payload, remaining, original, status)| {
            assert_eq!(
                format_timer_readback(requested, Some(payload)),
                format!(
                    "timer readback: remaining={remaining} minute(s), original={original} minute(s); {status}",
                ),
            );
        });
        assert_eq!(
            format_timer_readback(requested, Some(b"bad data")),
            "timer readback: unrecognized payload",
        );
    }

    #[test]
    fn scan_only_rejects_control_settings() {
        [
            &[
                "updraft",
                "probe",
                "ble",
                "--scan-only",
                "--set-timer-minutes",
                "1",
            ][..],
            &[
                "updraft",
                "probe",
                "ble",
                "--scan-only",
                "--set-auto-thresholds-tenths",
                "1050",
                "300",
            ][..],
        ]
        .into_iter()
        .for_each(|args| assert!(Cli::try_parse_from(args).is_err()));
    }
}
