use anyhow::Context;
use clap::Parser;
use rmcp::transport::SseServer;
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

    let ct = SseServer::serve(args.address)
        .await?
        .with_service_directly(Echo::new);

    tokio::signal::ctrl_c().await?;
    ct.cancel();
    Ok(())
}
