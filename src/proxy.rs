use crate::models::{
    AppState, BufferMode, JSONRPCError, JSONRPCMessage, JSONRPCNotification, JSONRPCRequest,
    JSONRPCResponse, NumberOrString, ProxyState, RequestId, SSEEvent,
};
use crate::sse;
use anyhow::Result;
use futures::{Stream, StreamExt};
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{
    Mutex,
    mpsc::{self, Receiver, Sender},
};
/// info!, debug!, error!, etc.
use tracing::*;
use url::Url;
use uuid::Uuid;

/// Event types that can be processed by the proxy
#[derive(Debug)]
pub enum ProxyEvent {
    ConnectToSSE,
    FlushInBuffer,
    SSEEvent(SSEEvent),
    SSEError(String),
    IOEvent(JSONRPCMessage),
    IOError(String),
    Reconnect,
    FailFlush,
    Shutdown(String),
}

/// Start the proxy with the given state
pub async fn start_proxy(
    url: String,
    max_disconnected_time: Option<u64>,
    receive_timeout_ms: u64,
) -> Result<()> {
    info!("Initializing proxy with URL: {}", url);

    let state = Arc::new(Mutex::new(AppState::new(url, max_disconnected_time)));

    // Create channels for communication between tasks
    let (sender, receiver) = mpsc::channel(100);

    // Spawn the event loop - do this last so we already have the connection attempt started
    let event_loop_state = state.clone();
    info!("Starting event loop");
    let event_loop_handle = tokio::spawn(async move {
        sender.send(ProxyEvent::ConnectToSSE).await.unwrap();
        event_loop(event_loop_state, receiver, sender, receive_timeout_ms).await
    });

    // Wait for the event loop to complete
    match event_loop_handle.await {
        Ok(result) => result,
        Err(e) => Err(anyhow::anyhow!("Event loop task failed: {}", e)),
    }
}

