use crate::models::{JSONRPCMessage, SSEEvent};
use anyhow::Result;
use bytes::Bytes;
use futures::Stream;
use reqwest::{Body, header};
use serde_json::Value;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::time::timeout;
use tracing::*;

/// A stream that parses SSE events from a bytes stream
pub struct SSEStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: String,
    event_type: Option<String>,
    data: String,
}

impl SSEStream {
    pub fn new(stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(stream),
            buffer: String::new(),
            event_type: None,
            data: String::new(),
        }
    }

    fn process_line(&mut self, line: &str) -> Option<SSEEvent> {
        if line.is_empty() {
            // Empty line means end of event
            if self.data.is_empty() {
                return None;
            }

            let event_type = self
                .event_type
                .take()
                .unwrap_or_else(|| "message".to_string());
            let data = std::mem::take(&mut self.data);

            match event_type.as_str() {
                "message" => match serde_json::from_str::<JSONRPCMessage>(&data) {
                    Ok(message) => Some(SSEEvent::Message(message)),
                    Err(_) => Some(SSEEvent::Other(event_type, data)),
                },
                "endpoint" => Some(SSEEvent::Endpoint(data)),
                _ => Some(SSEEvent::Other(event_type, data)),
            }
        } else if line.starts_with("event:") {
            self.event_type = Some(line[6..].trim().to_string());
            None
        } else if line.starts_with("data:") {
            self.data = line[5..].trim().to_string();
            None
        } else {
            None
        }
    }
}

impl Stream for SSEStream {
    type Item = Result<SSEEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(pos) = self.buffer.find('\n') {
                let line = self.buffer[..pos].trim().to_string();
                self.buffer = self.buffer[pos + 1..].to_string();

                if let Some(event) = self.process_line(&line) {
                    return Poll::Ready(Some(Ok(event)));
                }
                continue;
            }

            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(bytes))) => match String::from_utf8(bytes.to_vec()) {
                    Ok(s) => self.buffer.push_str(&s),
                    Err(e) => {
                        return Poll::Ready(Some(Err(anyhow::anyhow!("Invalid UTF-8: {}", e))));
                    }
                },
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Some(Err(anyhow::anyhow!("Stream error: {}", e))));
                }
                Poll::Ready(None) => {
                    // Process any remaining data if there's content left in the buffer
                    if !self.buffer.is_empty() {
                        let line = std::mem::take(&mut self.buffer);
                        if let Some(event) = self.process_line(&line) {
                            return Poll::Ready(Some(Ok(event)));
                        }
                    }
                    return Poll::Ready(None);
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Creates an SSE client and returns a stream of events
pub async fn create_sse_stream(
    url: &str,
) -> Result<Pin<Box<dyn Stream<Item = Result<SSEEvent>> + Send>>> {
    let client = reqwest::Client::new();

    let response = client
        .get(url)
        .header(header::ACCEPT, "text/event-stream")
        .send()
        .await?;

    if !response.status().is_success() {
        anyhow::bail!("Server returned non-success status: {}", response.status());
    }

    let bytes_stream = response.bytes_stream();
    let sse_stream = SSEStream::new(bytes_stream);

    Ok(Box::pin(sse_stream))
}

/// Send an HTTP POST request to the given endpoint with the given JSON-RPC message
pub async fn forward_message(
    message: &Value,
    endpoint: &str,
    receive_timeout_ms: u64,
) -> Result<()> {
    debug!("Forwarding request to server: {:?}", message);

    let client = reqwest::Client::new();

    // Convert message to a string first
    let message_body = serde_json::to_string(message)?;
    let body = Body::from(message_body);

    let future = client
        .post(endpoint)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send();

    match timeout(Duration::from_millis(receive_timeout_ms), future).await {
        Ok(response) => match response {
            Ok(response) => {
                if !response.status().is_success() {
                    info!(
                        "Forwarding request failed with status code: {}",
                        response.status()
                    );
                    // even when the server replies with a status code that is not in the 200 range
                    // it might still send a reply on the SSE connection
                    // so we do not consider this a real failure
                }
            }
            Err(e) => {
                info!("Forwarding request failed with error: {:?}", e);
            }
        },
        Err(_) => anyhow::bail!("Request timed out after {} ms", receive_timeout_ms),
    };

    Ok(())
}
