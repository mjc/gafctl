use std::time::Duration;
use std::{net::SocketAddr, path::PathBuf};

use super::config::{ServerConfig, read_quickconnect_config};
#[cfg(feature = "mqtt")]
use super::config::{environment_value, mqtt_config};
use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use gafctl_bluetooth::{ProbeMode, ProbeOptions, probe};
use gafctl_protocol::{
    AutomaticThresholds, ControlCommand, HumidityTenthsPercent, Minutes, TemperatureTenthsF,
};
use std::process::ExitCode;

use crate::arguments::DeadlineSeconds;

#[derive(Debug, Parser)]
#[command(
    name = "gafctl-server",
    version,
    about = "GAF attic fan proxy and controller"
)]
struct Cli {
    #[command(flatten)]
    options: ServeOptions,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate shell completions without connecting to any transport.
    Completions { shell: clap_complete::Shell },
    /// Inspect the GAF Wi-Fi Vent over a device transport.
    Probe(ProbeCommand),
}

#[derive(Debug, Args)]
struct ServeOptions {
    /// Peripheral ID printed by a scan-only run. Never include it in logs or API responses.
    #[arg(long, env = "GAFCTL_DEVICE_ID")]
    device_id: Option<String>,

    /// Path to the private account/provider-to-local device identity map.
    #[arg(long, env = "GAFCTL_IDENTITY_STORE")]
    identity_store: Option<PathBuf>,

    /// Listener address. Non-loopback addresses require --allow-remote.
    #[arg(long, default_value = "127.0.0.1:8787")]
    bind: SocketAddr,

    /// Allow non-loopback access. Restrict network access with the host firewall.
    #[arg(long)]
    allow_remote: bool,

    /// MQTT broker host. When set, publish retained state and availability.
    #[cfg(feature = "mqtt")]
    #[arg(
        long,
        env = "GAFCTL_MQTT_HOST",
        requires = "mqtt_username",
        value_parser = clap::builder::NonEmptyStringValueParser::new()
    )]
    mqtt_host: Option<String>,

    /// MQTT broker port.
    #[cfg(feature = "mqtt")]
    #[arg(long, env = "GAFCTL_MQTT_PORT", default_value_t = 1883)]
    mqtt_port: u16,

    /// MQTT username. Required with --mqtt-host.
    #[cfg(feature = "mqtt")]
    #[arg(
        long,
        env = "GAFCTL_MQTT_USERNAME",
        requires = "mqtt_host",
        value_parser = clap::builder::NonEmptyStringValueParser::new()
    )]
    mqtt_username: Option<String>,

    /// Enable MQTT discovery for devices whose Home Assistant source is MQTT.
    #[cfg(feature = "mqtt")]
    #[arg(long, env = "GAFCTL_MQTT_DISCOVERY", requires = "mqtt_host")]
    mqtt_discovery: bool,

    /// QuickConnect account role (`contractor` or `consumer`).
    #[arg(long, env = "GAFCTL_QUICKCONNECT_ROLE", default_value = "contractor")]
    quickconnect_role: String,

    /// Enable QuickConnect settings writes. Disabled by default.
    #[arg(long, env = "GAFCTL_QUICKCONNECT_WRITES_ENABLED")]
    quickconnect_writes_enabled: bool,
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
    #[arg(long, default_value = "5")]
    scan_seconds: DeadlineSeconds,

    /// Seconds for GATT setup, command writes, and responses; platform calls allow at least 40s.
    #[arg(long, default_value = "3")]
    response_timeout_seconds: DeadlineSeconds,

    /// Print the raw identity response, which may contain a device identifier.
    #[arg(long)]
    show_identity: bool,

    /// Set automatic thresholds before reading state. Values are tenths:
    /// 1050 means 105.0°F and 300 means 30.0% humidity.
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
            scan_duration: Duration::from_secs(self.scan_seconds.get()),
            response_timeout: Duration::from_secs(self.response_timeout_seconds.get()),
            control_deadline: None,
            refresh_settings: false,
            mode,
        }
    }
}

pub(crate) async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    tracing::debug!("running CLI command");
    match cli.command {
        Some(Command::Completions { shell }) => {
            let mut completions = Vec::new();
            clap_complete::generate(
                shell,
                &mut Cli::command(),
                "gafctl-server",
                &mut completions,
            );
            crate::stdout::write_stdout(|output| output.write_all(&completions))?;
            Ok(ExitCode::SUCCESS)
        }
        None => cli.options.run().await.map(|()| ExitCode::SUCCESS),
        Some(Command::Probe(ProbeCommand {
            transport: ProbeTransport::Ble(options),
        })) => run_ble_probe(options).await.map(|()| ExitCode::SUCCESS),
    }
}

