//! TCP + TLS transport layer.
//!
//! Handles raw TCP connections and optional TLS wrapping via rustls.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use rustls::ClientConfig;
use rustls::pki_types::ServerName;

use nova_mod_api::NovaError;

/// An abstraction over a plain TCP or TLS stream.
pub enum Transport {
    Plain(TcpStream),
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
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

        let config = ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();

        let connector = TlsConnector::from(Arc::new(config));

        let server_name = ServerName::try_from(host.to_string())
            .map_err(|e| NovaError::TlsError(format!("invalid server name '{host}': {e}")))?;

        let tls_stream = connector
            .connect(server_name, tcp)
            .await
            .map_err(|e| {
                let detail = format!("{e}");
                let hint = if detail.contains("CertificateRequired") || detail.contains("certificate") {
                    " The site's SSL certificate could not be verified. It may be expired, self-signed, or issued by an untrusted authority."
                } else if detail.contains("HandshakeFailure") {
                    " The server rejected the TLS handshake. It may not support modern TLS versions."
                } else {
                    ""
                };
                NovaError::TlsError(format!(
                    "Certificate error for {host}: {detail}.{hint}"
                ))
            })?;

        Ok(Transport::Tls(tls_stream))
    } else {
        Ok(Transport::Plain(tcp))
    }
}

/// Send an HTTP request and read the full response.
pub async fn send_request(transport: Transport, request: &str) -> Result<Vec<u8>, NovaError> {
    match transport {
        Transport::Plain(mut stream) => {
            stream
                .write_all(request.as_bytes())
                .await
                .map_err(|e| NovaError::NetworkError(format!("write failed: {e}")))?;

            read_response(&mut stream).await
        }
        Transport::Tls(mut stream) => {
            stream
                .write_all(request.as_bytes())
                .await
                .map_err(|e| NovaError::NetworkError(format!("TLS write failed: {e}")))?;

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
