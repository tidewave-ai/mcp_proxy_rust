use crate::core::{
    flush_buffer_with_errors, generate_id, initiate_post_reconnect_handshake,
    process_buffered_messages, process_client_request, reply_disconnected,
    send_heartbeat_if_needed, try_reconnect,
};
use crate::{SseClientType, StdoutSink};
use anyhow::Result;
use futures::SinkExt;
use rmcp::model::{
    ClientJsonRpcMessage, ClientNotification, ClientRequest, EmptyResult, InitializedNotification,
    InitializedNotificationMethod, RequestId, ServerJsonRpcMessage,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::Sender;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};


/// Reasons why a reconnection attempt might fail.
#[derive(Debug)]
pub enum ReconnectFailureReason {
    TimeoutExceeded,
    ConnectionFailed(anyhow::Error),
}

/// Buffer mode for message handling during disconnection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferMode {
    Store,
    Fail,
}

/// Proxy state to track connection and message handling
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyState {
    Connecting,
    Connected,
    Disconnected,
    WaitingForClientInit,
    WaitingForServerInit(RequestId),
    WaitingForServerInitHidden(RequestId),
    WaitingForClientInitialized,
}

/// Application state for the proxy
#[derive(Debug)]
pub struct AppState {
    /// URL of the SSE server
    pub url: String,
    /// Maximum time to try reconnecting in seconds (None = infinity)
    pub max_disconnected_time: Option<u64>,
    /// When we were disconnected
    pub disconnected_since: Option<Instant>,
    /// Current state of the application
    pub state: ProxyState,
    /// Number of connection attempts
    pub connect_tries: u32,
    /// The initialization message (for reconnection)
    pub init_message: Option<ClientJsonRpcMessage>,
    /// Map of generated IDs to original IDs (Client -> Server flow)
    pub id_map: HashMap<String, RequestId>,
    /// Buffer for holding messages during reconnection
    pub in_buf: Vec<ClientJsonRpcMessage>,
    /// Buffer mode (store or fail)
    pub buf_mode: BufferMode,
    /// Whether a flush timer is in progress
    pub flush_timer_active: bool,
    /// Channel sender for reconnect events
    pub reconnect_tx: Option<Sender<()>>,
    /// Channel sender for timer events
    pub timer_tx: Option<Sender<()>>,
    /// Whether reconnect is already scheduled
    pub reconnect_scheduled: bool,
    /// Whether the transport is still valid
    pub transport_valid: bool,
    /// Time of last heartbeat check
    pub last_heartbeat: Instant,
    /// Whether to enable protocol version rewriting
    pub rewrite_protocol_version: bool,
}

impl AppState {
    pub fn new(url: String, max_disconnected_time: Option<u64>, rewrite_protocol_version: bool) -> Self {
        Self {
            url,
            max_disconnected_time,
            disconnected_since: None,
            state: ProxyState::Connecting,
            connect_tries: 0,
            init_message: None,
            id_map: HashMap::new(),
            in_buf: Vec::new(),
            buf_mode: BufferMode::Store,
            flush_timer_active: false,
            reconnect_tx: None,
            timer_tx: None,
            reconnect_scheduled: false,
            transport_valid: true,
            last_heartbeat: Instant::now(),
            rewrite_protocol_version,
        }
    }

    pub fn connected(&mut self) {
        self.state = ProxyState::Connected;
        self.connect_tries = 0;
        self.disconnected_since = None;
        self.buf_mode = BufferMode::Store;
        self.reconnect_scheduled = false;
        self.transport_valid = true;
        self.last_heartbeat = Instant::now();
    }

    pub fn disconnected(&mut self) {
        if self.state != ProxyState::Disconnected {
            debug!("State changing to disconnected");
            self.state = ProxyState::Disconnected;
            self.disconnected_since = Some(Instant::now());
            self.buf_mode = BufferMode::Store;
            self.transport_valid = false;
        }
        self.connect_tries += 1;
    }

