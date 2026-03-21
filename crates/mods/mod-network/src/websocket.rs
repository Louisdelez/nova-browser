//! WebSocket support for NOVA.
//!
//! Provides a WebSocket connection abstraction that wraps `tokio-tungstenite`
//! for establishing `ws://` and `wss://` connections, sending and receiving
//! text or binary messages, and performing graceful close handshakes.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use nova_mod_api::NovaError;

/// A unique handle identifying a WebSocket connection.
pub type WsHandle = u64;

/// A WebSocket message (text or binary).
#[derive(Debug, Clone)]
pub enum WsMessage {
    /// UTF-8 text message.
    Text(String),
    /// Binary message.
    Binary(Vec<u8>),
    /// Ping frame.
    Ping(Vec<u8>),
    /// Pong frame.
    Pong(Vec<u8>),
    /// Close frame with optional code and reason.
    Close(Option<u16>, Option<String>),
}

/// The current state of a WebSocket connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsReadyState {
    /// The connection is being established.
    Connecting = 0,
    /// The connection is open and ready for communication.
    Open = 1,
    /// The connection is going through the closing handshake.
    Closing = 2,
    /// The connection has been closed or could not be opened.
    Closed = 3,
}

/// A single WebSocket connection.
struct WebSocketConnection {
    /// The write half of the WebSocket stream.
    writer: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    /// The URL this connection was opened to.
    url: String,
    /// The negotiated subprotocol, if any.
    protocol: Option<String>,
    /// Current ready state.
    ready_state: WsReadyState,
}

/// Manager for multiple WebSocket connections.
///
/// Each connection is identified by a unique [`WsHandle`]. The manager
/// provides methods to connect, send, receive, and close connections.
pub struct WebSocketManager {
    /// Active connections keyed by handle.
    connections: Arc<Mutex<HashMap<WsHandle, WebSocketConnection>>>,
    /// Counter for generating unique handles.
    next_handle: AtomicU64,
}

impl WebSocketManager {
    /// Create a new WebSocket manager.
    pub fn new() -> Self {
        info!("WebSocket manager created");
        Self {
            connections: Arc::new(Mutex::new(HashMap::new())),
            next_handle: AtomicU64::new(1),
        }
    }

    /// Establish a new WebSocket connection to the given URL.
    ///
    /// Supports `ws://` and `wss://` URLs. Returns a handle that can be
    /// used for subsequent send/recv/close operations.
    pub async fn connect(&self, url: &str) -> Result<WsHandle, NovaError> {
        debug!(url = %url, "WebSocket: connecting");

        let (ws_stream, response) =
            tokio_tungstenite::connect_async(url)
                .await
                .map_err(|e| NovaError::NetworkError(format!("WebSocket connect failed: {e}")))?;

        let protocol = response
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let handle = self.next_handle.fetch_add(1, Ordering::Relaxed);

        let conn = WebSocketConnection {
            writer: ws_stream,
            url: url.to_string(),
            protocol,
            ready_state: WsReadyState::Open,
        };

        self.connections.lock().await.insert(handle, conn);

        info!(handle, url = %url, "WebSocket: connected");
        Ok(handle)
    }

    /// Send a message on a WebSocket connection.
    pub async fn send(&self, handle: WsHandle, message: WsMessage) -> Result<(), NovaError> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let mut conns = self.connections.lock().await;
        let conn = conns
            .get_mut(&handle)
            .ok_or_else(|| NovaError::NetworkError(format!("WebSocket handle {handle} not found")))?;

        if conn.ready_state != WsReadyState::Open {
            return Err(NovaError::NetworkError(
                "WebSocket is not in OPEN state".into(),
            ));
        }

        let tung_msg = match message {
            WsMessage::Text(text) => Message::Text(text.into()),
            WsMessage::Binary(data) => Message::Binary(data.into()),
            WsMessage::Ping(data) => Message::Ping(data.into()),
            WsMessage::Pong(data) => Message::Pong(data.into()),
            WsMessage::Close(code, reason) => {
                let frame = code.map(|c| {
                    tokio_tungstenite::tungstenite::protocol::CloseFrame {
                        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(c),
                        reason: reason.unwrap_or_default().into(),
                    }
                });
                Message::Close(frame)
            }
        };

        conn.writer
            .send(tung_msg)
            .await
            .map_err(|e| NovaError::NetworkError(format!("WebSocket send failed: {e}")))?;

