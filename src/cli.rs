use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// The URL of the SSE endpoint to connect to
    #[arg(value_name = "URL", help="The URL of the SSE endpoint to connect to")]
    pub sse_url: Option<String>,

    /// Enable debug logging
    #[arg(long, help="Enable debug logging")]
    pub debug: bool,

    /// Maximum time to try reconnecting in seconds
    #[arg(long, help="Maximum time to try reconnecting in seconds")]
    pub max_disconnected_time: Option<u64>,

    /// Initial retry interval in seconds. Default is 5 seconds
    #[arg(long, default_value = "5", help="Initial retry interval in seconds. Default is 5 seconds")]
    pub initial_retry_interval: u64,

    #[arg(long, help="Override the protocol version returned to the client")]
    /// Override the protocol version returned to the client
    pub override_protocol_version: Option<String>,
}