impl ServeOptions {
    async fn run(self) -> Result<()> {
        #[cfg(feature = "mqtt")]
        let mqtt_config = mqtt_config(
            self.mqtt_host,
            self.mqtt_port,
            self.mqtt_username,
            environment_value("GAFCTL_MQTT_PASSWORD")?,
            self.mqtt_discovery,
        )?;
        let quickconnect_config =
            read_quickconnect_config(&self.quickconnect_role, self.quickconnect_writes_enabled)?;
        crate::server::serve(ServerConfig {
            device_id: self.device_id,
            identity_store: self.identity_store,
            address: self.bind,
            allow_remote: self.allow_remote,
            #[cfg(feature = "mqtt")]
            mqtt_config,
            quickconnect_config,
        })
        .await
    }
}

async fn run_ble_probe(options: BleOptions) -> Result<()> {
    let show_identity = options.show_identity;
    let control_requested = options.requested_control().is_some();
    let result = probe(options.into_probe_options())
        .await
        .context("BLE probe failed")?;
    let control_confirmed = control_result_confirmed(control_requested, &result);
    crate::output::print_probe_result(result, show_identity)?;
    if !control_confirmed {
        bail!("requested control was not confirmed");
    }
    Ok(())
}

fn control_result_confirmed(
    control_requested: bool,
    result: &gafctl_bluetooth::ProbeResult,
) -> bool {
    let control = match result {
        gafctl_bluetooth::ProbeResult::Queried { result, .. } => result.control.as_ref(),
        gafctl_bluetooth::ProbeResult::NoDevices
        | gafctl_bluetooth::ProbeResult::Discovered { .. }
        | gafctl_bluetooth::ProbeResult::Ambiguous { .. }
        | gafctl_bluetooth::ProbeResult::DiscoveryIncomplete { .. } => None,
    };
    control_status_successful(control_requested, control)
}