/// Main event loop for the proxy
async fn event_loop(
    state: Arc<Mutex<AppState>>,
    mut receiver: Receiver<ProxyEvent>,
    sender: Sender<ProxyEvent>,
    receive_timeout_ms: u64,
) -> Result<()> {
    while let Some(event) = receiver.recv().await {
        let mut state_guard = state.lock().await;
        debug!("Received event: {:?}", event);
        match &event {
            ProxyEvent::ConnectToSSE => {
                debug!("Connecting to SSE: {:?}", state_guard.url);
                // Drop the lock before calling connect_to_sse to avoid deadlock
                drop(state_guard);

                if let Err(e) = connect_to_sse(state.clone(), sender.clone()).await {
                    error!("Error connecting to SSE server: {}", e);
                }
            }
            ProxyEvent::FlushInBuffer => {
                debug!(
                    "Flushing buffer with {} items after reconnection",
                    state_guard.in_buf.len()
                );
                for message in std::mem::take(&mut state_guard.in_buf) {
                    sender.send(ProxyEvent::IOEvent(message)).await.unwrap();
                }
            }
            ProxyEvent::SSEEvent(SSEEvent::Endpoint(endpoint)) => match state_guard.state {
                ProxyState::WaitingForEndpoint => {
                    // Parse the full URL by properly joining the base URL with the endpoint
                    let full_endpoint =
                        if endpoint.starts_with("http://") || endpoint.starts_with("https://") {
                            // If it's already an absolute URL, use it directly
                            endpoint.clone()
                        } else {
                            // Otherwise join it with the base URL
                            match Url::parse(&state_guard.url) {
                                Ok(base_url) => {
                                    match base_url.join(endpoint) {
                                        Ok(full_url) => full_url.to_string(),
                                        Err(e) => {
                                            error!("Failed to join URL: {}", e);
                                            // Fall back to direct concatenation
                                            format!("{}{}", state_guard.url, endpoint)
                                        }
                                    }
                                }
                                Err(e) => {
                                    error!("Invalid base URL: {}", e);
                                    // Fall back to direct concatenation
                                    format!("{}{}", state_guard.url, endpoint)
                                }
                            }
                        };

                    debug!("Using endpoint: {}", full_endpoint);
                    state_guard.endpoint = Some(full_endpoint);
                    if state_guard.init_message.is_some() {
                        debug!("Forwarding stored init message to endpoint");
                        let msg = state_guard.init_message.as_ref().unwrap();
                        sse::forward_message(
                            &serde_json::to_value(&msg)?,
                            &state_guard.endpoint.clone().unwrap(),
                            receive_timeout_ms,
                        )
                        .await?;
                        state_guard.state = ProxyState::WaitingForServerInitHidden(msg.id.clone());
                    } else {
                        debug!("Waiting for client init message");
                        spawn_stdin_reader(sender.clone());
                        state_guard.state = ProxyState::WaitingForClientInit;
                    }
                }
                _ => {
                    error!(
                        "Unexpected state when receiving endpoint: {:?}",
                        state_guard.state
                    );
                }
            },
            ProxyEvent::IOEvent(JSONRPCMessage::Request(
                message @ JSONRPCRequest { id, method, .. },
            )) if state_guard.state == ProxyState::WaitingForClientInit
                && method == "initialize" =>
            {
                debug!("Got client init!");

                sse::forward_message(
                    &serde_json::to_value(&message)?,
                    &state_guard.endpoint.clone().unwrap(),
                    receive_timeout_ms,
                )
                .await?;

                state_guard.init_message = Some(message.clone());
                state_guard.state = ProxyState::WaitingForServerInit(id.clone());
            }
            ProxyEvent::SSEEvent(SSEEvent::Message(JSONRPCMessage::Response(
                message @ JSONRPCResponse { id, .. },
            ))) if matches!(state_guard.state, ProxyState::WaitingForServerInit(ref expected_id) if expected_id == id) =>
            {
                debug!("Got server init, now waiting for client initialized notification!");
                writeln!(io::stdout(), "{}", serde_json::to_string(&message)?)?;
                state_guard.state = ProxyState::WaitingForClientInitialized;
            }
            ProxyEvent::SSEEvent(SSEEvent::Message(JSONRPCMessage::Response(
                JSONRPCResponse { id, .. },
            ))) if matches!(state_guard.state, ProxyState::WaitingForServerInitHidden(ref expected_id) if expected_id == id) =>
            {
                debug!(
                    "Got server init, but we are reconnecting, so we fake the initialized notification!"
                );

                // for protocol compliance, we need to also send the initialized notification
                sse::forward_message(
                    &serde_json::to_value(&JSONRPCNotification {
                        jsonrpc: "2.0".to_string(),
                        method: "notifications/initialized".to_string(),
                        params: None,
                    })?,
                    &state_guard.endpoint.clone().unwrap(),
                    receive_timeout_ms,
                )
                .await?;

                // reset the state to connected
                state_guard.connected();
                sender.send(ProxyEvent::FlushInBuffer).await.unwrap();
            }
            ProxyEvent::IOEvent(JSONRPCMessage::Notification(
                message @ JSONRPCNotification { method, .. },
            )) if method == "notifications/initialized" => {
                sse::forward_message(
                    &serde_json::to_value(&message)?,
                    &state_guard.endpoint.clone().unwrap(),
                    receive_timeout_ms,
                )
                .await?;

                // reset the state to connected
                state_guard.connected();
                sender.send(ProxyEvent::FlushInBuffer).await.unwrap();
            }
            ProxyEvent::IOEvent(io_event @ JSONRPCMessage::Request(_))
                if state_guard.state != ProxyState::Connected
                    && state_guard.state != ProxyState::WaitingForClientInit =>
            {
                debug!("Received IO event, but not connected: {:?}", io_event);
                match io_event {
                    JSONRPCMessage::Request(JSONRPCRequest { id, method, .. })
                        if method == "ping" =>
                    {
                        // we directly answer ping requests
                        writeln!(
                            io::stdout(),
                            "{}",
                            serde_json::to_string(&JSONRPCResponse {
                                jsonrpc: "2.0".to_string(),
                                id: id.clone(),
                                result: Some(serde_json::json!({})),
                                error: None,
                            })?
                        )?;
                    }
                    JSONRPCMessage::Request(JSONRPCRequest { method, .. })
                        if method == "tools/call" =>
                    {
                        // we store tool calls for later
                        state_guard.in_buf.push(io_event.clone());
                    }
                    JSONRPCMessage::Request(JSONRPCRequest { id, .. }) => {
                        reply_disconnected(&id)?;
                    }
                    _ => {
                        // we ignore other requests
                    }
                }
            }
            // Regular events
            ProxyEvent::SSEEvent(sse_event) => {
                // Release the lock before handling the event
                let sse_event_clone = sse_event.clone();
                drop(state_guard);

                if let Err(e) = handle_sse_event(state.clone(), sse_event_clone).await {
                    error!("Error handling SSE event: {}", e);
                }
            }
            ProxyEvent::IOEvent(io_event) => {
                // Release the lock before handling the event
                let io_event_clone = io_event.clone();
                drop(state_guard);

                if let Err(e) = handle_io_event(
                    state.clone(),
                    io_event_clone,
                    receive_timeout_ms,
                    sender.clone(),
                )
                .await
                {
                    error!("Error handling IO event: {}", e);
                }
            }
            ProxyEvent::IOError(error) => {
                error!("IO error: {}", error);
                return Err(anyhow::anyhow!("IO error: {}", error));
            }
            ProxyEvent::SSEError(error) => {
                error!("SSE error: {} - changing state to disconnected", error);

                // If this is the first attempt, schedule a FailFlush after 20 seconds
                if state_guard.connect_tries == 0 {
                    let sender_clone = sender.clone();
                    tokio::spawn(async move {
                        debug!("Scheduling FailFlush to occur in 20 seconds if still disconnected");
                        tokio::time::sleep(tokio::time::Duration::from_secs(20)).await;
                        if let Err(e) = sender_clone.send(ProxyEvent::FailFlush).await {
                            error!("Failed to send FailFlush event: {}", e);
                        } else {
                            debug!("FailFlush event sent successfully");
                        }
                    });
                }

                state_guard.state = ProxyState::Disconnected;
                state_guard.endpoint = None;
                state_guard.disconnected_since = Some(std::time::Instant::now());
                let backoff = std::cmp::min(2u32.pow(state_guard.connect_tries), 8);
                state_guard.connect_tries += 1;

                info!(
                    "SSE connection failed. Trying to reconnect in {}s.",
                    backoff
                );

                if state_guard.disconnected_too_long() {
                    error!("Reconnect timeout exceeded, giving up reconnection attempts");
                    return Err(anyhow::anyhow!("Reconnect timeout exceeded"));
                } else {
                    // Schedule the reconnect with proper error handling
                    let sender_clone = sender.clone();
                    tokio::spawn(async move {
                        debug!("Waiting {}s before attempting reconnection", backoff);
                        tokio::time::sleep(tokio::time::Duration::from_secs(backoff.into())).await;
                        debug!("Reconnection time reached, sending Reconnect event");
                        if let Err(e) = sender_clone.send(ProxyEvent::Reconnect).await {
                            error!("Failed to send Reconnect event: {}", e);
                        } else {
                            debug!("Reconnect event sent successfully");
                        }
                    });
                }
            }
            ProxyEvent::Reconnect => {
                // Try to reconnect
                info!("Attempting to reconnect to SSE server");
                // Drop the lock before calling connect_to_sse to avoid deadlock
                drop(state_guard);

                match connect_to_sse(state.clone(), sender.clone()).await {
                    Ok(_) => debug!("Reconnection initiated successfully"),
                    Err(e) => {
                        error!("Reconnection attempt failed: {}", e);
                    }
                }
            }
            ProxyEvent::FailFlush => {
                if state_guard.state != ProxyState::Disconnected {
                    debug!("Ignoring FailFlush event in state {:?}", state_guard.state);
                } else {
                    debug!("Processing FailFlush event");
                    // Change buffer mode to fail and flush the buffer with errors
                    info!("Did not reconnect in time. Flushing buffer, replying with errors!");
                    debug!(
                        "Flushing {} buffered messages with errors",
                        state_guard.in_buf.len()
                    );

                    // Extract messages to process
                    let buffered_messages = std::mem::take(&mut state_guard.in_buf);

                    // Clear ID map as well since we're abandoning any pending requests
                    if !state_guard.id_map.is_empty() {
                        debug!("Clearing ID map with {} entries", state_guard.id_map.len());
                        state_guard.id_map.clear();
                    }

                    state_guard.buf_mode = BufferMode::Fail;
                    debug!("Buffer mode set to Fail after flush");

                    // Release the lock before processing messages
                    drop(state_guard);

                    // Now process all the buffered messages and send error responses
                    for message in buffered_messages {
                        match message {
                            JSONRPCMessage::Request(request) => {
                                debug!(
                                    "Sending error response for buffered request: {}",
                                    request.method
                                );
                                if let Err(e) = reply_disconnected(&request.id) {
                                    error!("Failed to send error response: {}", e);
                                }
                            }
                            // For notifications, we can't reply with errors, just log them
                            JSONRPCMessage::Notification(notification) => {
                                debug!("Dropping buffered notification: {}", notification.method);
                            }
                            _ => {}
                        }
                    }
                }
            }
            ProxyEvent::Shutdown(reason) => {
                info!("Shutting down: {}", reason);
                return Ok(());
            }
        }
    }

    Ok(())
}