    pub fn disconnected_too_long(&self) -> bool {
        match (self.max_disconnected_time, self.disconnected_since) {
            (Some(max_time), Some(since)) => since.elapsed().as_secs() > max_time,
            _ => false,
        }
    }

    pub fn get_backoff_duration(&self) -> Duration {
        let seconds = std::cmp::min(2u64.pow(self.connect_tries.saturating_sub(1)), 8);
        Duration::from_secs(seconds)
    }

    pub fn schedule_reconnect(&mut self) {
        if !self.reconnect_scheduled {
            if let Some(tx) = &self.reconnect_tx {
                let tx_clone = tx.clone();
                let backoff = self.get_backoff_duration();
                debug!("Scheduling reconnect in {}s", backoff.as_secs());
                tokio::spawn(async move {
                    sleep(backoff).await;
                    let _ = tx_clone.send(()).await;
                });
                self.reconnect_scheduled = true;
            }
        } else {
            debug!("Reconnect already scheduled, skipping");
        }
    }

    pub fn schedule_flush_timer(&mut self) {
        if !self.flush_timer_active {
            if let Some(tx) = &self.timer_tx {
                debug!("Scheduling flush timer for 20s");
                self.flush_timer_active = true;
                let tx_clone = tx.clone();
                tokio::spawn(async move {
                    sleep(Duration::from_secs(20)).await;
                    let _ = tx_clone.send(()).await;
                });
            }
        } else {
            debug!("Flush timer already active, skipping");
        }
    }

    pub fn update_heartbeat(&mut self) {
        self.last_heartbeat = Instant::now();
    }

    /// Handles common logic for fatal transport errors:
    /// Sets state to disconnected and schedules timer/reconnect.
    pub fn handle_fatal_transport_error(&mut self) {
        if self.state != ProxyState::Disconnected {
            self.disconnected();
            self.schedule_flush_timer();
            self.schedule_reconnect();
        }
    }

    /// Handles messages received from stdin.
    /// Returns Ok(true) to continue processing, Ok(false) to break the main loop.
    pub(crate) async fn handle_stdin_message(
        &mut self,
        msg: Option<
            Result<ClientJsonRpcMessage, rmcp::transport::async_rw::JsonRpcMessageCodecError>,
        >,
        transport: &mut SseClientType,
        stdout_sink: &mut StdoutSink,
    ) -> Result<bool> {
        match msg {
            Some(Ok(message)) => {
                process_client_request(message, self, transport, stdout_sink).await?;
                Ok(true)
            }
            Some(Err(e)) => {
                error!("Error reading from stdin: {}", e);
                Ok(false)
            }
            None => {
                info!("Stdin stream ended.");
                Ok(false)
            }
        }
    }

