use anyhow::{Context, Result, anyhow};
use clap::Parser;
use futures::StreamExt;
use rmcp::{
    model::{ClientJsonRpcMessage, ErrorCode, ServerJsonRpcMessage},
    transport::{StreamableHttpClientTransport, Transport, sse_client::SseClientTransport},
};
use std::env;
use tokio::io::{Stdin, Stdout};
use tokio::time::{Duration, Instant, sleep};
use tokio_util::codec::{FramedRead, FramedWrite};
use tracing::{debug, error, info, warn};
use tracing_subscriber::FmtSubscriber;

// Modules
mod cli;
mod core;
mod state;

use crate::cli::Args;
use crate::core::{connect, flush_buffer_with_errors};
use crate::state::{AppState, ProxyState}; // Only needed directly by main for final check

// Custom Error Codes (Keep here or move to common/state? Keeping here for now)
const DISCONNECTED_ERROR_CODE: ErrorCode = ErrorCode(-32010);
const TRANSPORT_SEND_ERROR_CODE: ErrorCode = ErrorCode(-32011);

enum SseClientType {
    Sse(SseClientTransport<reqwest::Client>),
    Streamable(StreamableHttpClientTransport<reqwest::Client>),
}

impl SseClientType {
    async fn send(
        &mut self,
        item: ClientJsonRpcMessage,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
        match self {
            SseClientType::Sse(transport) => transport.send(item).await.map_err(|e| e.into()),
            SseClientType::Streamable(transport) => {
                transport.send(item).await.map_err(|e| e.into())
            }
        }
    }

    async fn receive(&mut self) -> Option<ServerJsonRpcMessage> {
        match self {
            SseClientType::Sse(transport) => transport.receive().await,
            SseClientType::Streamable(transport) => transport.receive().await,
        }
    }
}

type StdinCodec = rmcp::transport::async_rw::JsonRpcMessageCodec<ClientJsonRpcMessage>;
type StdoutCodec = rmcp::transport::async_rw::JsonRpcMessageCodec<ServerJsonRpcMessage>;
type StdinStream = FramedRead<Stdin, StdinCodec>;
type StdoutSink = FramedWrite<Stdout, StdoutCodec>;

// --- Helper for Initial Connection ---
const INITIAL_CONNECT_TIMEOUT: Duration = Duration::from_secs(5 * 60); // 5 minutes

/// Attempts to establish the initial SSE connection, retrying on failure.
async fn connect_with_retry(app_state: &AppState, delay: Duration) -> Result<SseClientType> {
    let start_time = Instant::now();
    let mut attempts = 0;

    loop {
        attempts += 1;
        info!(
            "Attempting initial SSE connection (attempt {})...",
            attempts
        );

        let result = connect(app_state).await;

        // Try creating the transport
        match result {
            Ok(transport) => {
                info!("Initial connection successful!");
                return Ok(transport);
            }
            Err(e) => {
                warn!("Attempt {} failed to start transport: {}", attempts, e);
            }
        }

        if start_time.elapsed() >= INITIAL_CONNECT_TIMEOUT {
            error!(
                "Failed to connect after {} attempts over {:?}. Giving up.",
                attempts, INITIAL_CONNECT_TIMEOUT
            );
            return Err(anyhow!(
                "Initial connection timed out after {:?}",
                INITIAL_CONNECT_TIMEOUT
            ));
        }

        info!("Retrying in {:?}...", delay);
        sleep(delay).await;
    }
}

// --- Main Function ---
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
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

    // Get the SSE URL from args or environment
    let sse_url = match args.sse_url {
          Some(url) => url,
          None => env::var("SSE_URL").context(
              "Either the URL must be passed as the first argument or the SSE_URL environment variable must be set",
          )?,
      };

    debug!("Starting MCP proxy with URL: {}", sse_url);
    debug!("Max disconnected time: {:?}s", args.max_disconnected_time);

    // Set up communication channels
    let (reconnect_tx, mut reconnect_rx) = tokio::sync::mpsc::channel(10);
    let (timer_tx, mut timer_rx) = tokio::sync::mpsc::channel(10);

    // Initialize application state
    let mut app_state = AppState::new(sse_url.clone(), args.max_disconnected_time);
    // Pass channel senders to state
    app_state.reconnect_tx = Some(reconnect_tx.clone());
    app_state.timer_tx = Some(timer_tx.clone());

    // Establish initial SSE connection using the retry helper
    info!("Attempting initial connection to {}...", sse_url);
    let mut transport =
        connect_with_retry(&app_state, Duration::from_secs(args.initial_retry_interval)).await?;

    info!("Connection established. Proxy operational.");
    app_state.state = ProxyState::WaitingForClientInit;

    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let mut stdin_stream: StdinStream = FramedRead::new(stdin, StdinCodec::default());
    let mut stdout_sink: StdoutSink = FramedWrite::new(stdout, StdoutCodec::default());

    info!("Connected to SSE endpoint, starting proxy");

    // Set up heartbeat interval
    let mut heartbeat_interval = tokio::time::interval(Duration::from_secs(1));

    // Main event loop
    loop {
        tokio::select! {
            // Bias select towards checking cheaper/more frequent events first if needed,
            // but default Tokio select is fair.
            biased;
            // Handle message from stdin
            msg = stdin_stream.next() => {
                if !app_state.handle_stdin_message(msg, &mut transport, &mut stdout_sink).await? {
                    break;
                }
            }
            // Handle message from SSE server
            result = transport.receive(), if app_state.transport_valid => {
                if !app_state.handle_sse_message(result, &mut transport, &mut stdout_sink).await? {
                    break;
                }
            }
            // Handle reconnect signal
            Some(_) = reconnect_rx.recv() => {
                // Call method on app_state
                if let Some(new_transport) = app_state.handle_reconnect_signal(&mut stdout_sink).await? {
                    transport = new_transport;
                }
                // Check if disconnected too long *after* attempting reconnect
                if app_state.disconnected_too_long() {
                    error!("Giving up after failed reconnection attempts and exceeding max disconnected time.");
                    // Ensure buffer is flushed if not already done by handle_reconnect_signal
                    if !app_state.in_buf.is_empty() && app_state.buf_mode == state::BufferMode::Store {
                        flush_buffer_with_errors(&mut app_state, &mut stdout_sink).await?;
                    }
                    break;
                }
            }
            // Handle flush timer signal
            Some(_) = timer_rx.recv() => app_state.handle_timer_signal(&mut stdout_sink).await?,
            // Handle heartbeat tick
            _ = heartbeat_interval.tick() => app_state.handle_heartbeat_tick(&mut transport).await?,
            // Exit if no events are ready (shouldn't happen with interval timers unless others close)
            else => break,
        }
    }

    info!("Proxy terminated");
    Ok(())
}
