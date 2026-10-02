use std::time::Duration;
use std::{net::SocketAddr, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, CommandFactory, Parser, Subcommand};
use gafctl_bluetooth::{ProbeMode, ProbeOptions, probe};
use gafctl_protocol::{
    AutomaticThresholds, ControlCommand, HumidityTenthsPercent, Minutes, TemperatureTenthsF,
};
use gafctl_quickconnect::{AccountRole, Credentials};
use std::process::ExitCode;

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

pub(crate) struct QuickConnectRuntimeConfig {
    pub(crate) credentials: Credentials,
    pub(crate) account_id: String,
    pub(crate) writes_enabled: bool,
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

    /// Seconds for GATT setup, command writes, and responses; platform calls allow at least 40s.
    #[arg(long, default_value_t = 3)]
    response_timeout_seconds: u64,

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
            scan_duration: Duration::from_secs(self.scan_seconds),
            response_timeout: Duration::from_secs(self.response_timeout_seconds),
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
            crate::output::write_stdout(|output| output.write_all(&completions))?;
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
            read_mqtt_password()?,
            self.mqtt_discovery,
        )?;
        let quickconnect_config =
            read_quickconnect_config(&self.quickconnect_role, self.quickconnect_writes_enabled)?;
        ensure_quickconnect_identity_store(
            quickconnect_config.is_some(),
            self.identity_store.as_deref(),
        )?;
        crate::api::serve(
            self.device_id,
            self.identity_store,
            self.bind,
            self.allow_remote,
            #[cfg(feature = "mqtt")]
            mqtt_config,
            quickconnect_config,
        )
        .await
    }
}

fn read_quickconnect_config(
    role: &str,
    writes_enabled: bool,
) -> Result<Option<QuickConnectRuntimeConfig>> {
    let username = environment_value("GAFCTL_QUICKCONNECT_USERNAME")?;
    let password = environment_value("GAFCTL_QUICKCONNECT_PASSWORD")?;
    let password_file = environment_value("GAFCTL_QUICKCONNECT_PASSWORD_FILE")?.map(PathBuf::from);
    quickconnect_config_from(username, password, password_file, role, writes_enabled)
}

fn environment_value(name: &str) -> Result<Option<String>> {
    match std::env::var(name) {
        Ok(value) if value.is_empty() => bail!("{name} must not be empty"),
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => bail!("{name} must be valid UTF-8"),
    }
}

fn ensure_quickconnect_identity_store(
    quickconnect_enabled: bool,
    identity_store: Option<&std::path::Path>,
) -> Result<()> {
    anyhow::ensure!(
        !quickconnect_enabled || identity_store.is_some(),
        "QuickConnect requires --identity-store or GAFCTL_IDENTITY_STORE"
    );
    Ok(())
}

fn quickconnect_config_from(
    username: Option<String>,
    password: Option<String>,
    password_file: Option<PathBuf>,
    role: &str,
    writes_enabled: bool,
) -> Result<Option<QuickConnectRuntimeConfig>> {
    let role = match role {
        "contractor" => AccountRole::Contractor,
        "consumer" => AccountRole::Consumer,
        _ => bail!("GAFCTL_QUICKCONNECT_ROLE must be contractor or consumer"),
    };
    let configured = username.is_some() || password.is_some() || password_file.is_some();
    if !configured {
        anyhow::ensure!(
            !writes_enabled,
            "QuickConnect writes require account credentials"
        );
        return Ok(None);
    }
    let Some(username) = username
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        bail!("QuickConnect username and one password source are required")
    };
    anyhow::ensure!(
        password.is_none() || password_file.is_none(),
        "configure only one QuickConnect password source"
    );
    let password = match (password, password_file) {
        (Some(password), None) if !password.is_empty() => password,
        (None, Some(path)) => read_private_secret(&path)?,
        _ => bail!("QuickConnect username and one password source are required"),
    };
    let role_name = match role {
        AccountRole::Contractor => "contractor",
        AccountRole::Consumer => "consumer",
    };
    Ok(Some(QuickConnectRuntimeConfig {
        credentials: Credentials::new(username.clone(), password, role),
        account_id: format!("{role_name}:{username}"),
        writes_enabled,
    }))
}

fn read_private_secret(path: &std::path::Path) -> Result<String> {
    let metadata = std::fs::metadata(path).context("could not read QuickConnect password file")?;
    anyhow::ensure!(
        metadata.is_file(),
        "QuickConnect password path is not a file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            metadata.permissions().mode() & 0o077 == 0,
            "QuickConnect password file must not be accessible by group or others"
        );
    }
    let password =
        std::fs::read_to_string(path).context("could not read QuickConnect password file")?;
    let password = password.strip_suffix('\n').unwrap_or(&password);
    let password = password.strip_suffix('\r').unwrap_or(password);
    anyhow::ensure!(!password.is_empty(), "QuickConnect password file is empty");
    Ok(password.to_owned())
}

#[cfg(feature = "mqtt")]
fn read_mqtt_password() -> Result<Option<String>> {
    match std::env::var("GAFCTL_MQTT_PASSWORD") {
        Ok(password) => Ok(Some(password)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            bail!("GAFCTL_MQTT_PASSWORD must be valid UTF-8")
        }
    }
}

