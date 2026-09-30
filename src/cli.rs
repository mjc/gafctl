use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use updraft_bluetooth::{ProbeMode, ProbeOptions, probe};
use updraft_protocol::{
    AutomaticThresholds, ControlCommand, HumidityTenthsPercent, Minutes, TemperatureTenthsF,
};

#[derive(Debug, Parser)]
#[command(name = "updraft", about = "GAF attic fan protocol probe")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Inspect the GAF Wi-Fi Vent over a device transport.
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
        action = clap::ArgAction::Set,
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
            .and_then(|values| match values {
                [temperature, humidity] => Some(ControlCommand::SetAutomaticThresholds(
                    AutomaticThresholds {
                        temperature: TemperatureTenthsF::new(*temperature),
                        humidity: HumidityTenthsPercent::new(*humidity),
                    },
                )),
                _ => None,
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

pub(crate) async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Probe(ProbeCommand {
            transport: ProbeTransport::Ble(options),
        }) => run_ble_probe(options).await,
    }
}

async fn run_ble_probe(options: BleOptions) -> Result<()> {
    let show_identity = options.show_identity;
    let control_requested = options.requested_control().is_some();
    let probe_result = probe(options.into_probe_options()).await;
    let result = probe_result.context("BLE probe failed")?;
    let control_confirmed = control_result_confirmed(control_requested, &result);
    crate::output::print_probe_result(result, show_identity);
    if control_requested && !control_confirmed {
        bail!("requested control was not confirmed");
    }
    Ok(())
}

fn control_result_confirmed(
    control_requested: bool,
    result: &updraft_bluetooth::ProbeResult,
) -> bool {
    let (selected, control) = match result {
        updraft_bluetooth::ProbeResult::Queried { result, .. } => (true, result.control.as_ref()),
        updraft_bluetooth::ProbeResult::NoDevices
        | updraft_bluetooth::ProbeResult::Discovered { .. }
        | updraft_bluetooth::ProbeResult::Ambiguous { .. } => (false, None),
    };
    control_status_successful(control_requested, selected, control)
}

fn control_status_successful(
    control_requested: bool,
    selected: bool,
    control: Option<&updraft_protocol::ControlOutcome>,
) -> bool {
    !control_requested
        || (selected && control.is_some_and(updraft_protocol::ControlOutcome::is_confirmed))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn repeated_threshold_arguments_are_rejected_without_panicking() {
        assert!(
            Cli::try_parse_from([
                "updraft",
                "probe",
                "ble",
                "--set-auto-thresholds-tenths",
                "1050",
                "300",
                "--set-auto-thresholds-tenths",
                "1100",
                "350",
            ])
            .is_err()
        );
    }

    #[test]
    fn control_status_requires_selected_device_and_confirmed_readback() {
        use updraft_bluetooth::ProbeResult;
        use updraft_protocol::{ControlCommand, ControlOutcome, Frame, Minutes};

        assert!(!control_result_confirmed(true, &ProbeResult::NoDevices));
        assert!(!control_result_confirmed(
            true,
            &ProbeResult::Ambiguous {
                devices: Vec::new()
            },
        ));
        assert!(control_result_confirmed(false, &ProbeResult::NoDevices));

        let snapshot = |timer: &[u8]| {
            updraft_protocol::DeviceSnapshot::from_frames(
                Frame::parse(b"#idr030000\n").unwrap().into_owned(),
                Frame::parse(b"#dmran\n").unwrap().into_owned(),
                Frame::parse(b"#sdr03CA00AA\n").unwrap().into_owned(),
                Frame::parse(b"#atr041a012c\n").unwrap().into_owned(),
                Frame::parse(timer).unwrap().into_owned(),
            )
            .unwrap()
        };
        let command = ControlCommand::SetTimer(Minutes::new(2));
        let matching = snapshot(b"#ttr00010002\n");
        let unrecognized = ControlOutcome::from_response(
            command,
            Frame::parse(b"#tmr1\n").unwrap().into_owned(),
            Some(&matching),
        )
        .unwrap();
        let differing = snapshot(b"#ttr00010003\n");
        let mismatched = ControlOutcome::from_response(
            command,
            Frame::parse(b"#tmr0\n").unwrap().into_owned(),
            Some(&differing),
        )
        .unwrap();
        let confirmed = ControlOutcome::from_response(
            command,
            Frame::parse(b"#tmr0\n").unwrap().into_owned(),
            Some(&matching),
        )
        .unwrap();

        assert!(!control_status_successful(true, true, Some(&unrecognized)));
        assert!(!control_status_successful(true, true, Some(&mismatched)));
        assert!(control_status_successful(true, true, Some(&confirmed)));
    }
}
