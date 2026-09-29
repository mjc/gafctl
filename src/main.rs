use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use updraft_bluetooth::{DiscoveredDevice, ProbeMode, ProbeOptions, ProbeResult, ReadReply, probe};
use updraft_protocol::{ControlCommand, Frame, ReadCommand};

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
    let control_command = match options.set_auto_thresholds_tenths {
        Some(values) => {
            let [temperature_tenths_f, humidity_tenths_percent] = values
                .try_into()
                .expect("clap requires exactly two automatic thresholds");
            Some(ControlCommand::SetAutomaticThresholds {
                temperature_tenths_f,
                humidity_tenths_percent,
            })
        }
        None => options
            .set_timer_minutes
            .map(|duration_minutes| ControlCommand::SetTimer { duration_minutes }),
    };
    let mode = match options.scan_only {
        true => ProbeMode::Scan,
        false => ProbeMode::Query {
            device_id: options.device_id,
            control_command,
        },
    };
    let result = probe(ProbeOptions {
        scan_duration: Duration::from_secs(options.scan_seconds),
        response_timeout: Duration::from_secs(options.response_timeout_seconds),
        mode,
    })
    .await
    .context("BLE probe failed")?;

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
            print_control_reply(control_reply.as_ref());
            print_replies(&replies, show_identity);
            print_control_readback(control_command, &replies);
        }
    }

    Ok(())
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

fn print_control_reply(response: Option<&Frame>) {
    if let Some(response) = response {
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
}

fn print_replies(replies: &[ReadReply], show_identity: bool) {
    replies.iter().for_each(|reply| {
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
    });
}

fn print_control_readback(control_command: Option<ControlCommand>, replies: &[ReadReply]) {
    match control_command {
        Some(ControlCommand::SetAutomaticThresholds {
            temperature_tenths_f,
            humidity_tenths_percent,
        }) => {
            let expected_payload =
                format!("{temperature_tenths_f:04X}{humidity_tenths_percent:04X}");
            let status = replies
                .iter()
                .find(|reply| reply.request == ReadCommand::AutoThresholds)
                .map(|reply| {
                    if reply
                        .response
                        .payload()
                        .eq_ignore_ascii_case(expected_payload.as_bytes())
                    {
                        "matches request"
                    } else {
                        "differs from request"
                    }
                })
                .unwrap_or("unavailable");
            println!("automatic threshold readback: {status}");
        }
        Some(ControlCommand::SetTimer { duration_minutes }) => {
            let timer = replies
                .iter()
                .find(|reply| reply.request == ReadCommand::Timer)
                .and_then(|reply| parse_timer_readback(reply.response.payload()));
            match timer {
                Some((remaining_minutes, original_minutes)) => {
                    let matches = original_minutes == duration_minutes
                        && remaining_minutes <= duration_minutes;
                    println!(
                        "timer readback: remaining={remaining_minutes} minute(s), original={original_minutes} minute(s); {}",
                        if matches {
                            "matches request"
                        } else {
                            "differs from request"
                        }
                    );
                }
                None => {
                    println!("timer readback: unrecognized payload");
                }
            }
        }
        None => {}
    }
}

fn parse_timer_readback(payload: &[u8]) -> Option<(u16, u16)> {
    std::str::from_utf8(payload)
        .ok()
        .filter(|payload| {
            payload.len() == 8 && payload.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .and_then(|payload| {
            let (remaining, original) = payload.split_at(4);
            Some((
                u16::from_str_radix(remaining, 16).ok()?,
                u16::from_str_radix(original, 16).ok()?,
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::{Cli, Parser};

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
