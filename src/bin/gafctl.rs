#[path = "../cli_ble.rs"]
mod cli_ble;
#[path = "../cli_client.rs"]
mod cli_client;
#[path = "../control_display.rs"]
mod control_display;
#[path = "../logging.rs"]
mod logging;
#[path = "../stdout.rs"]
mod output;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use cli_client::{BleCommand, ControlOptions, ServiceOptions, StateOptions};
use std::process::ExitCode;
use std::{ffi::OsString, path::PathBuf, process::Command as ProcessCommand};

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

#[tokio::main]
async fn main() -> ExitCode {
    logging::init();
    finish(run().await)
}

async fn run() -> Result<ExitCode> {
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
            output::write_stdout(|stdout| stdout.write_all(&completions))?;
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

fn finish(result: Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error)
            if error
                .downcast_ref::<output::StdoutError>()
                .is_some_and(output::StdoutError::is_broken_pipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}
