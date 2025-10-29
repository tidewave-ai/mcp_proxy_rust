use clap::Parser;
use reqwest::header::HeaderMap;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Args {
    /// The URL of the SSE endpoint to connect to
    #[arg(value_name = "URL", env = "SSE_URL")]
    pub sse_url: String,

    /// Enable debug logging
    #[arg(long)]
    pub debug: bool,

    /// Maximum time to try reconnecting in seconds
    #[arg(long)]
    pub max_disconnected_time: Option<u64>,

    /// Initial retry interval in seconds. Default is 5 seconds
    #[arg(long, default_value = "5")]
    pub initial_retry_interval: u64,

    #[arg(long, env = "MCP_HEADERS", value_parser = parse_header_map)]
    /// Headers to send to the server
    /// This is a JSON object of header name to header value.
    /// Example: `{"Authorization": "Bearer 1234567890"}`
    pub headers: Option<HeaderMap>,

    #[arg(long)]
    /// Override the protocol version returned to the client
    pub override_protocol_version: Option<String>,
}

fn parse_header_map(s: &str) -> Result<Option<HeaderMap>, String> {
    let headers = {
        let mut de = serde_json::Deserializer::from_str(s);
        http_serde::option::header_map::deserialize(&mut de).map_err(|e| e.to_string())?
    };
    Ok(headers)
}
