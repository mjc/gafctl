use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    gafctl::run_cli().await
}