        debug!(handle, "WebSocket: message sent");
        Ok(())
    }

    /// Receive a message from a WebSocket connection.
    ///
    /// Blocks until a message is available or the connection is closed.
    pub async fn recv(&self, handle: WsHandle) -> Result<WsMessage, NovaError> {
        use futures_util::StreamExt;
        use tokio_tungstenite::tungstenite::Message;

        let mut conns = self.connections.lock().await;
        let conn = conns
            .get_mut(&handle)
            .ok_or_else(|| NovaError::NetworkError(format!("WebSocket handle {handle} not found")))?;

        match conn.writer.next().await {
            Some(Ok(msg)) => {
                let ws_msg = match msg {
                    Message::Text(text) => WsMessage::Text(text.to_string()),
                    Message::Binary(data) => WsMessage::Binary(data.to_vec()),
                    Message::Ping(data) => WsMessage::Ping(data.to_vec()),
                    Message::Pong(data) => WsMessage::Pong(data.to_vec()),
                    Message::Close(frame) => {
                        conn.ready_state = WsReadyState::Closed;
                        let (code, reason) = frame
                            .map(|f| (Some(f.code.into()), Some(f.reason.to_string())))
                            .unwrap_or((None, None));
                        WsMessage::Close(code, reason)
                    }
                    Message::Frame(_) => {
                        return Err(NovaError::NetworkError("unexpected raw frame".into()));
                    }
                };
                debug!(handle, "WebSocket: message received");
                Ok(ws_msg)
            }
            Some(Err(e)) => {
                conn.ready_state = WsReadyState::Closed;
                Err(NovaError::NetworkError(format!(
                    "WebSocket recv error: {e}"
                )))
            }
            None => {
                conn.ready_state = WsReadyState::Closed;
                Ok(WsMessage::Close(None, None))
            }
        }
    }

    /// Close a WebSocket connection gracefully.
    pub async fn close(
        &self,
        handle: WsHandle,
        code: Option<u16>,
        reason: Option<String>,
    ) -> Result<(), NovaError> {
        use futures_util::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let mut conns = self.connections.lock().await;
        let conn = conns
            .get_mut(&handle)
            .ok_or_else(|| NovaError::NetworkError(format!("WebSocket handle {handle} not found")))?;

        if conn.ready_state == WsReadyState::Closed {
            return Ok(());
        }

        conn.ready_state = WsReadyState::Closing;

        let frame = code.map(|c| {
            tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::from(c),
                reason: reason.unwrap_or_default().into(),
            }
        });

        let _ = conn.writer.send(Message::Close(frame)).await;
        conn.ready_state = WsReadyState::Closed;

        info!(handle, "WebSocket: closed");
        Ok(())
    }

    /// Get the ready state of a connection.
    pub async fn ready_state(&self, handle: WsHandle) -> Result<WsReadyState, NovaError> {
        let conns = self.connections.lock().await;
        let conn = conns
            .get(&handle)
            .ok_or_else(|| NovaError::NetworkError(format!("WebSocket handle {handle} not found")))?;
        Ok(conn.ready_state)
    }

    /// Get the URL of a connection.
    pub async fn url(&self, handle: WsHandle) -> Result<String, NovaError> {
        let conns = self.connections.lock().await;
        let conn = conns
            .get(&handle)
            .ok_or_else(|| NovaError::NetworkError(format!("WebSocket handle {handle} not found")))?;
        Ok(conn.url.clone())
    }

    /// Get the negotiated protocol of a connection.
    pub async fn protocol(&self, handle: WsHandle) -> Result<Option<String>, NovaError> {
        let conns = self.connections.lock().await;
        let conn = conns
            .get(&handle)
            .ok_or_else(|| NovaError::NetworkError(format!("WebSocket handle {handle} not found")))?;
        Ok(conn.protocol.clone())
    }

    /// Remove a closed connection from the manager.
    pub async fn remove(&self, handle: WsHandle) {
        self.connections.lock().await.remove(&handle);
        debug!(handle, "WebSocket: connection removed");
    }

    /// Get the number of active connections.
    pub async fn connection_count(&self) -> usize {
        self.connections.lock().await.len()
    }
}

impl Default for WebSocketManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_ready_state_values() {
        assert_eq!(WsReadyState::Connecting as u8, 0);
        assert_eq!(WsReadyState::Open as u8, 1);
        assert_eq!(WsReadyState::Closing as u8, 2);
        assert_eq!(WsReadyState::Closed as u8, 3);
    }

    #[test]
    fn ws_message_variants() {
        let text = WsMessage::Text("hello".into());
        assert!(matches!(text, WsMessage::Text(ref s) if s == "hello"));

        let bin = WsMessage::Binary(vec![1, 2, 3]);
        assert!(matches!(bin, WsMessage::Binary(ref d) if d.len() == 3));

        let close = WsMessage::Close(Some(1000), Some("normal".into()));
        assert!(matches!(close, WsMessage::Close(Some(1000), _)));
    }

    #[tokio::test]
    async fn ws_manager_creation() {
        let mgr = WebSocketManager::new();
        assert_eq!(mgr.connection_count().await, 0);
    }

    #[tokio::test]
    async fn ws_manager_invalid_handle() {
        let mgr = WebSocketManager::new();
        let result = mgr.ready_state(999).await;
        assert!(result.is_err());
    }
}
