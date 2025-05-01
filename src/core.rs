use crate::state::{AppState, BufferMode, ProxyState, ReconnectFailureReason};
use crate::{DISCONNECTED_ERROR_CODE, SseClientTransport, StdoutSink, TRANSPORT_SEND_ERROR_CODE};
use anyhow::Result;
use futures::{FutureExt, SinkExt, StreamExt};
use rmcp::{
    model::{
        ClientJsonRpcMessage, ClientRequest, EmptyResult, ErrorData, RequestId,
        ServerJsonRpcMessage,
    },
    transport::sse::{ReqwestSseClient, SseTransport, SseTransportRetryConfig},
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
        info!("Error writing disconnected error response to stdout: {}", e);
    }

    Ok(())
}

/// Attempts to reconnect to the SSE server with backoff.
/// Does not mutate AppState directly.
pub(crate) async fn try_reconnect(
    app_state: &AppState,
) -> Result<SseClientTransport, ReconnectFailureReason> {
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

    let client = ReqwestSseClient::new(&app_state.url)
        .map_err(|e| ReconnectFailureReason::ConnectionFailed(e.into()))?;

    match SseTransport::start_with_client(client).await {
        Ok(mut new_transport) => {
            new_transport.retry_config = SseTransportRetryConfig {
                max_times: Some(0),
                min_duration: Duration::from_millis(0),
            };
            info!("Successfully reconnected to SSE server");
            Ok(new_transport)
        }
        Err(e) => {
            info!("Failed to reconnect: {}", e);
            Err(ReconnectFailureReason::ConnectionFailed(e.into()))
        }
    }
}

/// Sends a JSON-RPC request to the SSE server and handles any transport errors.
/// Returns true if the send was successful, false otherwise.
pub(crate) async fn send_request_to_sse(
    transport: &mut SseClientTransport,
    request: ClientJsonRpcMessage,
    original_id: RequestId,
    stdout_sink: &mut StdoutSink,
    app_state: &mut AppState,
) -> Result<bool> {
    debug!("Sending request to SSE: {:?}", request);
    match transport.send(request.clone()).await {
        Ok(_) => Ok(true),
        Err(e) => {
            info!("Error sending to SSE: {}", e);
            app_state.disconnected();

            if app_state.buf_mode == BufferMode::Store {
                debug!("Buffering request for later retry");
                app_state.in_buf.push(request);
                app_state.schedule_flush_timer();
                app_state.schedule_reconnect();
            } else {
                let error_response = ServerJsonRpcMessage::error(
                    ErrorData::new(
                        TRANSPORT_SEND_ERROR_CODE,
                        format!("Transport error: {}", e),
                        None,
                    ),
                    original_id,
                );
                if let Err(write_err) = stdout_sink.send(error_response).await {
                    info!("Error writing error response to stdout: {}", write_err);
                }
            }
            Ok(false)
        }
    }
}

/// Processes a client request message, handles ID mapping, sends it to the SSE server,
/// and handles any transport errors.
pub(crate) async fn process_client_request(
    message: ClientJsonRpcMessage,
    app_state: &mut AppState,
    transport: &mut SseClientTransport,
    stdout_sink: &mut StdoutSink,
) -> Result<()> {
    // Handle ping directly if disconnected
    if let ClientJsonRpcMessage::Request(ref req) = message {
        if let ClientRequest::PingRequest(_) = &req.request {
            if app_state.state == ProxyState::Disconnected {
                debug!(
                    "Received Ping request while disconnected, replying directly: {:?}",
                    req.id
                );
                let response = ServerJsonRpcMessage::response(
                    rmcp::model::ServerResult::EmptyResult(EmptyResult {}),
                    req.id.clone(),
                );
                if let Err(e) = stdout_sink.send(response).await {
                    info!("Error sending direct ping response to stdout: {}", e);
                }
                return Ok(());
            }
        }
    }

    // Try mapping the ID first (for Response/Error cases).
    // If it returns None, the ID was unknown, so we skip processing/forwarding.
    let message = match app_state.map_client_response_error_id(message) {
        Some(msg) => msg,
        None => return Ok(()), // Skip forwarding if ID was not mapped
    };

    // Check if disconnected and buffer if necessary (both requests and mapped responses/errors)
    if app_state.state == ProxyState::Disconnected && app_state.buf_mode == BufferMode::Store {
        debug!("Buffering message while disconnected: {:?}", message);
        app_state.in_buf.push(message);
        return Ok(());
    }

    // Process requests separately to map their IDs before sending
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
            request_id, // Pass the original ID for potential error reporting
            stdout_sink,
            app_state,
        )
        .await?;
        return Ok(());
    }

    // Send other message types (Notifications, mapped Responses/Errors)
    debug!("Forwarding message from stdin to SSE: {:?}", message);
    if let Err(e) = transport.send(message).await {
        info!("Error sending message to SSE: {}", e);
        app_state.handle_fatal_transport_error();
    }

    Ok(())
}

/// Process buffered messages after a successful reconnection
pub(crate) async fn process_buffered_messages(
    app_state: &mut AppState,
    transport: &mut SseClientTransport,
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
                    info!("Error sending buffered request: {}", e);
                    let error_response = ServerJsonRpcMessage::error(
                        ErrorData::new(
                            TRANSPORT_SEND_ERROR_CODE,
                            format!("Transport error: {}", e),
                            None,
                        ),
                        request_id,
                    );
                    if let Err(write_err) = stdout_sink.send(error_response).await {
                        info!("Error writing error response to stdout: {}", write_err);
                    }
                }
            }
            _ => {
                // Notifications etc.
                if let Err(e) = transport.send(message.clone()).await {
                    info!("Error sending buffered message: {}", e);
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
    transport: &mut SseClientTransport,
    stdout_sink: &mut StdoutSink,
) -> Result<bool> {
    if let Some(init_msg) = &app_state.init_message {
        let id = if let ClientJsonRpcMessage::Request(req) = init_msg {
            req.id.clone()
        } else {
            error!("Stored init_message is not a request: {:?}", init_msg);
            process_buffered_messages(app_state, transport, stdout_sink).await?;
            app_state.state = ProxyState::Connected;
            return Ok(true);
        };

        debug!(
            "Initiating post-reconnect handshake by sending: {:?}",
            init_msg
        );
        app_state.state = ProxyState::WaitingForServerInitHidden(id.clone());

        if let Err(e) = transport.send(init_msg.clone()).await {
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
    transport: &mut SseClientTransport,
) -> Option<bool> {
    if app_state.last_heartbeat.elapsed() > Duration::from_secs(5) {
        debug!("Checking SSE connection state due to inactivity...");
        match transport.next().now_or_never() {
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
