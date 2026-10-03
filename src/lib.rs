//! Concrete CLI and service application entrypoints.

#[cfg(feature = "http")]
mod api;
#[cfg(any(feature = "cli", feature = "http"))]
mod arguments;
#[cfg(feature = "http")]
mod backend;
#[cfg(feature = "cli")]
mod cli;
#[cfg(any(feature = "cli", feature = "http"))]
mod control_display;
#[cfg(feature = "http")]
mod legacy_control;
#[cfg(any(feature = "cli", feature = "http"))]
mod legacy_projection;
#[cfg(any(feature = "cli", feature = "http"))]
mod logging;
#[cfg(feature = "mqtt")]
mod mqtt;
#[cfg(feature = "http")]
mod output;
#[cfg(feature = "http")]
mod server;
#[cfg(feature = "http")]
mod service;
#[cfg(any(feature = "cli", feature = "http"))]
mod stdout;
#[cfg(all(test, feature = "http"))]
mod test_support;

#[cfg(any(feature = "cli", feature = "http"))]
use std::process::ExitCode;

/// Run the administrative CLI with its command-line arguments.
#[cfg(feature = "cli")]
pub async fn run_cli() -> ExitCode {
    logging::init();
    finish(cli::run().await)
}

/// Run the service or its diagnostic command with its command-line arguments.
#[cfg(feature = "http")]
pub async fn run_server() -> ExitCode {
    logging::init();
    finish(server::cli::run().await)
}

#[cfg(any(feature = "cli", feature = "http"))]
fn finish(result: anyhow::Result<ExitCode>) -> ExitCode {
    match result {
        Ok(code) => code,
        Err(error)
            if error
                .downcast_ref::<stdout::StdoutError>()
                .is_some_and(stdout::StdoutError::is_broken_pipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, any(feature = "cli", feature = "http")))]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn nested_backend_broken_pipe_remains_a_failure() {
        let error = anyhow::Error::new(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "backend request write failed",
        ))
        .context("send BLE request")
        .context("BLE probe failed");
        assert_eq!(finish(Err(error)), ExitCode::FAILURE);
    }

    #[test]
    fn only_a_closed_stdout_pipe_is_a_clean_output_exit() {
        for (kind, expected) in [
            (io::ErrorKind::BrokenPipe, ExitCode::SUCCESS),
            (io::ErrorKind::PermissionDenied, ExitCode::FAILURE),
        ] {
            let result = stdout::write_stdout(|_| Err(io::Error::from(kind)))
                .map(|()| ExitCode::SUCCESS)
                .map_err(|error| error.context("write command output"));
            assert_eq!(finish(result), expected);
        }
    }
}