    /// Handles messages received from the SSE transport.
    /// Returns Ok(true) to continue processing, Ok(false) to break the main loop.
    pub(crate) async fn handle_sse_message(
        &mut self,
        result: Option<ServerJsonRpcMessage>,
        transport: &mut SseClientType,
        stdout_sink: &mut StdoutSink,
    ) -> Result<bool> {
        debug!("Received SSE message: {:?}", result);
        match result {
            Some(mut message) => {
                self.update_heartbeat();

                // --- Handle Server-Initiated Request ---
                if let ServerJsonRpcMessage::Request(mut req) = message {
                    let server_id = req.id.clone();
                    let proxy_id_str = generate_id();
                    let proxy_id = RequestId::String(proxy_id_str.clone().into());
                    debug!(
                        "Mapping server request ID {} to proxy ID {}",
                        server_id, proxy_id
                    );
                    self.id_map.insert(proxy_id_str, server_id);
                    req.id = proxy_id;
                    message = ServerJsonRpcMessage::Request(req);
                    // Now fall through to forward the modified request
                }
                // --- End Server-Initiated Request Handling ---
                else {
                    match self.map_server_response_error_id(message) {
                        Some(mapped_message) => message = mapped_message,
                        None => return Ok(true), // Skip forwarding this message
                    }
                    // --- Handle Initialization Response --- (Only for Response/Error)
                    let is_init_response = match &message {
                        ServerJsonRpcMessage::Response(response) => match self.state {
                            ProxyState::WaitingForServerInit(ref init_request_id) => {
                                *init_request_id == response.id
                            }
                            ProxyState::WaitingForServerInitHidden(ref init_request_id) => {
                                *init_request_id == response.id
                            }
                            _ => false,
                        },
                        // Don't treat errors related to init ID as special init responses
                        _ => false,
                    };

                    debug!(
                        "Handling initialization response - state: {:?}, message: {:?}, is_init_response: {}",
                        self.state, message, is_init_response
                    );

                    if is_init_response {
                        let was_hidden =
                            matches!(self.state, ProxyState::WaitingForServerInitHidden(_));
                        if was_hidden {
                            self.connected();
                            debug!("Reconnection successful, received hidden init response");
                            let initialized_notification = ClientJsonRpcMessage::notification(
                                ClientNotification::InitializedNotification(
                                    InitializedNotification {
                                        method: InitializedNotificationMethod,
                                        extensions: rmcp::model::Extensions::default(),
                                    },
                                ),
                            );
                            if let Err(e) = transport.send(initialized_notification).await {
                                error!(
                                    "Error sending initialized notification post-reconnect: {}",
                                    e
                                );
                                self.handle_fatal_transport_error();
                            } else {
                                process_buffered_messages(self, transport, stdout_sink).await?;
                            }
                            return Ok(true); // Don't forward the init response
                        } else {
                            debug!(
                                "Initial connection successful, received init response. Waiting for client initialized."
                            );
                            self.state = ProxyState::WaitingForClientInitialized;
                        }
                    }
                    // --- End Initialization Response Handling ---
                }

                // Apply protocol version rewriting if enabled
                let message = if self.rewrite_protocol_version {
                    self.rewrite_protocol_version(message)
                } else {
                    message
                };

                // Forward the (potentially modified) message to stdout
                // This now handles mapped server requests, mapped responses/errors, and notifications
                debug!("Forwarding from SSE to stdout: {:?}", message);
                if let Err(e) = stdout_sink.send(message).await {
                    error!("Error writing to stdout: {}", e);
                    return Ok(false);
                }

                Ok(true)
            }
            None => {
                debug!("SSE stream ended (Fatal error in transport) - trying to reconnect");
                self.handle_fatal_transport_error();
                Ok(true)
            }
        }
    }

    pub(crate) async fn maybe_handle_message_while_disconnected(
        &mut self,
        message: ClientJsonRpcMessage,
        stdout_sink: &mut StdoutSink,
    ) -> Result<()> {
        if self.state != ProxyState::Disconnected {
            return Err(anyhow::anyhow!("Not disconnected"));
        }

        // Handle ping directly if disconnected
        if let ClientJsonRpcMessage::Request(ref req) = message {
            if let ClientRequest::PingRequest(_) = &req.request {
                debug!(
                    "Received Ping request while disconnected, replying directly: {:?}",
                    req.id
                );
                let response = ServerJsonRpcMessage::response(
                    rmcp::model::ServerResult::EmptyResult(EmptyResult {}),
                    req.id.clone(),
                );
                if let Err(e) = stdout_sink.send(response).await {
                    error!("Error sending direct ping response to stdout: {}", e);
                }
                return Ok(());
            }
            if self.buf_mode == BufferMode::Store {
                debug!("Buffering request for later retry");
                self.in_buf.push(message);
            } else {
                reply_disconnected(&req.id, stdout_sink).await?;
            }
        }

        Ok(())
    }

