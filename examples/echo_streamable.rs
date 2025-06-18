use anyhow::Context;
use clap::Parser;
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use tracing_subscriber::FmtSubscriber;

use rmcp::{
    ServerHandler,
    model::{ServerCapabilities, ServerInfo},
    schemars, tool,
};
#[derive(Debug, Clone, Default)]
pub struct Echo;
#[tool(tool_box)]
impl Echo {
    pub fn new() -> Self {
        Self {}
    }

    #[tool(description = "Echo a message")]
    fn echo(&self, #[tool(param)] message: String) -> String {
        message
    }
}

#[tool(tool_box)]
impl ServerHandler for Echo {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some("A simple echo server".into()),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Address to bind the server to
    #[arg(short, long, default_value = "127.0.0.1:8080")]
    address: std::net::SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .finish();

    // Parse command line arguments
    let args = Args::parse();

    tracing::subscriber::set_global_default(subscriber).context("Failed to set up logging")?;

    let service = StreamableHttpService::new(
        || Ok(Echo::new()),
        LocalSessionManager::default().into(),
        Default::default(),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let tcp_listener = tokio::net::TcpListener::bind(args.address).await?;
    let _ = axum::serve(tcp_listener, router)
        .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.unwrap() })
        .await;

    Ok(())
}
