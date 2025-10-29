use crate::state::{AppState, BufferMode, ProxyState, ReconnectFailureReason};
use crate::{DISCONNECTED_ERROR_CODE, SseClientType, StdoutSink, TRANSPORT_SEND_ERROR_CODE};
use anyhow::{Context, Result, anyhow};
use futures::FutureExt;
use futures::SinkExt;
use rmcp::model::{
    ClientJsonRpcMessage, ClientNotification, ClientRequest, ErrorData, RequestId,
    ServerJsonRpcMessage,
};
use std::time::Duration;
use tracing::{debug, error, info};
use uuid::Uuid;

/// Generates a random UUID for request IDs
pub(crate) fn generate_id() -> String {
    Uuid::now_v7().to_string()
}

/// Sends a disconnected error response
pub(crate) async fn reply_disconnected(id: &RequestId, stdout_sink: &mut StdoutSink) -> Result<()> {
    let error_response = ServerJsonRpcMessage::error(
        ErrorData::new(
            DISCONNECTED_ERROR_CODE,
            "Server not connected. The SSE endpoint is currently not available. Please ensure it is running and retry.".to_string(),
            None,
        ),
        id.clone(),
    );

    if let Err(e) = stdout_sink.send(error_response).await {
        error!("Error writing disconnected error response to stdout: {}", e);
    }

    Ok(())
}

pub(crate) async fn connect(app_state: &AppState) -> Result<SseClientType> {
    // this function should try sending a POST request to the sse_url and see if
    // the server responds with 405 method not supported. If so, it should call
    // connect_with_sse, otherwise it should call connect_with_streamable.
    let mut headers = app_state.headers.clone().unwrap_or_default();
    headers.insert(
        "Accept",
        "application/json,text/event-stream".parse().unwrap(),
    );
    headers.insert("Content-Type", "application/json".parse().unwrap());
    let result = reqwest::Client::new()
        .post(app_state.url.clone())
        .headers(headers)
        .body(r#"{"jsonrpc":"2.0","id":"init","method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"0.1.0"}}}"#)
        .send()
        .await?;

    if result.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        debug!("Server responded with 405, using SSE transport");
        return connect_with_sse(app_state).await;
    } else if result.status().is_success() {
        debug!("Server responded successfully, using streamable transport");
        return connect_with_streamable(app_state).await;
    } else {
        error!("Server returned unexpected status: {}", result.status());
        anyhow::bail!("Server returned unexpected status: {}", result.status());
    }
}

pub(crate) async fn connect_with_streamable(app_state: &AppState) -> Result<SseClientType> {
    let mut builder = reqwest::Client::builder();
    if let Some(headers) = app_state.headers.clone() {
        builder = builder.default_headers(headers);
    }

    let result = rmcp::transport::StreamableHttpClientTransport::with_client(
        reqwest::Client::default(),
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig {
            uri: app_state.url.clone().into(),
            // we don't want the sdk to perform any retries
            retry_config: std::sync::Arc::new(rmcp::transport::common::client_side_sse::NeverRetry),
            auth_header: None,
            channel_buffer_capacity: 16,
            allow_stateless: true,
        },
    );

    Ok(SseClientType::Streamable(result))
}

pub(crate) async fn connect_with_sse(app_state: &AppState) -> Result<SseClientType> {
    let mut builder = reqwest::Client::builder();
    if let Some(headers) = app_state.headers.clone() {
        builder = builder.default_headers(headers);
    }

    let result = rmcp::transport::SseClientTransport::start_with_client(
        builder.build().context("failed to build reqwest client")?,
        rmcp::transport::sse_client::SseClientConfig {
            sse_endpoint: app_state.url.clone().into(),
            // we don't want the sdk to perform any retries
            retry_policy: std::sync::Arc::new(
                rmcp::transport::common::client_side_sse::FixedInterval {
                    max_times: Some(0),
                    duration: Duration::from_millis(0),
                },
            ),
            use_message_endpoint: None,
        },
    )
    .await;

    match result {
        Ok(transport) => {
            info!("Successfully reconnected to SSE server");
            Ok(SseClientType::Sse(transport))
        }
        Err(e) => {
            error!("Failed to reconnect: {}", e);
            Err(anyhow!("Connection failed: {}", e))
        }
    }
}