/// Connect to the SSE server
async fn connect_to_sse(state: Arc<Mutex<AppState>>, sender: Sender<ProxyEvent>) -> Result<()> {
    let url = {
        let state_guard = state.lock().await;
        state_guard.url.clone()
    };

    debug!("Connecting to SSE: {}", url);

    // Set state to waiting for endpoint before connecting
    {
        let mut state_guard = state.lock().await;
        state_guard.state = ProxyState::WaitingForEndpoint;
        state_guard.endpoint = None; // Clear any previous endpoint
        state_guard.buf_mode = BufferMode::Store; // Always buffer during connection attempts
        debug!("State changed to WaitingForEndpoint, buf_mode set to Store");
    }

    // Attempt to create an SSE stream
    match sse::create_sse_stream(&url).await {
        Ok(stream) => {
            debug!("SSE stream created successfully");
            // We got a stream, now spawn a task to process it
            spawn_sse_processor(stream, sender.clone());
            Ok(())
        }
        Err(e) => {
            error!("Failed to connect to SSE: {}", e);
            sender
                .send(ProxyEvent::SSEError(e.to_string()))
                .await
                .unwrap();
            Err(e)
        }
    }
}

/// Spawn a task to process the SSE stream
fn spawn_sse_processor(
    mut stream: Pin<Box<dyn Stream<Item = Result<SSEEvent>> + Send>>,
    sender: Sender<ProxyEvent>,
) {
    tokio::spawn(async move {
        debug!("SSE processor started");
        while let Some(result) = stream.next().await {
            match result {
                Ok(event) => {
                    if let Err(e) = sender.send(ProxyEvent::SSEEvent(event)).await {
                        error!("Failed to send SSE event to event loop: {}", e);
                        break;
                    }
                }
                Err(e) => {
                    error!("Error processing SSE event: {}", e);
                    break;
                }
            }
        }

        sender
            .send(ProxyEvent::SSEError("SSE stream ended".to_string()))
            .await
            .unwrap();
    });
}

