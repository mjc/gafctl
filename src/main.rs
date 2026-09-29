use std::{fmt, time::Duration};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use updraft_bluetooth::{DiscoveredDevice, ProbeMode, ProbeOptions, ProbeResult, probe};
use updraft_protocol::{
    Acknowledgement, AutomaticThresholds, ControlCommand, ControlOutcome, ControlReadback,
    DeviceSnapshot, HumidityTenthsPercent, Minutes, ReadCommand, ReadbackMatch, TemperatureTenthsF,
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

    /// Seconds allowed for each BLE operation and each command response.
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
    let result = probe(options.into_probe_options())
        .await
        .context("BLE probe failed")?;
    print_probe_result(result, show_identity);
    Ok(())
}

fn print_probe_result(result: ProbeResult, show_identity: bool) {
    match result {
        ProbeResult::NoDevices => {
            println!("No nearby BLE device advertising GAF service 00FF was found.");
        }
        ProbeResult::Discovered { devices } => {
            print_devices(devices.iter().map(|candidate| candidate.device()));
            println!("Scan-only mode: no connection or protocol request was made.");
        }
        ProbeResult::Ambiguous { devices } => {
            print_devices(devices.iter().map(|candidate| candidate.device()));
            println!(
                "More than one candidate found. Re-run with --device-id <id> to query one fan."
            );
        }
        ProbeResult::Queried { device, result } => {
            println!("Queried GAF BLE device: {}", DeviceDescription(&device));
            if let Some(control) = &result.control {
                print_control_acknowledgement(control);
            }
            print_snapshot(&result.snapshot, show_identity);
            if let Some(control) = &result.control {
                println!("{}", ControlReadbackDisplay(control.readback()));
            }
            if let updraft_bluetooth::DisconnectOutcome::Failed(error) = &result.disconnect {
                eprintln!("BLE query succeeded, but disconnect failed: {error}");
            }
        }
    }
}

fn print_devices<'a>(devices: impl ExactSizeIterator<Item = &'a DiscoveredDevice>) {
    println!("Found {} GAF BLE device(s):", devices.len());
    devices.enumerate().for_each(|(index, device)| {
        println!("  [{index}] {}", DeviceDescription(device));
    });
}

fn print_control_acknowledgement(control: &ControlOutcome) {
    let response = control.frame();
    let acknowledgement = match control.acknowledgement() {
        Acknowledgement::Accepted => "success",
        Acknowledgement::Unrecognized => "unrecognized/error",
    };
    println!(
        "control acknowledgement: {acknowledgement} ({} payload={})",
        String::from_utf8_lossy(&response.command()),
        String::from_utf8_lossy(response.payload()),
    );
}

fn print_snapshot(snapshot: &DeviceSnapshot, show_identity: bool) {
    snapshot.frames().for_each(|(request, response)| {
        let payload = display_reply_payload(request, response.payload(), show_identity);
        println!(
            "{} -> {} payload_hex={payload}",
            String::from_utf8_lossy(request.frame()).trim_end(),
            String::from_utf8_lossy(&response.command()),
        );
    });
}

fn display_reply_payload(
    request: ReadCommand,
    payload: &[u8],
    show_identity: bool,
) -> ReplyPayload<'_> {
    match (request, show_identity) {
        (ReadCommand::Identity, false) => ReplyPayload::Redacted(payload.len()),
        _ => ReplyPayload::Hex(payload),
    }
}

struct DeviceDescription<'a>(&'a DiscoveredDevice);

impl fmt::Display for DeviceDescription<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        let device = self.0;
        write!(
            output,
            "id={} name={} rssi={}",
            device.id,
            device.name.as_deref().unwrap_or("(not advertised)"),
            SignalStrength(device.rssi),
        )
    }
}

struct SignalStrength(Option<i16>);

impl fmt::Display for SignalStrength {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(rssi) => write!(output, "{rssi} dBm"),
            None => output.write_str("(unknown)"),
        }
    }
}

