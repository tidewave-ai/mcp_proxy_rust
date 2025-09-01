use anyhow::Context;
use clap::Parser;
use rmcp::transport::streamable_http_server::{
    StreamableHttpService, session::local::LocalSessionManager,
};
use tracing_subscriber::FmtSubscriber;

use rmcp::{
    ErrorData as McpError,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};

#[derive(Debug, Clone, Default)]
pub struct Echo {
    tool_router: ToolRouter<Echo>,
}
#[tool_router]
impl Echo {
    fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    #[tool(description = "Echo a message")]
    fn echo(&self, Parameters(object): Parameters<JsonObject>) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::Value::Object(object).to_string(),
        )]))
    }
}

#[tool_handler]
impl rmcp::ServerHandler for Echo {
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
