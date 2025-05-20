use anyhow::Context;
use clap::Parser;
use rmcp::transport::SseServer;
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

    let ct = SseServer::serve(args.address)
        .await?
        .with_service(Echo::default);

    tokio::signal::ctrl_c().await?;
    ct.cancel();
    Ok(())
}