/// Spawn a task to read from stdin
fn spawn_stdin_reader(sender: Sender<ProxyEvent>) {
    // Use tokio's own async stdin capabilities instead of standard library
    tokio::spawn(async move {
        debug!("Starting stdin reader");
        let mut buffer = String::new();
        let stdin = tokio::io::stdin();
        let mut reader = tokio::io::BufReader::new(stdin);

        loop {
            buffer.clear();
            match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut buffer).await {
                Ok(0) => {
                    // EOF reached
                    debug!("Stdin EOF reached");
                    break;
                }
                Ok(_) => {
                    // Remove trailing newline if present
                    if buffer.ends_with('\n') {
                        buffer.pop();
                    }

                    match serde_json::from_str::<JSONRPCMessage>(&buffer) {
                        Ok(message) => {
                            debug!("Read message from stdin: {}", buffer);
                            if sender.send(ProxyEvent::IOEvent(message)).await.is_err() {
                                error!("Failed to send message to event loop");
                                break;
                            }
                        }
                        Err(e) => {
                            error!("Error parsing JSON from stdin: {}", e);
                            let err_msg = format!("Error parsing JSON: {}", e);
                            if sender.send(ProxyEvent::IOError(err_msg)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("Error reading from stdin: {}", e);
                    let err_msg = format!("Error reading from stdin: {}", e);
                    if sender.send(ProxyEvent::IOError(err_msg)).await.is_err() {
                        break;
                    }
                    break;
                }
            }
        }

        debug!("Stdin reader ended");
        let _ = sender
            .send(ProxyEvent::Shutdown("Stdin closed".to_string()))
            .await;
    });
}

/// Handle an SSE event
async fn handle_sse_event(state: Arc<Mutex<AppState>>, sse_event: SSEEvent) -> Result<()> {
    match sse_event {
        SSEEvent::Message(JSONRPCMessage::Request(request)) => {
            // Lock state only when needed
            let mut state_guard = state.lock().await;
            let _endpoint = state_guard.endpoint.clone().unwrap();

            // Generate a random ID to prevent duplicate IDs
            let original_id = request.id.clone();
            let new_id = generate_id();

            let mut modified_request = request.clone();
            modified_request.id = NumberOrString::String(new_id.clone().into());

            // store the original id
            state_guard.id_map.insert(new_id.clone(), original_id);

            // forward the request
            writeln!(
                io::stdout(),
                "{}",
                serde_json::to_string(&modified_request)?
            )?;
            Ok(())
        }
        SSEEvent::Message(JSONRPCMessage::Response(response)) => {
            let mut state_guard = state.lock().await;
            // The key is always a UUID string, and we expect out IDs in responses,
            // so we need to convert to string here
            let lookup_key = response.id.to_string();

            if let Some(original_id) = state_guard.id_map.remove(&lookup_key) {
                drop(state_guard);
                // Clone original_id for later use in logging
                let original_id_clone = original_id.clone();

                // Replace the ID with the original one
                let mut modified_response = response.clone();
                modified_response.id = original_id;

                // Write the response to stdout
                info!(
                    "Forwarding response to client with original ID: {}",
                    original_id_clone
                );
                writeln!(
                    io::stdout(),
                    "{}",
                    serde_json::to_string(&modified_response)?
                )?;
                Ok(())
            } else {
                error!(
                    "Did not find original ID for response: {}. Discarding response!",
                    response.id
                );
                Ok(())
            }
        }
        // must be a notification without ID, can forward as is
        SSEEvent::Message(notification) => {
            writeln!(io::stdout(), "{}", serde_json::to_string(&notification)?)?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// Handle an IO event
async fn handle_io_event(
    state: Arc<Mutex<AppState>>,
    io_event: JSONRPCMessage,
    receive_timeout_ms: u64,
    _sender: Sender<ProxyEvent>,
) -> Result<()> {
    // Lock state to get the endpoint
    let mut state_guard = state.lock().await;
    let endpoint = state_guard.endpoint.clone().unwrap();

    match io_event {
        JSONRPCMessage::Request(request) => {
            // Generate a random ID to prevent duplicate IDs
            let original_id = request.id.clone();
            let new_id = generate_id();
            let mut modified_request = request.clone();
            modified_request.id = NumberOrString::String(new_id.clone().into());

            // forward the request
            if let Err(e) = sse::forward_message(
                &serde_json::to_value(&modified_request)?,
                &endpoint,
                receive_timeout_ms,
            )
            .await
            {
                // directly reply with an error
                let response = JSONRPCResponse {
                    jsonrpc: "2.0".to_string(),
                    id: request.id.clone(),
                    result: None,
                    error: Some(JSONRPCError {
                        code: -32011,
                        message: e.to_string(),
                        data: None,
                    }),
                };
                writeln!(std::io::stdout(), "{}", serde_json::to_string(&response)?)?;
                // we don't store the new_id when we already replied to prevent duplicates;
                // instead, we'll log an error if a server reply is received later and we already
                // replied
            } else {
                // all good, store the original id
                debug!("Adding ID mapping: {:?} -> {:?}", new_id, original_id);
                state_guard.id_map.insert(new_id.clone(), original_id);
            }

            Ok(())
        }
        JSONRPCMessage::Response(response) => {
            // Extract the proper key for lookup
            let lookup_key = response.id.to_string();
            let id_map_entry = state_guard.id_map.remove(&lookup_key);

            if let Some(original_id) = id_map_entry {
                drop(state_guard);
                // Clone original_id for later use in logging
                let original_id_clone = original_id.clone();

                // Replace the ID with the original one
                let mut modified_response = response.clone();
                modified_response.id = original_id;

                // Write the response to stdout
                info!(
                    "Forwarding response to client with original ID: {}",
                    original_id_clone
                );
                sse::forward_message(
                    &serde_json::to_value(&modified_response)?,
                    &endpoint,
                    receive_timeout_ms,
                )
                .await?;
            } else {
                error!(
                    "Did not find original ID for response: {}. Discarding response!",
                    response.id
                );
            }
            Ok(())
        }
        // must be a notification without ID, can forward as is
        notification => {
            // Release the lock before making external request
            drop(state_guard);

            sse::forward_message(
                &serde_json::to_value(&notification)?,
                &endpoint,
                receive_timeout_ms,
            )
            .await?;
            Ok(())
        }
    }
}

/// Reply with a disconnected error message
fn reply_disconnected(id: &RequestId) -> Result<()> {
    let response = JSONRPCResponse {
        jsonrpc: "2.0".to_string(),
        id: id.clone(),
        result: None,
        error: Some(JSONRPCError {
            code: -32010,
            message: "Server not connected. The SSE endpoint is currently not available. Please ensure it is running and retry.".to_string(),
            data: None,
        }),
    };

    writeln!(std::io::stdout(), "{}", serde_json::to_string(&response)?)?;
    Ok(())
}

/// Generate a random ID
fn generate_id() -> String {
    Uuid::now_v7().to_string()
}
