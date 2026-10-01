mod api;
pub mod backend;
mod cli;
mod cli_ble;
mod cli_client;
pub mod control;
pub mod device;
mod logging;
mod mqtt;
mod output;
pub mod quickconnect_control;

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    logging::init();
    finish(cli::run().await)
}

fn finish(result: anyhow::Result<ExitCode>) -> ExitCode {
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

#[cfg(test)]
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
            let result = output::write_stdout(|_| Err(io::Error::from(kind)))
                .map(|()| ExitCode::SUCCESS)
                .map_err(|error| error.context("write command output"));
            assert_eq!(finish(result), expected);
        }
    }
}