#[cfg(feature = "mqtt")]
fn mqtt_config(
    host: Option<String>,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    discovery_enabled: bool,
) -> Result<Option<crate::mqtt::MqttConfig>> {
    match (host, username, password) {
        (Some(host), Some(username), Some(password))
            if !host.is_empty() && !username.is_empty() && !password.is_empty() =>
        {
            Ok(Some(crate::mqtt::MqttConfig {
                host,
                port,
                username,
                password,
                discovery_enabled,
            }))
        }
        (None, None, None) if !discovery_enabled => Ok(None),
        (None, None, None) => bail!("MQTT discovery requires MQTT broker credentials"),
        _ => bail!("MQTT host, username, and GAFCTL_MQTT_PASSWORD must be configured together"),
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
    fn serve_can_start_without_a_ble_device_identifier() {
        assert!(
            Cli::try_parse_from(["gafctl-server"]).is_ok(),
            "serving without a BLE backend must be a valid startup mode"
        );
    }

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
    fn quickconnect_is_optional_without_credentials() {
        assert!(
            quickconnect_config_from(None, None, None, "contractor", false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn quickconnect_requires_a_persistent_identity_store() {
        assert!(ensure_quickconnect_identity_store(true, None).is_err());
        assert!(ensure_quickconnect_identity_store(false, None).is_ok());
        assert!(
            ensure_quickconnect_identity_store(true, Some(std::path::Path::new("identities.json")))
                .is_ok()
        );
    }

    #[test]
    fn quickconnect_rejects_partial_or_ambiguous_credentials() {
        [
            quickconnect_config_from(Some("account".into()), None, None, "contractor", false),
            quickconnect_config_from(None, Some("secret".into()), None, "contractor", false),
            quickconnect_config_from(
                Some("account".into()),
                Some("secret".into()),
                Some(PathBuf::from("/secret/file")),
                "contractor",
                false,
            ),
            quickconnect_config_from(None, None, None, "contractor", true),
        ]
        .into_iter()
        .for_each(|result| assert!(result.is_err()));
    }

    #[test]
    fn quickconnect_uses_private_password_file_and_keeps_writes_disabled_by_default() {
        use std::os::unix::fs::PermissionsExt;

        let password_path = std::env::temp_dir().join(format!(
            "gafctl-quickconnect-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&password_path, "private-token\n").unwrap();
        std::fs::set_permissions(&password_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let config = quickconnect_config_from(
            Some(" account ".into()),
            None,
            Some(password_path.clone()),
            "consumer",
            false,
        )
        .unwrap()
        .unwrap();

        std::fs::remove_file(password_path).unwrap();
        assert!(!config.writes_enabled);
        assert_eq!(config.account_id, "consumer:account");
        assert!(!format!("{:?}", config.credentials).contains("private-token"));
    }

    #[test]
    fn quickconnect_password_files_reject_group_or_other_access() {
        use std::os::unix::fs::PermissionsExt;

        let password_path = std::env::temp_dir().join(format!(
            "gafctl-quickconnect-open-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&password_path, "private-token").unwrap();
        std::fs::set_permissions(&password_path, std::fs::Permissions::from_mode(0o640)).unwrap();

        let result = quickconnect_config_from(
            Some("account".into()),
            None,
            Some(password_path.clone()),
            "consumer",
            false,
        );

        std::fs::remove_file(password_path).unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn quickconnect_writes_require_a_separate_explicit_gate() {
        let config = quickconnect_config_from(
            Some("account".into()),
            Some("private-token".into()),
            None,
            "contractor",
            true,
        )
        .unwrap()
        .unwrap();

        assert!(config.writes_enabled);
        assert!(!format!("{:?}", config.credentials).contains("private-token"));
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
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::tests::empty_mqtt_host_environment_child",
                "--nocapture",
            ])
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
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::tests::mqtt_password_debug_child",
                "--nocapture",
            ])
            .env("GAFCTL_TEST_MQTT_PASSWORD_DEBUG", "1")
            .env("GAFCTL_MQTT_HOST", "127.0.0.1")
            .env("GAFCTL_MQTT_USERNAME", "gafctl")
            .env("GAFCTL_MQTT_PASSWORD", "test-secret")
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "MQTT password appeared in CLI debug output: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn mqtt_password_debug_child() {
        if std::env::var_os("GAFCTL_TEST_MQTT_PASSWORD_DEBUG").is_none() {
            return;
        }

        let cli = Cli::try_parse_from(["gafctl-server", "--device-id", "local-device-id"]).unwrap();
        assert!(!format!("{cli:?}").contains("test-secret"));
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn serve_requires_both_mqtt_credentials_when_push_is_enabled() {
        assert!(
            mqtt_config(
                Some("192.168.1.5".into()),
                1883,
                Some("gafctl".into()),
                None,
                false,
            )
            .is_err()
        );
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn mqtt_discovery_cannot_be_enabled_without_broker_credentials() {
        assert!(mqtt_config(None, 1883, None, None, true).is_err());
        assert!(
            mqtt_config(None, 1883, None, None, false)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    #[cfg(feature = "mqtt")]
    fn mqtt_discovery_can_be_selected_with_broker_credentials() {
        let config = mqtt_config(
            Some("192.168.1.5".into()),
            1883,
            Some("gafctl".into()),
            Some("secret".into()),
            true,
        )
        .unwrap()
        .unwrap();

        assert!(config.discovery_enabled);
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
