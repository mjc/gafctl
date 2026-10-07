mod ble;
mod output;
mod service;

use std::{ffi::OsString, path::PathBuf, process::Command as ProcessCommand, process::ExitCode};

use crate::model::ControlPreset;
use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};

use ble::BleCommand;
use service::{ControlOptions, ServiceOptions, StateOptions};

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

#[derive(Debug, Parser)]
#[command(
    name = "gafctl",
    version,
    about = "Control GAF attic fans through Gafctl or Bluetooth"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start gafctl-server, forwarding its arguments.
    #[command(disable_help_flag = true, disable_help_subcommand = true)]
    Server {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// List registered devices on a running Gafctl service.
    Devices(ServiceOptions),
    /// Read a device's cached service snapshot, including availability and timestamps.
    State(StateOptions),
    /// Send a supported control through the running service.
    Control(ControlOptions),
    /// Scan, read, or control a fan directly over Bluetooth.
    Ble(BleCommand),
    /// Generate shell completions without connecting to any transport.
    Completions { shell: clap_complete::Shell },
}

pub(crate) async fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    tracing::debug!("running control CLI command");
    match cli.command {
        Command::Server { args } => launch_server(&args),
        Command::Devices(options) => options.devices().await,
        Command::State(options) => options.run().await,
        Command::Control(options) => options.run().await,
        Command::Ble(options) => options.run().await,
        Command::Completions { shell } => {
            let mut completions = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "gafctl", &mut completions);
            crate::stdout::write_stdout(|stdout| stdout.write_all(&completions))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn server_executable() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|executable| {
            executable.parent().map(|directory| {
                directory.join(format!("gafctl-server{}", std::env::consts::EXE_SUFFIX))
            })
        })
        .filter(|executable| executable.is_file())
        .unwrap_or_else(|| PathBuf::from("gafctl-server"))
}

fn launch_server(args: &[OsString]) -> Result<ExitCode> {
    let mut command = ProcessCommand::new(server_executable());
    command.args(args);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec())
            .map_err(|error| anyhow::anyhow!("could not launch gafctl-server: {error}"))
    }
    #[cfg(not(unix))]
    {
        let status = command.status()?;
        Ok(ExitCode::from(
            status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