    /// Handles the reconnect signal.
    /// Returns the potentially new transport if reconnection was successful.
    pub(crate) async fn handle_reconnect_signal(
        &mut self,
        stdout_sink: &mut StdoutSink,
    ) -> Result<Option<SseClientType>> {
        debug!("Received reconnect signal");
        self.reconnect_scheduled = false;

        if self.state == ProxyState::Disconnected {
            match try_reconnect(self).await {
                Ok(mut new_transport) => {
                    self.transport_valid = true;

                    initiate_post_reconnect_handshake(self, &mut new_transport, stdout_sink)
                        .await
                        .map(|success| {
                            if success {
                                Some(new_transport)
                            } else {
                                None // Handshake failed non-fatally, no new transport
                            }
                        })
                }
                Err(reason) => {
                    self.connect_tries += 1;
                    match reason {
                        ReconnectFailureReason::TimeoutExceeded => {
                            error!(
                                "Reconnect attempt {} failed: Timeout exceeded",
                                self.connect_tries
                            );
                            info!("Disconnected too long, flushing buffer.");
                            flush_buffer_with_errors(self, stdout_sink).await?;
                        }
                        ReconnectFailureReason::ConnectionFailed(e) => {
                            error!(
                                "Reconnect attempt {} failed: Connection error: {}",
                                self.connect_tries, e
                            );
                            if !self.disconnected_too_long() {
                                self.schedule_reconnect();
                            } else {
                                info!(
                                    "Disconnected too long after failed connect, flushing buffer."
                                );
                                flush_buffer_with_errors(self, stdout_sink).await?;
                            }
                        }
                    }
                    Ok(None)
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Handles the flush timer signal.
    pub(crate) async fn handle_timer_signal(&mut self, stdout_sink: &mut StdoutSink) -> Result<()> {
        debug!("Received flush timer signal");
        self.flush_timer_active = false;
        if self.state == ProxyState::Disconnected {
            info!("Still disconnected after 20 seconds, flushing buffer with errors");
            flush_buffer_with_errors(self, stdout_sink).await?;
        }
        Ok(())
    }

    /// Handles the heartbeat interval tick.
    pub(crate) async fn handle_heartbeat_tick(
        &mut self,
        transport: &mut SseClientType,
    ) -> Result<()> {
        if self.state == ProxyState::Connected {
            let check_result = send_heartbeat_if_needed(self, transport).await;
            match check_result {
                Some(true) => {
                    self.update_heartbeat();
                }
                Some(false) => {
                    self.handle_fatal_transport_error();
                }
                None => {}
            }
        }
        Ok(())
    }

    // --- ID Mapping Helpers ---
    fn lookup_and_remove_original_id(&mut self, current_id: &RequestId) -> Option<RequestId> {
        let lookup_key = current_id.to_string();
        self.id_map.remove(&lookup_key)
    }

    // Add the client mapping logic here
    pub(crate) fn map_client_response_error_id(
        &mut self,
        message: ClientJsonRpcMessage,
    ) -> Option<ClientJsonRpcMessage> {
        let (id_to_check, is_response_or_error) = match &message {
            ClientJsonRpcMessage::Response(res) => (Some(res.id.clone()), true),
            ClientJsonRpcMessage::Error(err) => (Some(err.id.clone()), true),
            _ => (None, false), // Requests or Notifications are not mapped back this way
        };

        if is_response_or_error {
            if let Some(current_id) = id_to_check {
                if let Some(original_id) = self.lookup_and_remove_original_id(&current_id) {
                    debug!(
                        "Mapping client message ID {} back to original server ID: {}",
                        current_id, original_id
                    );
                    return Some(match message {
                        ClientJsonRpcMessage::Response(mut res) => {
                            res.id = original_id;
                            ClientJsonRpcMessage::Response(res)
                        }
                        ClientJsonRpcMessage::Error(mut err) => {
                            err.id = original_id;
                            ClientJsonRpcMessage::Error(err)
                        }
                        _ => message, // Should not happen
                    });
                } else {
                    // ID not found, return None to prevent forwarding
                    warn!(
                        "Received client response/error with unknown ID: {}, skipping forwarding.",
                        current_id
                    );
                    return None;
                }
            } else {
                // Error message has no ID (should not happen for JSON-RPC errors)
                warn!("Received client error message without an ID, skipping forwarding.");
                return None;
            }
        }
        // Not a response/error, return Some(original_message)
        Some(message)
    }

    /// Checks if a server message (Response or Error) has an ID corresponding
    /// to a mapped client request ID. If found, replaces the message's ID with
    /// the original client ID and returns `Some` modified message.
    /// Otherwise, returns `None`.
    // Renamed from try_map_server_message_id and made private
    pub(crate) fn map_server_response_error_id(
        &mut self,
        message: ServerJsonRpcMessage,
    ) -> Option<ServerJsonRpcMessage> {
        let (id_to_check, is_response_or_error) = match &message {
            ServerJsonRpcMessage::Response(res) => (Some(res.id.clone()), true),
            ServerJsonRpcMessage::Error(err) => (Some(err.id.clone()), true),
            _ => (None, false), // Notifications or Requests are not mapped back
        };

        if is_response_or_error {
            if let Some(current_id) = id_to_check {
                if let Some(original_id) = self.lookup_and_remove_original_id(&current_id) {
                    debug!(
                        "Mapping server message ID {} back to original client ID: {}",
                        current_id, original_id
                    );
                    return Some(match message {
                        ServerJsonRpcMessage::Response(mut res) => {
                            res.id = original_id;
                            ServerJsonRpcMessage::Response(res)
                        }
                        ServerJsonRpcMessage::Error(mut err) => {
                            err.id = original_id;
                            ServerJsonRpcMessage::Error(err)
                        }
                        _ => message, // Should not happen due to is_response_or_error check
                    });
                } else {
                    // ID not found, return None to prevent forwarding
                    warn!(
                        "Received server response/error with unknown ID: {}, skipping forwarding.",
                        current_id
                    );
                    return None;
                }
            } else {
                // Error message has no ID (should not happen for JSON-RPC errors)
                warn!("Received server error message without an ID, skipping forwarding.");
                return None;
            }
        }
        // Not a response/error, return Some(original_message)
        Some(message)
    }

    /// Rewrites protocol version in server messages for compatibility
    /// Changes "2025-03-26" to "2024-11-05" in initialize responses
    fn rewrite_protocol_version(&self, message: ServerJsonRpcMessage) -> ServerJsonRpcMessage {
        match message {
            ServerJsonRpcMessage::Response(response) => {
                // Serialize the response to JSON, modify it, then deserialize back
                if let Ok(mut json_value) = rmcp::serde_json::to_value(&response) {
                    if let Some(result) = json_value.get_mut("result") {
                        let mut modified = false;

                        // Check for protocolVersion in the result
                        if let Some(protocol_version) = result.get_mut("protocolVersion") {
                            if protocol_version == "2025-03-26" {
                                debug!("Rewriting protocolVersion from '2025-03-26' to '2024-11-05' for compatibility");
                                *protocol_version = rmcp::serde_json::Value::String("2024-11-05".to_string());
                                modified = true;
                            }
                        }

                        if modified {
                            if let Ok(rewritten_response) = rmcp::serde_json::from_value(json_value) {
                                return ServerJsonRpcMessage::Response(rewritten_response);
                            }
                        }
                    }
                }
                ServerJsonRpcMessage::Response(response)
            }
            _ => message, // Don't modify other message types
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_protocol_version_rewriting() {
        let app_state = AppState::new("http://test".to_string(), None, true);

        // Create a mock JSON response that contains protocolVersion
        let mock_json = rmcp::serde_json::json!({
            "jsonrpc": "2.0",
            "id": "test-id",
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {
                    "tools": {}
                }
            }
        });

        // Convert to ServerJsonRpcMessage and test rewriting
        if let Ok(message) = rmcp::serde_json::from_value::<ServerJsonRpcMessage>(mock_json) {
            let rewritten = app_state.rewrite_protocol_version(message);

            // Convert back to JSON to check the result
            if let Ok(rewritten_json) = rmcp::serde_json::to_value(&rewritten) {
                if let Some(result) = rewritten_json.get("result") {
                    if let Some(protocol_version) = result.get("protocolVersion") {
                        assert_eq!(protocol_version, "2024-11-05");
                    }
                }
            }
        }
    }
}
