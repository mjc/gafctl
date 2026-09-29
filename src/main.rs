mod cli;
#[cfg(feature = "heap-track")]
mod heap_track;
mod output;

use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    cli::run().await
}
