//! TCP + TLS transport layer.
//!
//! Handles raw TCP connections and optional TLS wrapping via rustls.
//! Supports ALPN negotiation for HTTP/2 (`h2`) and HTTP/1.1.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;

use nova_mod_api::NovaError;

use crate::http2::NegotiatedProtocol;

/// An abstraction over a plain TCP or TLS stream.
pub enum Transport {
    Plain(TcpStream),
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
}

impl Transport {
    /// Get the ALPN-negotiated protocol for this transport.
    ///
    /// Returns `NegotiatedProtocol::None` for plain TCP connections.
    /// For TLS connections, checks the negotiated ALPN protocol.
    pub fn negotiated_protocol(&self) -> NegotiatedProtocol {
        match self {
            Transport::Plain(_) => NegotiatedProtocol::None,
            Transport::Tls(tls) => {
                let (_, conn) = tls.get_ref();
                let alpn = conn.alpn_protocol();
                crate::http2::detect_protocol(alpn)
            }
        }
    }
}

/// Connect to a host:port, optionally wrapping in TLS.
pub async fn connect(host: &str, port: u16, use_tls: bool) -> Result<Transport, NovaError> {
    let addr = format!("{host}:{port}");
    let tcp = TcpStream::connect(&addr)
        .await
        .map_err(|e| NovaError::NetworkError(format!("TCP connect to {addr} failed: {e}")))?;

    if use_tls {
        let mut root_store = rustls::RootCertStore::empty();

        // Try native OS certificate store first (handles corporate/custom CAs).
        let native_certs = rustls_native_certs::load_native_certs();
        let native_count = native_certs.certs.len();
        let native_errors = native_certs.errors.len();

        if native_count > 0 {
            let (added, failed) = root_store.add_parsable_certificates(native_certs.certs);
            tracing::debug!(
                added,
                failed,
                native_errors,
                "loaded native OS certificates"
            );
        }

        // Fall back to bundled webpki-roots if native certs are unavailable.
        if root_store.is_empty() {
            tracing::warn!("no native certs found, falling back to webpki-roots");
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }

        let mut config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        // Configure ALPN protocols: prefer http/1.1 (h2 framing not yet implemented).
        // Once full HTTP/2 frame support is added, h2 can be advertised first.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];

        let connector = TlsConnector::from(Arc::new(config));

        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| NovaError::TlsError(format!("invalid server name '{host}': {e}")))?;

        let tls_stream = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| NovaError::TlsError(format!("TLS handshake with {host} failed: {e}")))?;

        Ok(Transport::Tls(tls_stream))
    } else {
        Ok(Transport::Plain(tcp))
    }
}

/// Send an HTTP request and read the full response.
pub async fn send_request(transport: Transport, request: &str) -> Result<Vec<u8>, NovaError> {
    send_request_with_body(transport, request.as_bytes(), None).await
}

/// Send an HTTP request with optional body and read the full response.
///
/// `headers_bytes` contains the HTTP request line and headers (including
/// the final `\r\n\r\n`). If `body` is provided, it is sent immediately
/// after the headers.
pub async fn send_request_with_body(
    transport: Transport,
    headers_bytes: &[u8],
    body: Option<&[u8]>,
) -> Result<Vec<u8>, NovaError> {
    match transport {
        Transport::Plain(mut stream) => {
            stream
                .write_all(headers_bytes)
                .await
                .map_err(|e| NovaError::NetworkError(format!("write failed: {e}")))?;
            if let Some(body) = body {
                stream
                    .write_all(body)
                    .await
                    .map_err(|e| NovaError::NetworkError(format!("body write failed: {e}")))?;
            }
            read_response(&mut stream).await
        }
        Transport::Tls(mut stream) => {
            stream
                .write_all(headers_bytes)
                .await
                .map_err(|e| NovaError::NetworkError(format!("TLS write failed: {e}")))?;
            if let Some(body) = body {
                stream
                    .write_all(body)
                    .await
                    .map_err(|e| NovaError::NetworkError(format!("TLS body write failed: {e}")))?;
            }
            read_response(&mut stream).await
        }
    }
}

/// Read the full response into a buffer.
async fn read_response(stream: &mut (impl AsyncReadExt + Unpin)) -> Result<Vec<u8>, NovaError> {
    let mut buf = Vec::with_capacity(64 * 1024);
    let mut chunk = [0u8; 8192];

    loop {
        match stream.read(&mut chunk).await {
            Ok(0) => break, // EOF — connection closed.
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Safety limit: 16 MB max response.
                if buf.len() > 16 * 1024 * 1024 {
                    return Err(NovaError::NetworkError(
                        "response too large (>16MB)".into(),
                    ));
                }
            }
            Err(e) => {
                // If we already have data, return what we have.
                if !buf.is_empty() {
                    break;
                }
                return Err(NovaError::NetworkError(format!("read failed: {e}")));
            }
        }
    }

    Ok(buf)
}
