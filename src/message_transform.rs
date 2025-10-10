//! Message transformation layer to handle MCP specification compliance issues
//!
//! The rmcp library expects method name "notifications/initialized" but the MCP spec
//! defines the initialized notification with method name "initialized". This module
//! provides transformation to bridge this gap.

use bytes::BytesMut;
use rmcp::model::ClientJsonRpcMessage;
use serde_json::Value;
use tokio_util::codec::{Decoder, Encoder};
use tracing::debug;

/// A codec wrapper that transforms problematic MCP messages before passing them to rmcp
pub struct McpCompatCodec {
    inner: rmcp::transport::async_rw::JsonRpcMessageCodec<ClientJsonRpcMessage>,
}

impl Default for McpCompatCodec {
    fn default() -> Self {
        Self {
            inner: rmcp::transport::async_rw::JsonRpcMessageCodec::default(),
        }
    }
}

impl McpCompatCodec {
    /// Transform raw JSON string to be compatible with rmcp expectations
    fn transform_message(json_str: &str) -> Result<String, serde_json::Error> {
        let mut value: Value = serde_json::from_str(json_str)?;
        
        // Check if this is a JSON-RPC message with method "initialized" or "progress"
        if let Some(obj) = value.as_object_mut() {
            if let Some(method) = obj.get("method") {
                if let Some(method_str) = method.as_str() {
                    match method_str {
                        "initialized" => {
                            debug!("Transforming 'initialized' method to 'notifications/initialized' for rmcp compatibility");
                            obj.insert("method".to_string(), Value::String("notifications/initialized".to_string()));
                            
                            // If params is empty or missing, remove it as rmcp expects no params for this notification
                            if let Some(params) = obj.get("params") {
                                if params.is_object() && params.as_object().unwrap().is_empty() {
                                    obj.remove("params");
                                } else if params.is_null() {
                                    obj.remove("params");
                                }
                            }
                        },
                        "progress" => {
                            debug!("Filtering out 'progress' notification");
                            
                            // Return empty string to indicate this message should be filtered out
                            return Ok(String::new());
                        },
                        _ => {
                            // No transformation needed for other methods
                        }
                    }
                }
            }
        }
        
        Ok(serde_json::to_string(&value)?)
    }
}

impl Decoder for McpCompatCodec {
    type Item = ClientJsonRpcMessage;
    type Error = rmcp::transport::async_rw::JsonRpcMessageCodecError;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        // First, let's see if we can find a complete JSON message
        let start_pos = src.iter().position(|&b| b == b'{');
        if start_pos.is_none() {
            return Ok(None);
        }

        let start_pos = start_pos.unwrap();
        
        // Find the end of the JSON message
        let mut brace_count = 0;
        let mut end_pos = None;
        let mut in_string = false;
        let mut escaped = false;
        
        for (i, &byte) in src[start_pos..].iter().enumerate() {
            let actual_pos = start_pos + i;
            
            match byte {
                b'"' if !escaped => {
                    in_string = !in_string;
                }
                b'{' if !in_string => {
                    brace_count += 1;
                }
                b'}' if !in_string => {
                    brace_count -= 1;
                    if brace_count == 0 {
                        end_pos = Some(actual_pos + 1);
                        break;
                    }
                }
                _ => {}
            }
            
            escaped = byte == b'\\' && !escaped;
        }

        if let Some(end_pos) = end_pos {
            // Extract the JSON message
            let json_bytes = src[start_pos..end_pos].to_vec();
            let json_str = String::from_utf8_lossy(&json_bytes);
            
            // Transform the message if needed
            match Self::transform_message(&json_str) {
                Ok(transformed) => {
                    // Check if message was filtered out (empty string)
                    if transformed.is_empty() {
                        debug!("Message filtered out, removing from buffer");
                        // Remove the message from buffer by copying everything except the JSON message
                        let mut new_buf = BytesMut::with_capacity(src.len());
                        new_buf.extend_from_slice(&src[..start_pos]);
                        new_buf.extend_from_slice(&src[end_pos..]);
                        *src = new_buf;
                        // Return Ok(None) to indicate no message to process
                        return Ok(None);
                    } else {
                        
                        // Replace the original bytes with transformed bytes in the buffer
                        let transformed_bytes = transformed.as_bytes();
                        let mut new_buf = BytesMut::with_capacity(src.len());
                        
                        // Copy everything before the JSON message
                        new_buf.extend_from_slice(&src[..start_pos]);
                        // Add the transformed message
                        new_buf.extend_from_slice(transformed_bytes);
                        // Copy everything after the JSON message
                        new_buf.extend_from_slice(&src[end_pos..]);
                        
                        // Replace the buffer
                        *src = new_buf;
                    }
                }
                Err(e) => {
                    debug!("Failed to transform message: {}, using original", e);
                }
            }
        }
        
        // Now let the inner codec handle the (possibly transformed) message
        self.inner.decode(src)
    }
}

impl Encoder<ClientJsonRpcMessage> for McpCompatCodec {
    type Error = rmcp::transport::async_rw::JsonRpcMessageCodecError;

    fn encode(&mut self, item: ClientJsonRpcMessage, dst: &mut BytesMut) -> Result<(), Self::Error> {
        self.inner.encode(item, dst)
    }
}