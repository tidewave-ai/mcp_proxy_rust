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
    #[allow(dead_code)]
    pub fn new() -> Self {
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
