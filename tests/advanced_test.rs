mod echo;
use anyhow::Result;
use rmcp::{
    ServiceExt,
    model::CallToolRequestParam,
    object,
    transport::{SseServer, TokioChildProcess},
};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::sleep,
};

// Creates a new SSE server for testing
async fn create_sse_server(
    address: SocketAddr,
) -> Result<(tokio_util::sync::CancellationToken, String)> {
    let url = format!("http://{}/sse", address);
    let ct = tokio_util::sync::CancellationToken::new();

    tracing::info!("Creating SSE server at {}", url);

    let config = rmcp::transport::sse_server::SseServerConfig {
        bind: address,
        sse_path: "/sse".to_string(),
        post_path: "/message".to_string(),
        ct: ct.clone(),
        sse_keep_alive: None,
    };

    let (sse_server, router) = SseServer::new(config);

    // Bind the listener for the server
    let listener = tokio::net::TcpListener::bind(sse_server.config.bind).await?;
    tracing::debug!("SSE server bound to {}", sse_server.config.bind);

    // Create a child token for cancellation
    let child_ct = sse_server.config.ct.child_token();

    // Spawn the server task with graceful shutdown
    let server = axum::serve(listener, router).with_graceful_shutdown(async move {
        tracing::info!("Waiting for cancellation signal...");
        child_ct.cancelled().await;
        tracing::info!("SSE server cancelled");
    });

    tracing::debug!("Starting SSE server task");
    tokio::spawn(async move {
        if let Err(e) = server.await {
            tracing::error!(error = %e, "SSE server shutdown with error");
        } else {
            tracing::info!("SSE server shutdown successfully");
        }
    });

    // Create the echo service
    let service_ct = sse_server.with_service(echo::Echo::default);
    tracing::info!("SSE server created successfully with Echo service");

    // Force using this cancellation token to ensure proper shutdown
    Ok((service_ct, url))
}

#[tokio::test]
async fn test_protocol_initialization() -> Result<()> {
    const BIND_ADDRESS: &str = "127.0.0.1:8181";
    // Start the SSE server
    let (server_handle, server_url) = create_sse_server(BIND_ADDRESS.parse()?).await?;

    // Create a child process for the proxy
    let mut cmd = tokio::process::Command::new("cargo");
    cmd.arg("run")
        .arg("--")
        .arg(&server_url)
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;

    // Get stdin and stdout handles
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // Send initialization message
    let init_message = r#"{"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1.0"}}}"#;
    stdin.write_all(init_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the response
    let mut response = String::new();
    reader.read_line(&mut response).await?;

    // Verify the response contains expected data
    assert!(response.contains("\"id\":\"init-1\""));
    assert!(response.contains("\"result\""));

    // Send initialized notification
    let initialized_message = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin.write_all(initialized_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Call the echo tool
    let echo_call = r#"{"jsonrpc":"2.0","id":"call-1","method":"tools/call","params":{"name":"echo","arguments":{"message":"Hey!"}}}"#;
    stdin.write_all(echo_call.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the response
    let mut echo_response = String::new();
    reader.read_line(&mut echo_response).await?;

    // Verify the echo response
    assert!(echo_response.contains("\"id\":\"call-1\""));
    assert!(echo_response.contains("Hey!"));

    // Clean up
    child.kill().await?;
    server_handle.cancel();

    Ok(())
}

#[tokio::test]
async fn test_reconnection_handling() -> Result<()> {
    // Set up custom logger for this test to clearly see what's happening
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    const BIND_ADDRESS: &str = "127.0.0.1:8182";

    // Start the SSE server
    println!("Test: Starting initial SSE server");
    let (server_handle, server_url) = create_sse_server(BIND_ADDRESS.parse()?).await?;

    // Create a child process for the proxy
    println!("Test: Creating proxy process");
    let mut cmd = tokio::process::Command::new("cargo");
    cmd.arg("run")
        .arg("--")
        .arg(&server_url)
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;

    // Get stdin and stdout handles
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // Send initialization message
    let init_message = r#"{"jsonrpc":"2.0","id":"init-1","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0.1.0"}}}"#;
    stdin.write_all(init_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the response
    let mut response = String::new();
    reader.read_line(&mut response).await?;

    // Send initialized notification
    let initialized_message = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin.write_all(initialized_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Call the echo tool to ensure server is working
    let initial_echo_call = r#"{"jsonrpc":"2.0","id":"call-1","method":"tools/call","params":{"name":"echo","arguments":{"message":"Initial Call"}}}"#;
    stdin.write_all(initial_echo_call.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    let mut initial_echo_response = String::new();
    reader.read_line(&mut initial_echo_response).await?;
    assert!(
        initial_echo_response.contains("Initial Call"),
        "Initial echo call failed"
    );

    // Shutdown the server
    server_handle.cancel();

    // Give the server time to shut down
    sleep(Duration::from_millis(1000)).await;

    // Create a new server on the same address
    println!("Test: Starting new SSE server");
    let (new_ct, new_url) = create_sse_server(BIND_ADDRESS.parse()?).await?;
    assert_eq!(
        server_url, new_url,
        "New server URL should match the original"
    );

    // Give the proxy time to reconnect
    sleep(Duration::from_millis(3000)).await;

    // Call the echo tool after reconnection
    let echo_call = r#"{"jsonrpc":"2.0","id":"call-2","method":"tools/call","params":{"name":"echo","arguments":{"message":"After Reconnect"}}}"#;
    stdin.write_all(echo_call.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the response
    let mut echo_response = String::new();
    reader.read_line(&mut echo_response).await?;

    // Even if the response contains an error, we should at least get a response
    assert!(
        echo_response.contains("\"id\":\"call-2\""),
        "No response received after reconnection"
    );

    // Clean up
    new_ct.cancel();
    sleep(Duration::from_millis(500)).await; // Give server time to shutdown
    child.kill().await?;

    Ok(())
}

#[tokio::test]
async fn test_server_info_and_capabilities() -> Result<()> {
    const BIND_ADDRESS: &str = "127.0.0.1:8183";
    // Start the SSE server
    let (server_handle, server_url) = create_sse_server(BIND_ADDRESS.parse()?).await?;

    // Create a transport for the proxy
    let transport = TokioChildProcess::new(
        tokio::process::Command::new("cargo")
            .arg("run")
            .arg("--")
            .arg(&server_url),
    )?;

    // Connect a client to the proxy
    let client = ().serve(transport).await?;

    // List available tools
    let tools = client.list_all_tools().await?;

    // Verify the echo tool is available
    assert!(tools.iter().any(|t| t.name == "echo"));

    // Call the echo tool with a test message
    if let Some(echo_tool) = tools.iter().find(|t| t.name.contains("echo")) {
        let result = client
            .call_tool(CallToolRequestParam {
                name: echo_tool.name.clone(),
                arguments: Some(object!({
                    "message": "Testing server capabilities"
                })),
            })
            .await?;

        // Verify the response
        let result_str = format!("{:?}", result);
        assert!(result_str.contains("Testing server capabilities"));
    } else {
        panic!("Echo tool not found");
    }

    // Clean up
    drop(client);
    server_handle.cancel();

    Ok(())
}
