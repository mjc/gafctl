mod api;
mod cli;
mod logging;
mod mqtt;
mod output;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();
    cli::run().await
}
