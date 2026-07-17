//! Binary entrypoint for the `cctp` command.

use eyre::Result;

#[tokio::main]
async fn main() -> Result<()> {
    cctp::run().await
}
