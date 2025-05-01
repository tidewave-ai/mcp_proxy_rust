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
