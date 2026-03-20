//! HTTP/2 support and connection pooling.
//!
//! Provides ALPN-aware connection management and a connection pool that
//! reuses TCP/TLS connections to the same origin. Actual h2 framing is
//! a future enhancement — for now this module focuses on:
//!
//! - Connection pooling for HTTP/1.1 (reuse TCP connections)
//! - ALPN configuration so h2-capable servers know we support it
//! - Tracking the negotiated protocol after TLS handshake

use std::collections::HashMap;
use std::sync::Mutex;

use tracing::{debug, info, warn};

use nova_mod_api::NovaError;

use crate::transport::{self, Transport};

/// The negotiated application protocol after TLS handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NegotiatedProtocol {
    /// HTTP/2 was negotiated via ALPN.
    H2,
    /// HTTP/1.1 was negotiated or no ALPN was available.
    Http11,
    /// No TLS (plain HTTP), protocol is implicitly HTTP/1.1.
    None,
}

impl std::fmt::Display for NegotiatedProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NegotiatedProtocol::H2 => write!(f, "h2"),
            NegotiatedProtocol::Http11 => write!(f, "http/1.1"),
            NegotiatedProtocol::None => write!(f, "none"),
        }
    }
}

/// A cached connection entry in the pool.
struct PoolEntry {
    /// The transport connection (may be consumed on use).
    transport: Option<Transport>,
    /// The protocol negotiated for this connection.
    protocol: NegotiatedProtocol,
}

/// A connection pool that caches TCP/TLS connections by origin.
///
/// Connections are keyed by `host:port` and reused when available.
/// Each origin can have at most one cached connection. When a connection
/// is taken from the pool, the entry is removed.
pub struct ConnectionPool {
    /// Pool entries keyed by `host:port`.
    pool: Mutex<HashMap<String, PoolEntry>>,
    /// Maximum number of cached connections.
    max_connections: usize,
}

impl ConnectionPool {
    /// Create a new connection pool.
    ///
    /// `max_connections` limits the total number of cached connections
    /// across all origins.
    pub fn new(max_connections: usize) -> Self {
        info!(max_connections, "connection pool created");
        Self {
            pool: Mutex::new(HashMap::new()),
            max_connections,
        }
    }

    /// Get a cached connection for the given origin, if available.
    ///
    /// Removes the connection from the pool (it cannot be reused after
    /// this call unless it is returned via `put`).
    pub fn take(&self, host: &str, port: u16) -> Option<(Transport, NegotiatedProtocol)> {
        let key = format!("{host}:{port}");
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = pool.remove(&key) {
            if let Some(transport) = entry.transport {
                debug!(origin = %key, protocol = %entry.protocol, "reusing pooled connection");
                return Some((transport, entry.protocol));
            }
        }
        None
    }

    /// Return a connection to the pool for future reuse.
    ///
    /// If the pool is full, the connection is dropped.
    pub fn put(
        &self,
        host: &str,
        port: u16,
        transport: Transport,
        protocol: NegotiatedProtocol,
    ) {
        let key = format!("{host}:{port}");
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());

        if pool.len() >= self.max_connections {
            debug!(
                origin = %key,
                pool_size = pool.len(),
                "connection pool full, dropping connection"
            );
            return;
        }

        debug!(origin = %key, protocol = %protocol, "connection returned to pool");
        pool.insert(
            key,
            PoolEntry {
                transport: Some(transport),
                protocol,
            },
        );
    }

    /// Get a connection, either from the pool or by creating a new one.
    ///
    /// If a cached connection exists for the origin, it is returned.
    /// Otherwise, a new connection is established using `transport::connect`.
    pub async fn get_or_connect(
        &self,
        host: &str,
        port: u16,
        use_tls: bool,
    ) -> Result<(Transport, NegotiatedProtocol), NovaError> {
        // Try the pool first.
        if let Some((transport, protocol)) = self.take(host, port) {
            return Ok((transport, protocol));
        }

        // Create a new connection.
        let transport = transport::connect(host, port, use_tls).await?;
        let protocol = transport.negotiated_protocol();

        info!(
            host = %host,
            port,
            protocol = %protocol,
            "new connection established"
        );

        Ok((transport, protocol))
    }

    /// Return the number of currently pooled connections.
    pub fn len(&self) -> usize {
        let pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        pool.len()
    }

    /// Check if the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Clear all connections from the pool.
    pub fn clear(&self) {
        let mut pool = self.pool.lock().unwrap_or_else(|e| e.into_inner());
        let count = pool.len();
        pool.clear();
        if count > 0 {
            info!(dropped = count, "connection pool cleared");
        }
    }
}

impl Default for ConnectionPool {
    fn default() -> Self {
        Self::new(6) // Default: 6 connections per pool (similar to browser limits)
    }
}

/// Detect the negotiated ALPN protocol from a TLS connection.
///
/// Returns `NegotiatedProtocol::H2` if `h2` was negotiated,
/// `Http11` if `http/1.1` was negotiated or no ALPN was available.
pub fn detect_protocol(alpn: Option<&[u8]>) -> NegotiatedProtocol {
    match alpn {
        Some(b"h2") => {
            debug!("ALPN negotiated: h2");
            NegotiatedProtocol::H2
        }
        Some(b"http/1.1") => {
            debug!("ALPN negotiated: http/1.1");
            NegotiatedProtocol::Http11
        }
        Some(other) => {
            let proto = String::from_utf8_lossy(other);
            warn!(protocol = %proto, "unknown ALPN protocol, falling back to HTTP/1.1");
            NegotiatedProtocol::Http11
        }
        None => {
            debug!("no ALPN negotiated, using HTTP/1.1");
            NegotiatedProtocol::Http11
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_pool_new() {
        let pool = ConnectionPool::new(10);
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
    }

    #[test]
    fn connection_pool_take_empty() {
        let pool = ConnectionPool::new(10);
        assert!(pool.take("example.com", 443).is_none());
    }

    #[test]
    fn connection_pool_clear() {
        let pool = ConnectionPool::new(10);
        pool.clear();
        assert!(pool.is_empty());
    }

    #[test]
    fn detect_protocol_h2() {
        assert_eq!(detect_protocol(Some(b"h2")), NegotiatedProtocol::H2);
    }

    #[test]
    fn detect_protocol_http11() {
        assert_eq!(
            detect_protocol(Some(b"http/1.1")),
            NegotiatedProtocol::Http11
        );
    }

    #[test]
    fn detect_protocol_none() {
        assert_eq!(detect_protocol(None), NegotiatedProtocol::Http11);
    }

    #[test]
    fn detect_protocol_unknown() {
        assert_eq!(
            detect_protocol(Some(b"spdy/3.1")),
            NegotiatedProtocol::Http11
        );
    }

    #[test]
    fn negotiated_protocol_display() {
        assert_eq!(NegotiatedProtocol::H2.to_string(), "h2");
        assert_eq!(NegotiatedProtocol::Http11.to_string(), "http/1.1");
        assert_eq!(NegotiatedProtocol::None.to_string(), "none");
    }

    #[test]
    fn connection_pool_default() {
        let pool = ConnectionPool::default();
        assert_eq!(pool.max_connections, 6);
        assert!(pool.is_empty());
    }
}
