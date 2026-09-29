use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use updraft_bluetooth::{ProbeOptions, probe};
use updraft_protocol::{ControlCommand, ReadCommand};

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
    #[arg(long)]
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
        num_args = 2,
        value_names = ["TEMP_TENTHS_F", "HUMIDITY_TENTHS_PERCENT"]
    )]
    set_auto_thresholds_tenths: Option<Vec<u16>>,
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
    let control_command =
        options
            .set_auto_thresholds_tenths
            .map(|values| ControlCommand::SetAutomaticThresholds {
                temperature_tenths_f: values[0],
                humidity_tenths_percent: values[1],
            });
    let result = probe(ProbeOptions {
        scan_duration: Duration::from_secs(options.scan_seconds),
        response_timeout: Duration::from_secs(options.response_timeout_seconds),
        device_id: options.device_id,
        scan_only: options.scan_only,
        control_command,
    })
    .await
    .context("BLE probe failed")?;

    if result.devices.is_empty() {
        println!("No nearby BLE device advertising GAF service 00FF was found.");
        return Ok(());
    }

    println!("Found {} GAF BLE device(s):", result.devices.len());
    for (index, device) in result.devices.iter().enumerate() {
        println!(
            "  [{index}] id={} name={} rssi={}",
            device.id,
            device.name.as_deref().unwrap_or("(not advertised)"),
            device
                .rssi
                .map_or_else(|| "(unknown)".to_owned(), |rssi| format!("{rssi} dBm")),
        );
    }

    if options.scan_only {
        println!("Scan-only mode: no connection or protocol request was made.");
        return Ok(());
    }
    if result.replies.is_empty() && result.devices.len() > 1 {
        println!("More than one candidate found. Re-run with --device-id <id> to query one fan.");
        return Ok(());
    }
    if result.replies.is_empty() {
        bail!("a GAF peripheral was found, but no read responses were collected");
    }

    if let Some(response) = result.control_reply {
        let acknowledgement = if response.payload() == b"0" {
            "success"
        } else {
            "unrecognized/error"
        };
        println!(
            "control acknowledgement: {acknowledgement} ({} payload={})",
            String::from_utf8_lossy(&response.command()),
            String::from_utf8_lossy(response.payload()),
        );
    }

    for reply in &result.replies {
        let payload = if reply.request == ReadCommand::Identity && !show_identity {
            format!("<redacted; {} bytes>", reply.response.payload().len())
        } else {
            reply
                .response
                .payload()
                .iter()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>()
        };
        println!(
            "{} -> {} payload_hex={payload}",
            String::from_utf8_lossy(reply.request.frame()).trim_end(),
            String::from_utf8_lossy(&reply.response.command()),
        );
    }

    if let Some(expected) = control_command {
        let expected = expected.frame();
        let expected_threshold_payload = &expected[4..expected.len() - 1];
        let threshold_reply = result
            .replies
            .iter()
            .find(|reply| reply.request == ReadCommand::AutoThresholds);
        if let Some(threshold_reply) = threshold_reply {
            let matches = threshold_reply
                .response
                .payload()
                .eq_ignore_ascii_case(expected_threshold_payload);
            println!(
                "automatic threshold readback: {}",
                if matches {
                    "matches request"
                } else {
                    "differs from request"
                }
            );
        }
    }

    Ok(())
}
