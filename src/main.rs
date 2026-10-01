mod api;
pub mod backend;
mod cli;
pub mod control;
pub mod device;
mod logging;
mod mqtt;
mod output;
pub mod quickconnect_control;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();
    cli::run().await
}