/// Attempts to reconnect to the SSE server with backoff.
/// Does not mutate AppState directly.
pub(crate) async fn try_reconnect(
    app_state: &AppState,
) -> Result<SseClientType, ReconnectFailureReason> {
    let backoff = app_state.get_backoff_duration();
    info!(
        "Attempting to reconnect in {}s (attempt {})",
        backoff.as_secs(),
        app_state.connect_tries
    );

    if app_state.disconnected_too_long() {
        error!("Reconnect timeout exceeded, giving up reconnection attempts");
        return Err(ReconnectFailureReason::TimeoutExceeded);
    }

    let result = connect(app_state).await;

    match result {
        Ok(transport) => {
            info!("Successfully reconnected to SSE server");
            Ok(transport)
        }
        Err(e) => {
            error!("Failed to reconnect: {}", e);
            Err(ReconnectFailureReason::ConnectionFailed(e))
        }
    }
}

/// Sends a JSON-RPC request to the SSE server and handles any transport errors.
/// Returns true if the send was successful, false otherwise.
pub(crate) async fn send_request_to_sse(
    transport: &mut SseClientType,
    request: ClientJsonRpcMessage,
    original_message: ClientJsonRpcMessage,
    stdout_sink: &mut StdoutSink,
    app_state: &mut AppState,
) -> Result<bool> {
    debug!("Sending request to SSE: {:?}", request);
    match transport.send(request.clone()).await {
        Ok(_) => Ok(true),
        Err(e) => {
            error!("Error sending to SSE: {}", e);
            app_state.handle_fatal_transport_error();
            app_state
                .maybe_handle_message_while_disconnected(original_message, stdout_sink)
                .await?;

            Ok(false)
        }
    }
}

/// Processes a client request message, handles ID mapping, sends it to the SSE server,
/// and handles any transport errors.
pub(crate) async fn process_client_request(
    message: ClientJsonRpcMessage,
    app_state: &mut AppState,
    transport: &mut SseClientType,
    stdout_sink: &mut StdoutSink,
) -> Result<()> {
    // Try mapping the ID first (for Response/Error cases).
    // If it returns None, the ID was unknown, so we skip processing/forwarding.
    let message = match app_state.map_client_response_error_id(message) {
        Some(msg) => msg,
        None => return Ok(()), // Skip forwarding if ID was not mapped
    };

    // Handle ping directly if disconnected
    match app_state
        .maybe_handle_message_while_disconnected(message.clone(), stdout_sink)
        .await
    {
        Err(_) => {}
        Ok(_) => return Ok(()),
    }

    match &message {
        ClientJsonRpcMessage::Request(req) => {
            if app_state.init_message.is_none() {
                if let ClientRequest::InitializeRequest(_) = req.request {
                    debug!("Stored client initialization message");
                    app_state.init_message = Some(message.clone());
                    app_state.state = ProxyState::WaitingForServerInit(req.id.clone());
                }
            }
        }
        ClientJsonRpcMessage::Notification(notification) => {
            if let ClientNotification::InitializedNotification(_) = notification.notification {
                if app_state.state == ProxyState::WaitingForClientInitialized {
                    debug!("Received client initialized notification, proxy fully connected.");
                    app_state.connected();
                } else {
                    debug!("Forwarding client initialized notification outside of expected state.");
                }
            }
        }
        _ => {}
    }

    // Process requests separately to map their IDs before sending
    let original_message = message.clone();
    if let ClientJsonRpcMessage::Request(req) = message {
        let request_id = req.id.clone();
        let mut req = req.clone();
        debug!("Forwarding request from stdin to SSE: {:?}", req);

        let new_id = generate_id();
        let new_request_id = RequestId::String(new_id.clone().into());
        req.id = new_request_id;
        app_state.id_map.insert(new_id, request_id.clone());

        let _success = send_request_to_sse(
            transport,
            ClientJsonRpcMessage::Request(req),
            original_message,
            stdout_sink,
            app_state,
        )
        .await?;
        return Ok(());
    }

    // Send other message types (Notifications, mapped Responses/Errors)
    debug!("Forwarding message from stdin to SSE: {:?}", message);
    if let Err(e) = transport.send(message).await {
        error!("Error sending message to SSE: {}", e);
        app_state.handle_fatal_transport_error();
    }

    Ok(())
}

