use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub enum NumberOrString {
    Number(u32),
    String(Arc<str>),
}

impl std::fmt::Display for NumberOrString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NumberOrString::Number(n) => n.fmt(f),
            NumberOrString::String(s) => s.fmt(f),
        }
    }
}

impl Serialize for NumberOrString {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            NumberOrString::Number(n) => n.serialize(serializer),
            NumberOrString::String(s) => s.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for NumberOrString {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value: Value = Deserialize::deserialize(deserializer)?;
        match value {
            Value::Number(n) => Ok(NumberOrString::Number(
                n.as_u64()
                    .ok_or(serde::de::Error::custom("Expect an integer"))? as u32,
            )),
            Value::String(s) => Ok(NumberOrString::String(s.into())),
            _ => Err(serde::de::Error::custom("Expect number or string")),
        }
    }
}

pub type RequestId = NumberOrString;

/// JSON-RPC 2.0 request
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JSONRPCRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 response
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JSONRPCResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JSONRPCError>,
}

/// JSON-RPC 2.0 error
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JSONRPCError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 notification
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JSONRPCNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// Any JSON-RPC 2.0 message (request, response, or notification)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum JSONRPCMessage {
    Request(JSONRPCRequest),
    Response(JSONRPCResponse),
    Notification(JSONRPCNotification),
}

/// SSE event types
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SSEEvent {
    Endpoint(String),
    Message(JSONRPCMessage),
    Other(String, String),
}

/// Application state
pub struct AppState {
    /// URL of the SSE server
    pub url: String,
    /// Endpoint URL for sending HTTP requests
    pub endpoint: Option<String>,
    /// Maximum time to try reconnecting in seconds (None = infinity)
    pub max_disconnected_time: Option<u64>,
    /// When we were disconnected
    pub disconnected_since: Option<std::time::Instant>,
    /// Current state of the application
    pub state: ProxyState,
    /// Number of connection attempts
    pub connect_tries: u32,
    /// The initialization message (for reconnecting)
    pub init_message: Option<JSONRPCRequest>,
    /// Map of generated IDs to original IDs
    pub id_map: HashMap<String, RequestId>,
    /// Buffer for holding messages during reconnection
    pub in_buf: Vec<JSONRPCMessage>,
    /// Buffer mode (store or fail)
    pub buf_mode: BufferMode,
}

/// Buffer mode
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferMode {
    Store,
    Fail,
}

/// Proxy state
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyState {
    Connecting,
    Connected,
    Disconnected,
    WaitingForEndpoint,
    WaitingForClientInit,
    WaitingForServerInit(RequestId),
    WaitingForServerInitHidden(RequestId),
    WaitingForClientInitialized,
}

impl AppState {
    pub fn new(url: String, max_disconnected_time: Option<u64>) -> Self {
        Self {
            url,
            endpoint: None,
            max_disconnected_time,
            disconnected_since: None,
            state: ProxyState::Connecting,
            connect_tries: 0,
            init_message: None,
            id_map: HashMap::new(),
            in_buf: Vec::new(),
            buf_mode: BufferMode::Store,
        }
    }

    pub fn connected(&mut self) {
        self.state = ProxyState::Connected;
        self.connect_tries = 0;
        self.disconnected_since = None;
        self.buf_mode = BufferMode::Store;
    }

    pub fn disconnected_too_long(&self) -> bool {
        match (self.max_disconnected_time, self.disconnected_since) {
            (Some(max_time), Some(since)) => since.elapsed().as_secs() > max_time,
            _ => false,
        }
    }
}
