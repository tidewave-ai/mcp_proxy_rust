mod echo;
use anyhow::Result;
use rmcp::{
    ServiceExt,
    model::CallToolRequestParam,
    object,
    transport::{ConfigureCommandExt, TokioChildProcess},
};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::{sleep, timeout},
};

/// A guard that ensures processes are killed on drop, especially on test failures (panics)
struct TestGuard {
    child: Option<tokio::process::Child>,
    server_handle: Option<tokio::process::Child>,
    stderr_buffer: Arc<Mutex<Vec<String>>>,
}

impl TestGuard {
    fn new(
        child: tokio::process::Child,
        server_handle: tokio::process::Child,
        stderr_buffer: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        Self {
            child: Some(child),
            server_handle: Some(server_handle),
            stderr_buffer,
        }
    }
}

impl Drop for TestGuard {
    fn drop(&mut self) {
        // If we're dropping because of a panic, print the stderr content
        if std::thread::panicking() {
            eprintln!("Test failed! Process stderr output:");
            for line in self.stderr_buffer.lock().unwrap().iter() {
                eprintln!("{}", line);
            }
        }

        // Force kill both processes
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
        if let Some(mut server_handle) = self.server_handle.take() {
            let _ = server_handle.start_kill();
        }
    }
}

/// Spawns a proxy process with stdin, stdout, and stderr all captured
async fn spawn_proxy(
    server_url: &str,
    extra_args: Vec<&str>,
) -> Result<(
    tokio::process::Child,
    tokio::io::BufReader<tokio::process::ChildStdout>,
    tokio::io::BufReader<tokio::process::ChildStderr>,
    tokio::process::ChildStdin,
)> {
    let mut cmd = tokio::process::Command::new("./target/debug/mcp-proxy");
    cmd.arg(server_url)
        .args(extra_args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .stdin(std::process::Stdio::piped());

    let mut child = cmd.spawn()?;
    let stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let stderr = BufReader::new(child.stderr.take().unwrap());

    Ok((child, stdout, stderr, stdin))
}

/// Collects stderr lines in the background
fn collect_stderr(
    mut stderr_reader: BufReader<tokio::process::ChildStderr>,
) -> Arc<Mutex<Vec<String>>> {
    let stderr_buffer = Arc::new(Mutex::new(Vec::new()));
    let buffer_clone = stderr_buffer.clone();

    tokio::spawn(async move {
        let mut line = String::new();
        while let Ok(bytes_read) = stderr_reader.read_line(&mut line).await {
            if bytes_read == 0 {
                break;
            }
            buffer_clone.lock().unwrap().push(line.clone());
            line.clear();
        }
    });

    stderr_buffer
}

// Creates a new SSE server for testing
// Starts the echo-server as a subprocess
async fn create_sse_server(
    server_name: &str,
    address: SocketAddr,
) -> Result<(tokio::process::Child, String)> {
    let url = if server_name == "echo_streamable" {
        format!("http://{}", address)
    } else {
        format!("http://{}/sse", address)
    };

    tracing::info!("Starting echo-server at {}", url);

    // Create echo-server process
    let mut cmd = tokio::process::Command::new(format!("./target/debug/examples/{}", server_name));
    cmd.arg("--address").arg(address.to_string());

    tracing::debug!("cmd: {:?}", cmd);

    // Start the process with stdout/stderr redirected to null
    let child = cmd
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    // Give the server time to start up
    sleep(Duration::from_millis(500)).await;
    tracing::info!("{} server started successfully", server_name);

    Ok((child, url))
}

async fn protocol_initialization(server_name: &str) -> Result<()> {
    const BIND_ADDRESS: &str = "127.0.0.1:8181";
    let (server_handle, server_url) = create_sse_server(server_name, BIND_ADDRESS.parse()?).await?;

    // Create a child process for the proxy with stderr capture
    let (child, mut reader, stderr_reader, mut stdin) = spawn_proxy(&server_url, vec![]).await?;
    let stderr_buffer = collect_stderr(stderr_reader);
    let _guard = TestGuard::new(child, server_handle, stderr_buffer);

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

    Ok(())
}

async fn protocol_version_compatibility_with_rewrite(server_name: &str) -> Result<()> {
    const BIND_ADDRESS: &str = "127.0.0.1:8186";
    let (server_handle, server_url) = create_sse_server(server_name, BIND_ADDRESS.parse()?).await?;

    // Create a child process for the proxy with rewrite flag enabled
    let (child, mut reader, stderr_reader, mut stdin) = spawn_proxy(&server_url, vec!["--rewrite-protocol-version"]).await?;
    let stderr_buffer = collect_stderr(stderr_reader);
    let _guard = TestGuard::new(child, server_handle, stderr_buffer);

    // Send initialization message with 2024-11-05 (older version)
    let init_message = r#"{"jsonrpc":"2.0","id":"init-compat","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"compatibility-test","version":"0.1.0"}}}"#;
    stdin.write_all(init_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the response
    let mut response = String::new();
    reader.read_line(&mut response).await?;

    tracing::info!("Protocol compatibility test with rewrite response: {}", response.trim());

    // Verify the response contains expected data
    assert!(response.contains("\"id\":\"init-compat\""));
    assert!(response.contains("\"result\""));

    // Check that protocol version rewriting is working
    if response.contains("protocolVersion") {
        // Should contain 2024-11-05 (either original or rewritten)
        assert!(response.contains("2024-11-05"), "Protocol version should be 2024-11-05");
    }

    // Send initialized notification
    let initialized_message = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin.write_all(initialized_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    Ok(())
}

async fn protocol_version_compatibility_without_rewrite(server_name: &str) -> Result<()> {
    const BIND_ADDRESS: &str = "127.0.0.1:8187";
    let (server_handle, server_url) = create_sse_server(server_name, BIND_ADDRESS.parse()?).await?;

    // Create a child process for the proxy WITHOUT rewrite flag
    let (child, mut reader, stderr_reader, mut stdin) = spawn_proxy(&server_url, vec![]).await?;
    let stderr_buffer = collect_stderr(stderr_reader);
    let _guard = TestGuard::new(child, server_handle, stderr_buffer);

    // Send initialization message with 2024-11-05 (older version)
    let init_message = r#"{"jsonrpc":"2.0","id":"init-no-rewrite","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"no-rewrite-test","version":"0.1.0"}}}"#;
    stdin.write_all(init_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the response
    let mut response = String::new();
    reader.read_line(&mut response).await?;

    tracing::info!("Protocol compatibility test without rewrite response: {}", response.trim());

    // Verify the response contains expected data
    assert!(response.contains("\"id\":\"init-no-rewrite\""));
    assert!(response.contains("\"result\""));

    // Send initialized notification
    let initialized_message = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin.write_all(initialized_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    Ok(())
}

#[tokio::test]
async fn test_protocol_initialization() -> Result<()> {
    protocol_initialization("echo").await?;
    protocol_initialization("echo_streamable").await?;

    Ok(())
}

async fn reconnection_handling(server_name: &str) -> Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    const BIND_ADDRESS: &str = "127.0.0.1:8182";

    // Start the SSE server
    tracing::info!("Test: Starting initial SSE server");
    let (server_handle, server_url) = create_sse_server(server_name, BIND_ADDRESS.parse()?).await?;

    // Create a child process for the proxy
    tracing::info!("Test: Creating proxy process");
    let (child, mut reader, stderr_reader, mut stdin) = spawn_proxy(&server_url, vec![]).await?;
    let stderr_buffer = collect_stderr(stderr_reader);
    let mut test_guard = TestGuard::new(child, server_handle, stderr_buffer);

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
    if let Some(mut server) = test_guard.server_handle.take() {
        server.kill().await?;
    }

    // Give the server time to shut down
    sleep(Duration::from_millis(1000)).await;

    // Create a new server on the same address
    tracing::info!("Test: Starting new SSE server");
    let (new_server_handle, new_url) =
        create_sse_server(server_name, BIND_ADDRESS.parse()?).await?;
    assert_eq!(
        server_url, new_url,
        "New server URL should match the original"
    );

    // Update the test guard with the new server handle
    test_guard.server_handle = Some(new_server_handle);

    // Give the proxy time to reconnect
    sleep(Duration::from_millis(3000)).await;

    // Call the echo tool after reconnection
    let echo_call = r#"{"jsonrpc":"2.0","id":"call-2","method":"tools/call","params":{"name":"echo","arguments":{"message":"After Reconnect"}}}"#;
    stdin.write_all(echo_call.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the response
    let mut echo_response = String::new();
    reader.read_line(&mut echo_response).await?;

    tracing::info!("Test: Received echo response: {}", echo_response.trim());

    // Even if the response contains an error, we should at least get a response
    assert!(
        echo_response.contains("\"id\":\"call-2\""),
        "No response received after reconnection"
    );

    Ok(())
}

#[tokio::test]
async fn test_reconnection_handling() -> Result<()> {
    reconnection_handling("echo").await?;
    reconnection_handling("echo_streamable").await?;

    Ok(())
}

async fn server_info_and_capabilities(server_name: &str) -> Result<()> {
    const BIND_ADDRESS: &str = "127.0.0.1:8183";
    // Start the SSE server
    let (mut server_handle, server_url) =
        create_sse_server(server_name, BIND_ADDRESS.parse()?).await?;

    // Create a transport for the proxy
    let transport = TokioChildProcess::new(
        tokio::process::Command::new("./target/debug/mcp-proxy").configure(|cmd| {
            cmd.arg(&server_url);
        }),
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
    server_handle.kill().await?;

    Ok(())
}

#[tokio::test]
async fn test_server_info_and_capabilities() -> Result<()> {
    server_info_and_capabilities("echo").await?;
    server_info_and_capabilities("echo_streamable").await?;

    Ok(())
}

async fn initial_connection_retry(server_name: &str) -> Result<()> {
    // Set up custom logger for this test to clearly see what's happening
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    const BIND_ADDRESS: &str = "127.0.0.1:8184";
    let server_url = if server_name == "echo_streamable" {
        format!("http://{}", BIND_ADDRESS)
    } else {
        format!("http://{}/sse", BIND_ADDRESS)
    };
    let bind_addr: SocketAddr = BIND_ADDRESS.parse()?;

    // 1. Start the proxy process BEFORE the server
    tracing::info!("Test: Starting proxy process...");
    let (child, mut reader, stderr_reader, mut stdin) =
        spawn_proxy(&server_url, vec!["--initial-retry-interval", "1"]).await?;

    let stderr_buffer = collect_stderr(stderr_reader);

    // 2. Wait for slightly longer than the proxy's retry delay
    // This ensures the proxy has attempted connection at least once and is retrying.
    let retry_wait = Duration::from_secs(2);
    tracing::info!(
        "Test: Waiting {:?} for proxy to attempt connection...",
        retry_wait
    );
    sleep(retry_wait).await;

    // Send initialize message WHILE proxy is still trying to connect
    // (it will be buffered by the OS pipe until proxy reads stdin)
    tracing::info!("Test: Sending initialize request (before server starts)...");
    let init_message = r#"{"jsonrpc":"2.0","id":"init-retry","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"retry-test","version":"0.1.0"}}}"#;
    stdin.write_all(init_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // 3. Start the SSE server AFTER the wait and AFTER sending init
    tracing::info!("Test: Starting SSE server on {}", BIND_ADDRESS);
    let (server_handle, returned_url) = create_sse_server(server_name, bind_addr).await?;
    assert_eq!(server_url, returned_url, "Server URL mismatch");

    let _test_guard = TestGuard::new(child, server_handle, stderr_buffer);

    // 4. Proceed with initialization handshake (Proxy should now process buffered init)
    // Read the initialize response (with a timeout)
    tracing::info!("Test: Waiting for initialize response...");
    let mut init_response = String::new();
    match timeout(
        Duration::from_secs(10),
        reader.read_line(&mut init_response),
    )
    .await
    {
        Ok(Ok(_)) => {
            tracing::info!(
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

    tracing::info!("Test: Sending initialized notification...");
    let initialized_message = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
    stdin.write_all(initialized_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // 5. Test basic functionality (e.g., echo tool call)
    tracing::info!("Test: Sending echo request...");
    let echo_call = r#"{"jsonrpc":"2.0","id":"call-retry","method":"tools/call","params":{"name":"echo","arguments":{"message":"Hello after initial retry!"}}}"#;
    stdin.write_all(echo_call.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    tracing::info!("Test: Waiting for echo response...");
    let mut echo_response = String::new();
    match timeout(Duration::from_secs(5), reader.read_line(&mut echo_response)).await {
        Ok(Ok(_)) => {
            tracing::info!("Test: Received echo response: {}", echo_response.trim());
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

    tracing::info!("Test: Completed successfully");
    Ok(())
}

#[tokio::test]
async fn test_initial_connection_retry() -> Result<()> {
    initial_connection_retry("echo").await?;
    initial_connection_retry("echo_streamable").await?;

    Ok(())
}

async fn ping_when_disconnected(server_name: &str) -> Result<()> {
    const BIND_ADDRESS: &str = "127.0.0.1:8185";
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_test_writer()
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // 1. Start the SSE server
    tracing::info!("Test: Starting SSE server for ping test");
    let (server_handle, server_url) = create_sse_server(server_name, BIND_ADDRESS.parse()?).await?;

    // Create a child process for the proxy
    tracing::info!("Test: Creating proxy process");
    let (child, mut reader, stderr_reader, mut stdin) =
        spawn_proxy(&server_url, vec!["--debug"]).await?;

    let stderr_buffer = collect_stderr(stderr_reader);

    let mut test_guard = TestGuard::new(child, server_handle, stderr_buffer);

    // 2. Initializes everything
    let init_message = r#"{"jsonrpc":"2.0","id":"init-ping","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"ping-test","version":"0.1.0"}}}"#;
    tracing::info!("Test: Sending initialize request");
    stdin.write_all(init_message.as_bytes()).await?;
    stdin.write_all(b"\n").await?;

    // Read the initialize response
    let mut init_response = String::new();
    match timeout(Duration::from_secs(5), reader.read_line(&mut init_response)).await {
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
    if let Some(mut server) = test_guard.server_handle.take() {
        server.kill().await?;
    }
    // Give the server time to shut down and the proxy time to notice
    sleep(Duration::from_secs(3)).await;

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

    Ok(())
}

#[tokio::test]
async fn test_ping_when_disconnected() -> Result<()> {
    ping_when_disconnected("echo").await?;
    ping_when_disconnected("echo_streamable").await?;

    Ok(())
}

#[tokio::test]
async fn test_protocol_version_compatibility() -> Result<()> {
    // Test with rewrite flag enabled
    protocol_version_compatibility_with_rewrite("echo").await?;
    protocol_version_compatibility_with_rewrite("echo_streamable").await?;

    // Test without rewrite flag (default behavior)
    protocol_version_compatibility_without_rewrite("echo").await?;
    protocol_version_compatibility_without_rewrite("echo_streamable").await?;

    Ok(())
}
