use std::pin::Pin;

use eventsource_stream::Eventsource;
use futures::{Stream, StreamExt};

/// A parsed SSE event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// Optional SSE event type.
    pub event_type: String,
    /// Raw SSE data payload.
    pub data: String,
}

/// SSE parsing failure.
#[derive(Debug, thiserror::Error)]
pub enum SseError {
    /// The underlying event stream failed.
    #[error("Stream error: {0}")]
    Stream(String),
}

/// Convert an HTTP response body into a stream of SSE events.
pub fn sse_stream(
    response: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = Result<SseEvent, SseError>> + Send>> {
    let stream = response.bytes_stream().eventsource().map(|result| {
        result
            .map(|event| SseEvent {
                event_type: event.event,
                data: event.data,
            })
            .map_err(|error| SseError::Stream(error.to_string()))
    });
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn parses_basic_sse_events() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let address = listener.local_addr().expect("local addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request_buffer = [0u8; 1024];
            let _ = socket.read(&mut request_buffer).await;
            let body = "event: message\ndata: {\"hello\":true}\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
        });

        let response = reqwest::Client::new()
            .get(format!("http://{address}"))
            .send()
            .await
            .expect("request response");
        let mut stream = sse_stream(response);
        let event = stream
            .next()
            .await
            .expect("sse event")
            .expect("parsed event");

        assert_eq!(event.event_type, "message");
        assert_eq!(event.data, r#"{"hello":true}"#);

        server.await.expect("server task");
    }
}
