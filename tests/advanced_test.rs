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
    time::{sleep, timeout},
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
    let mut cmd = tokio::process::Command::new("./target/debug/mcp-proxy");
    cmd.arg(&server_url)
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
    let mut cmd = tokio::process::Command::new("./target/debug/mcp-proxy");
    cmd.arg(&server_url)
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
        tokio::process::Command::new("./target/debug/mcp-proxy").arg(&server_url),
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

#[tokio::test]
async fn test_initial_connection_retry() -> Result<()> {
    // Set up custom logger for this test to clearly see what's happening
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    const BIND_ADDRESS: &str = "127.0.0.1:8184";
    let server_url = format!("http://{}/sse", BIND_ADDRESS);
    let bind_addr: SocketAddr = BIND_ADDRESS.parse()?;

    // 1. Start the proxy process BEFORE the server
    println!("Test: Starting proxy process...");
    let mut cmd = tokio::process::Command::new("./target/debug/mcp-proxy");
    cmd.arg(&server_url)
        .arg("--initial-retry-interval")
        .arg("1")
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;

    // Get stdin and stdout handles
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // 2. Wait for slightly longer than the proxy's retry delay
    // This ensures the proxy has attempted connection at least once and is retrying.
    let retry_wait = Duration::from_secs(2);
    println!(
        "Test: Waiting {:?} for proxy to attempt connection...",
        retry_wait
    );
    sleep(retry_wait).await;

    // Send initialize message WHILE proxy is still trying to connect
    // (it will be buffered by the OS pipe until proxy reads stdin)
    println!("Test: Sending initialize request (before server starts)...");
    let init_message = r#"{"jsonrpc":"2.0","id":"init-retry","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"retry-test","version":"0.1.0"}}}"#;
    stdin.write_all(init_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // 3. Start the SSE server AFTER the wait and AFTER sending init
    println!("Test: Starting SSE server on {}", BIND_ADDRESS);
    let (server_handle, returned_url) = create_sse_server(bind_addr).await?;
    assert_eq!(server_url, returned_url, "Server URL mismatch");

    // 4. Proceed with initialization handshake (Proxy should now process buffered init)
    // Read the initialize response (with a timeout)
    println!("Test: Waiting for initialize response...");
    let mut init_response = String::new();
    match timeout(
        Duration::from_secs(10),
        reader.read_line(&mut init_response),
    )
    .await
    {
        Ok(Ok(_)) => {
            println!(
                "Test: Received initialize response: {}",
                init_response.trim()
            );
            assert!(
                init_response.contains("\"id\":\"init-retry\""),
                "Init response missing correct ID"
            );
            assert!(
                init_response.contains("\"result\""),
                "Init response missing result"
            );
        }
        Ok(Err(e)) => return Err(anyhow::anyhow!("Error reading init response: {}", e)),
        Err(_) => return Err(anyhow::anyhow!("Timed out waiting for init response")),
    }

    println!("Test: Sending initialized notification...");
    let initialized_message = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin.write_all(initialized_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // 5. Test basic functionality (e.g., echo tool call)
    println!("Test: Sending echo request...");
    let echo_call = r#"{"jsonrpc":"2.0","id":"call-retry","method":"tools/call","params":{"name":"echo","arguments":{"message":"Hello after initial retry!"}}}"#;
    stdin.write_all(echo_call.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    println!("Test: Waiting for echo response...");
    let mut echo_response = String::new();
    match timeout(Duration::from_secs(5), reader.read_line(&mut echo_response)).await {
        Ok(Ok(_)) => {
            println!("Test: Received echo response: {}", echo_response.trim());
            assert!(
                echo_response.contains("\"id\":\"call-retry\""),
                "Echo response missing correct ID"
            );
            assert!(
                echo_response.contains("Hello after initial retry!"),
                "Echo response missing correct message"
            );
        }
        Ok(Err(e)) => return Err(anyhow::anyhow!("Error reading echo response: {}", e)),
        Err(_) => return Err(anyhow::anyhow!("Timed out waiting for echo response")),
    }

    // 6. Cleanup
    println!("Test: Cleaning up...");
    child.kill().await?;
    server_handle.cancel();
    sleep(Duration::from_millis(500)).await; // Give server time to shutdown

    println!("Test: Completed successfully");
    Ok(())
}

#[tokio::test]
async fn test_ping_when_disconnected() -> Result<()> {
    const BIND_ADDRESS: &str = "127.0.0.1:8185";
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // 1. Start the SSE server
    tracing::info!("Test: Starting SSE server for ping test");
    let (server_handle, server_url) = create_sse_server(BIND_ADDRESS.parse()?).await?;

    // Create a child process for the proxy
    tracing::info!("Test: Creating proxy process");
    let mut cmd = tokio::process::Command::new("./target/debug/mcp-proxy");
    cmd.arg(&server_url)
        .arg("--debug")
        .stdout(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;

    // Get stdin and stdout handles
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    // 2. Initializes everything
    let init_message = r#"{"jsonrpc":"2.0","id":"init-ping","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"ping-test","version":"0.1.0"}}}"#;
    tracing::info!("Test: Sending initialize request");
    stdin.write_all(init_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the initialize response
    let mut init_response = String::new();
    match timeout(
        Duration::from_secs(15),
        reader.read_line(&mut init_response),
    )
    .await
    {
        Ok(Ok(_)) => {
            tracing::info!(
                "Test: Received initialize response: {}",
                init_response.trim()
            );
            assert!(init_response.contains("\"id\":\"init-ping\""));
        }
        Ok(Err(e)) => panic!("Failed to read init response: {}", e),
        Err(_) => panic!("Timed out waiting for init response"),
    }

    // Send initialized notification
    let initialized_message = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    tracing::info!("Test: Sending initialized notification");
    stdin.write_all(initialized_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    // Allow time for proxy to process initialized and potentially send buffered msgs (if any)
    sleep(Duration::from_millis(100)).await;

    // 3. Kills the SSE server
    tracing::info!("Test: Shutting down SSE server");
    server_handle.cancel();
    // Give the server time to shut down and the proxy time to notice
    sleep(Duration::from_secs(2)).await;

    // 4. Sends a ping request
    let ping_message = r#"{"jsonrpc":"2.0","id":"ping-1","method":"ping"}"#;
    tracing::info!("Test: Sending ping request while server is down");
    stdin.write_all(ping_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // 5. Checks that it receives a response
    let mut ping_response = String::new();
    match timeout(Duration::from_secs(2), reader.read_line(&mut ping_response)).await {
        Ok(Ok(_)) => {
            tracing::info!("Test: Received ping response: {}", ping_response.trim());
            // Expecting: {"jsonrpc":"2.0","id":"ping-1","result":{}}
            assert!(
                ping_response.contains("\"id\":\"ping-1\""),
                "Response ID mismatch"
            );
            assert!(
                ping_response.contains("\"result\":{}"),
                "Expected empty result object"
            );
        }
        Ok(Err(e)) => panic!("Failed to read ping response: {}", e),
        Err(_) => panic!("Timed out waiting for ping response"),
    }

    // Clean up
    tracing::info!("Test: Cleaning up proxy process");
    child.kill().await?;

    Ok(())
}
