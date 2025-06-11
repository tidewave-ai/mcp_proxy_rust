use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// The URL of the SSE endpoint to connect to
    #[arg(value_name = "URL")]
    pub sse_url: Option<String>,

    /// Enable debug logging
    #[arg(long)]
    pub debug: bool,

    /// Maximum time to try reconnecting in seconds
    #[arg(long)]
    pub max_disconnected_time: Option<u64>,

    /// Initial retry interval in seconds. Default is 5 seconds
    #[arg(long, default_value = "5")]
    pub initial_retry_interval: u64,

    /// Enable protocol version rewriting (2025-03-26 -> 2024-11-05)
    #[arg(long)]
    pub rewrite_protocol_version: bool,
}