enum ReplyPayload<'a> {
    Redacted(usize),
    Hex(&'a [u8]),
}

impl fmt::Display for ReplyPayload<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Redacted(length) => write!(output, "<redacted; {length} bytes>"),
            Self::Hex(bytes) => bytes
                .iter()
                .try_for_each(|byte| write!(output, "{byte:02X}")),
        }
    }
}

struct ControlReadbackDisplay<'a>(&'a ControlReadback);

impl fmt::Display for ControlReadbackDisplay<'_> {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            ControlReadback::Thresholds(Ok(readback)) => write!(
                output,
                "automatic threshold readback: {}",
                describe_readback_match(readback.comparison),
            ),
            ControlReadback::Thresholds(Err(_)) => {
                output.write_str("automatic threshold readback: unrecognized payload")
            }
            ControlReadback::Timer(Ok(readback)) => write!(
                output,
                "timer readback: remaining={} minute(s), original={} minute(s); {}",
                readback.actual.remaining.value(),
                readback.actual.original.value(),
                describe_readback_match(readback.comparison),
            ),
            ControlReadback::Timer(Err(_)) => {
                output.write_str("timer readback: unrecognized payload")
            }
        }
    }
}

fn describe_readback_match(comparison: ReadbackMatch) -> &'static str {
    match comparison {
        ReadbackMatch::Matches => "matches request",
        ReadbackMatch::Differs => "differs from request",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use updraft_protocol::{PayloadError, Readback, ReadbackError, TimerState};

    #[test]
    fn borrowed_payload_display_preserves_hex_and_identity_redaction() {
        assert_eq!(
            display_reply_payload(ReadCommand::Identity, b"AB", false).to_string(),
            "<redacted; 2 bytes>",
        );
        assert_eq!(
            display_reply_payload(ReadCommand::Identity, b"AB", true).to_string(),
            "4142",
        );
        assert_eq!(
            display_reply_payload(ReadCommand::Sensors, &[0x00, 0xAF], false).to_string(),
            "00AF",
        );
    }

    #[test]
    fn threshold_report_formats_typed_outcomes() {
        let actual = AutomaticThresholds {
            temperature: TemperatureTenthsF::new(1050),
            humidity: HumidityTenthsPercent::new(300),
        };
        [
            (ReadbackMatch::Matches, "matches request"),
            (ReadbackMatch::Differs, "differs from request"),
        ]
        .into_iter()
        .for_each(|(comparison, status)| {
            let outcome = ControlReadback::Thresholds(Ok(Readback { actual, comparison }));
            assert_eq!(
                ControlReadbackDisplay(&outcome).to_string(),
                format!("automatic threshold readback: {status}"),
            );
        });
        let outcome =
            ControlReadback::Thresholds(Err(PayloadError::from(ReadbackError::InvalidHex)));
        assert_eq!(
            ControlReadbackDisplay(&outcome).to_string(),
            "automatic threshold readback: unrecognized payload",
        );
    }

    #[test]
    fn timer_report_formats_typed_outcomes() {
        [
            (ReadbackMatch::Matches, "matches request"),
            (ReadbackMatch::Differs, "differs from request"),
        ]
        .into_iter()
        .for_each(|(comparison, status)| {
            let outcome = ControlReadback::Timer(Ok(Readback {
                actual: TimerState {
                    remaining: Minutes::new(3),
                    original: Minutes::new(5),
                },
                comparison,
            }));
            assert_eq!(
                ControlReadbackDisplay(&outcome).to_string(),
                format!("timer readback: remaining=3 minute(s), original=5 minute(s); {status}"),
            );
        });
        let outcome = ControlReadback::Timer(Err(PayloadError::from(ReadbackError::InvalidHex)));
        assert_eq!(
            ControlReadbackDisplay(&outcome).to_string(),
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
