mod models;
mod proxy;
mod sse;

use anyhow::{Context, Result};
use clap::Parser;
use std::env;
use std::io::Write;
use tracing::{debug, info};
use tracing_subscriber::FmtSubscriber;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// The URL of the SSE endpoint to connect to
    #[arg(value_name = "URL")]
    sse_url: Option<String>,

    /// Enable debug logging
    #[arg(long)]
    debug: bool,

    /// Maximum time to try reconnecting in seconds
    #[arg(long)]
    max_disconnected_time: Option<u64>,

    /// Maximum time to wait for a response from the server in milliseconds
    #[arg(long, default_value = "60000")]
    receive_timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Set up logging
    let log_level = if args.debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    let subscriber = FmtSubscriber::builder()
        .with_max_level(log_level)
        .with_writer(std::io::stderr)
        .finish();

    tracing::subscriber::set_global_default(subscriber).context("Failed to set up logging")?;

    // Ensure logs are flushed immediately
    std::io::stderr().flush().ok();

    // Get the SSE URL from args or environment
    let sse_url = match args.sse_url {
        Some(url) => url,
        None => env::var("SSE_URL").context(
            "Either the URL must be passed as the first argument or the SSE_URL environment variable must be set",
        )?,
    };

    debug!("Starting MCP proxy with URL: {}", sse_url);
    debug!("Max disconnected time: {:?}s", args.max_disconnected_time);
    debug!("Receive timeout: {}ms", args.receive_timeout);

    info!("Starting MCP proxy");
    proxy::start_proxy(sse_url, args.max_disconnected_time, args.receive_timeout).await
}