fn control_status_successful(
    control_requested: bool,
    control: Option<&gafctl_protocol::ControlOutcome>,
) -> bool {
    !control_requested || control.is_some_and(gafctl_protocol::ControlOutcome::is_confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_accepts_ble_only_cloud_only_and_mixed_cli_modes() {
        [
            &["gafctl-server", "--device-id", "synthetic-ble-id"][..],
            &["gafctl-server"][..],
            &[
                "gafctl",
                "--device-id",
                "synthetic-ble-id",
                "--quickconnect-writes-enabled",
            ][..],
        ]
        .into_iter()
        .for_each(|args| assert!(Cli::try_parse_from(args).is_ok()));
    }

    #[test]
    fn diagnostic_probe_rejects_invalid_deadlines() {
        for flag in ["--scan-seconds", "--response-timeout-seconds"] {
            for value in ["0", "18446744073709551615"] {
                assert!(
                    Cli::try_parse_from(["gafctl-server", "probe", "ble", flag, value]).is_err(),
                    "{flag} must reject {value} before transport access"
                );
            }
        }
    }

    #[test]
    fn diagnostic_probe_preserves_default_and_explicit_deadlines() {
        [
            (&["gafctl-server", "probe", "ble"][..], 5, 3),
            (
                &[
                    "gafctl-server",
                    "probe",
                    "ble",
                    "--scan-seconds",
                    "7",
                    "--response-timeout-seconds",
                    "4",
                ][..],
                7,
                4,
            ),
        ]
        .into_iter()
        .for_each(|(args, scan_seconds, response_seconds)| {
            let Some(Command::Probe(ProbeCommand {
                transport: ProbeTransport::Ble(options),
            })) = Cli::try_parse_from(args).unwrap().command
            else {
                unreachable!("expected BLE probe command");
            };
            let options = options.into_probe_options();
            assert_eq!(options.scan_duration, Duration::from_secs(scan_seconds));
            assert_eq!(
                options.response_timeout,
                Duration::from_secs(response_seconds)
            );
        });
    }

    #[test]
    fn scan_only_rejects_control_settings() {
        [
            &[
                "gafctl",
                "probe",
                "ble",
                "--scan-only",
                "--set-timer-minutes",
                "1",
            ][..],
            &[
                "gafctl",
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
                "gafctl",
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
    #[cfg(feature = "mqtt")]
    fn serve_keeps_http_pull_and_allows_mqtt_push_together() {
        let cli = Cli::try_parse_from([
            "gafctl",
            "--device-id",
            "local-device-id",
            "--bind",
            "0.0.0.0:8787",
            "--allow-remote",
            "--mqtt-host",
            "192.168.1.5",
            "--mqtt-username",
            "gafctl",
            "--mqtt-discovery",
        ])
        .unwrap();

        let options = cli.options;
        assert_eq!(options.bind, "0.0.0.0:8787".parse().unwrap());
        assert!(options.allow_remote);
        assert_eq!(options.mqtt_host.as_deref(), Some("192.168.1.5"));
        assert!(options.mqtt_discovery);
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn empty_mqtt_host_environment_is_rejected() {
        let child = concat!(module_path!(), "::empty_mqtt_host_environment_child")
            .split_once("::")
            .unwrap()
            .1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", child, "--nocapture"])
            .env("GAFCTL_TEST_EMPTY_MQTT_HOST", "1")
            .env("GAFCTL_MQTT_HOST", "")
            .env("GAFCTL_MQTT_USERNAME", "gafctl")
            .env("GAFCTL_MQTT_PASSWORD", "test-secret")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "empty MQTT host was accepted: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn empty_mqtt_host_environment_child() {
        if std::env::var_os("GAFCTL_TEST_EMPTY_MQTT_HOST").is_none() {
            return;
        }

        assert!(Cli::try_parse_from(["gafctl-server", "--device-id", "local-device-id"]).is_err());
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn mqtt_password_is_not_in_cli_debug_output() {
        let child = concat!(module_path!(), "::mqtt_password_debug_child")
            .split_once("::")
            .unwrap()
            .1;
        [" test-secret ", ""].into_iter().for_each(|password| {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", child, "--nocapture"])
                .env("GAFCTL_TEST_MQTT_PASSWORD_DEBUG", "1")
                .env("GAFCTL_MQTT_HOST", "127.0.0.1")
                .env("GAFCTL_MQTT_USERNAME", "gafctl")
                .env("GAFCTL_MQTT_PASSWORD", password)
                .output()
                .unwrap();

            assert!(
                output.status.success(),
                "MQTT password validation or debug redaction failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        });
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn mqtt_password_debug_child() {
        if std::env::var_os("GAFCTL_TEST_MQTT_PASSWORD_DEBUG").is_none() {
            return;
        }

        let cli = Cli::try_parse_from(["gafctl-server", "--device-id", "local-device-id"]).unwrap();
        assert!(!format!("{cli:?}").contains("test-secret"));
        let password = environment_value("GAFCTL_MQTT_PASSWORD");
        if std::env::var("GAFCTL_MQTT_PASSWORD").unwrap().is_empty() {
            assert_eq!(
                password.unwrap_err().to_string(),
                "GAFCTL_MQTT_PASSWORD must not be empty"
            );
        } else {
            assert_eq!(password.unwrap(), Some(" test-secret ".into()));
        }
    }

    #[test]
    fn only_requested_controls_require_a_queried_device() {
        [
            gafctl_bluetooth::ProbeResult::NoDevices,
            gafctl_bluetooth::ProbeResult::Discovered {
                devices: Vec::new(),
            },
            gafctl_bluetooth::ProbeResult::Ambiguous {
                devices: Vec::new(),
            },
            gafctl_bluetooth::ProbeResult::DiscoveryIncomplete {
                devices: Vec::new(),
                failures: Vec::new(),
            },
        ]
        .iter()
        .for_each(|result| {
            assert!(control_result_confirmed(false, result));
            assert!(!control_result_confirmed(true, result));
        });
    }

    #[test]
    fn control_status_requires_acknowledgement_and_matching_readback() {
        [
            (None, false),
            (
                Some(timer_control_outcome(b"#tmr1\n", b"#ttr00010002\n")),
                false,
            ),
            (
                Some(timer_control_outcome(b"#tmr0\n", b"#ttr00010003\n")),
                false,
            ),
            (
                Some(timer_control_outcome(b"#tmr0\n", b"#ttr00010002\n")),
                true,
            ),
        ]
        .into_iter()
        .for_each(|(control, confirmed)| {
            assert_eq!(control_status_successful(true, control.as_ref()), confirmed);
            assert!(control_status_successful(false, control.as_ref()));
        });
    }

    fn timer_control_outcome(
        acknowledgement: &'static [u8],
        timer: &'static [u8],
    ) -> gafctl_protocol::ControlOutcome {
        let snapshot = timer_snapshot(timer);
        gafctl_protocol::ControlOutcome::from_response(
            ControlCommand::SetTimer(Minutes::new(2)),
            protocol_frame(acknowledgement),
            Some(&snapshot),
        )
        .unwrap()
    }

    fn timer_snapshot(timer: &'static [u8]) -> gafctl_protocol::DeviceSnapshot {
        let [identity, mode, sensors, thresholds, timer] = [
            b"#idr030000\n".as_slice(),
            b"#dmrtn\n",
            b"#sdr03CA00AA\n",
            b"#atr041a012c\n",
            timer,
        ]
        .map(protocol_frame);
        gafctl_protocol::DeviceSnapshot::from_frames(identity, mode, sensors, thresholds, timer)
            .unwrap()
    }

    fn protocol_frame(payload: &'static [u8]) -> gafctl_protocol::Frame<'static> {
        gafctl_protocol::Frame::from_bytes(bytes::Bytes::from_static(payload)).unwrap()
    }
}
