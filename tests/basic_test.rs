mod echo;
use echo::Echo;
use rmcp::{
    ServiceExt,
    transport::{ConfigureCommandExt, SseServer, TokioChildProcess},
};

const BIND_ADDRESS: &str = "127.0.0.1:8099";
const TEST_SERVER_URL: &str = "http://localhost:8099/sse";

#[tokio::test]
async fn test_proxy_connects_to_real_server() -> anyhow::Result<()> {
    let ct = SseServer::serve(BIND_ADDRESS.parse()?)
        .await?
        .with_service_directly(Echo::new);

    let transport = TokioChildProcess::new(
        tokio::process::Command::new("./target/debug/mcp-proxy").configure(|cmd| {
            cmd.arg(TEST_SERVER_URL);
        }),
    )?;

    let client = ().serve(transport).await?;
    let tools = client.list_all_tools().await?;

    // assert that the echo tool is available
    assert!(tools.iter().any(|t| t.name == "echo"));

    if let Some(echo_tool) = tools.iter().find(|t| t.name.contains("echo")) {
        let result = client
            .call_tool(rmcp::model::CallToolRequestParam {
                name: echo_tool.name.clone(),
                arguments: Some(rmcp::object!({
                    "message": "Hello, world!"
                })),
            })
            .await?;

        // Assert that the result contains our expected text
        let result_debug = format!("{:?}", result);
        assert!(
            result_debug.contains("Hello, world!"),
            "Expected result to contain 'Hello, world!', but got: {}",
            result_debug
        );

        println!("Result: {}", result_debug);
    } else {
        assert!(false, "No echo tool found");
    }

    // Properly shutdown the client and kill the child process
    drop(client);
    // Wait for the server to shut down
    ct.cancel();

    Ok(())
}
