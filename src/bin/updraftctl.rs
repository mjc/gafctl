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

#[derive(Debug, Parser)]
#[command(
    name = "updraftctl",
    version,
    about = "Control GAF attic fans through Updraft or Bluetooth"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List registered devices on a running Updraft service.
    Devices(ServiceOptions),
    /// Read a device's cached service snapshot, including availability and timestamps.
    State(StateOptions),
    /// Issue a supported control through the running service's transaction owner.
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
        Command::Devices(options) => options.devices().await,
        Command::State(options) => options.run().await,
        Command::Control(options) => options.run().await,
        Command::Ble(options) => options.run().await,
        Command::Completions { shell } => {
            let mut completions = Vec::new();
            clap_complete::generate(shell, &mut Cli::command(), "updraftctl", &mut completions);
            output::write_stdout(|stdout| stdout.write_all(&completions))?;
            Ok(ExitCode::SUCCESS)
        }
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