/// Process buffered messages after a successful reconnection
pub(crate) async fn process_buffered_messages(
    app_state: &mut AppState,
    transport: &mut SseClientType,
    stdout_sink: &mut StdoutSink,
) -> Result<()> {
    let buffered_messages = std::mem::take(&mut app_state.in_buf);
    debug!("Processing {} buffered messages", buffered_messages.len());

    for message in buffered_messages {
        match &message {
            ClientJsonRpcMessage::Request(req) => {
                let request_id = req.id.clone();
                let mut req = req.clone();

                let new_id = generate_id();
                req.id = RequestId::String(new_id.clone().into());
                app_state.id_map.insert(new_id, request_id.clone());

                if let Err(e) = transport.send(ClientJsonRpcMessage::Request(req)).await {
                    error!("Error sending buffered request: {}", e);
                    let error_response = ServerJsonRpcMessage::error(
                        ErrorData::new(
                            TRANSPORT_SEND_ERROR_CODE,
                            format!("Transport error: {}", e),
                            None,
                        ),
                        request_id,
                    );
                    if let Err(write_err) = stdout_sink.send(error_response).await {
                        error!("Error writing error response to stdout: {}", write_err);
                    }
                }
            }
            _ => {
                // Notifications etc.
                if let Err(e) = transport.send(message.clone()).await {
                    error!("Error sending buffered message: {}", e);
                    // If sending a buffered notification fails, we probably just log it.
                    // Triggering another disconnect cycle might be excessive.
                }
            }
        }
    }
    Ok(())
}

/// Sends error responses for all buffered messages
pub(crate) async fn flush_buffer_with_errors(
    app_state: &mut AppState,
    stdout_sink: &mut StdoutSink,
) -> Result<()> {
    debug!(
        "Flushing buffer with errors: {} messages",
        app_state.in_buf.len()
    );

    let buffered_messages = std::mem::take(&mut app_state.in_buf);
    app_state.buf_mode = BufferMode::Fail;

    if !app_state.id_map.is_empty() {
        debug!("Clearing ID map with {} entries", app_state.id_map.len());
        app_state.id_map.clear();
    }

    for message in buffered_messages {
        if let ClientJsonRpcMessage::Request(request) = message {
            debug!("Sending error response for buffered request");
            reply_disconnected(&request.id, stdout_sink).await?;
        }
    }

    Ok(())
}

/// Initiates the post-reconnection handshake by sending the initialize request.
/// Sets the state to WaitingForServerInitHidden.
/// Returns Ok(true) if handshake initiated successfully (or not needed).
/// Returns Ok(false) if sending the init message failed (triggers disconnect).
pub(crate) async fn initiate_post_reconnect_handshake(
    app_state: &mut AppState,
    transport: &mut SseClientType,
    stdout_sink: &mut StdoutSink,
) -> Result<bool> {
    if let Some(init_msg) = &app_state.init_message {
        let id = if let ClientJsonRpcMessage::Request(req) = init_msg {
            req.id.clone()
        } else {
            error!("Stored init_message is not a request: {:?}", init_msg);
            return Ok(false);
        };

        debug!(
            "Initiating post-reconnect handshake by sending: {:?}",
            init_msg
        );
        app_state.state = ProxyState::WaitingForServerInitHidden(id.clone());

        if let Err(e) =
            process_client_request(init_msg.clone(), app_state, transport, stdout_sink).await
        {
            info!("Error resending init message during handshake: {}", e);
            app_state.handle_fatal_transport_error();
            Ok(false)
        } else {
            Ok(true)
        }
    } else {
        // If the init_message is None during a reconnect attempt, it's a fatal error.
        error!(
            "No initialization message stored. Cannot reconnect! This indicates a critical state issue."
        );
        // Return an Err to signal a fatal condition that should terminate the proxy.
        Err(anyhow::anyhow!(
            "Cannot perform reconnect handshake: init_message is missing"
        ))
    }
}

/// Send a heartbeat ping to check if the transport is still connected.
/// Returns Some(true) if alive, Some(false) if dead, None if check not needed.
pub(crate) async fn send_heartbeat_if_needed(
    app_state: &AppState,
    transport: &mut SseClientType,
) -> Option<bool> {
    if app_state.last_heartbeat.elapsed() > Duration::from_secs(5) {
        debug!("Checking SSE connection state due to inactivity...");
        match transport.receive().now_or_never() {
            Some(Some(_)) => {
                debug!("Heartbeat check: Received message/event, connection alive.");
                Some(true)
            }
            Some(None) => {
                debug!("Heartbeat check: Stream terminated, connection dead.");
                Some(false)
            }
            None => {
                debug!(
                    "Heartbeat check: No immediate message/event, state uncertain but assuming alive for now."
                );
                Some(true)
            }
        }
    } else {
        None
    }
}
