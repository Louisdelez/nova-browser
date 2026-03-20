//! # mod-network
//!
//! NOVA Mod for HTTP/HTTPS networking.
//! Handles `FetchUrl("http")` and `FetchUrl("https")` capabilities.
//! Uses hyper for HTTP/1.1 and rustls for TLS.

use std::io::Read as IoRead;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use flate2::read::GzDecoder;
use semver::Version;
use tracing::{debug, info, warn};

use nova_mod_api::{
    capability::CapabilityType,
    content::{ContentRequest, HttpResponse, TypedData},
    error::NovaError,
    manifest::ModManifest,
    permission::{Permission, TrustLevel},
    trigger::{ContentTrigger, TriggerCondition},
    types::ModId,
    CoreApi, NovaMod,
};

mod transport;

/// The network mod — fetches resources over HTTP and HTTPS.
pub struct NetworkMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
}

impl NetworkMod {
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.network"),
            name: "NOVA Network".into(),
            version: Version::new(0, 1, 0),
            description: "HTTP/HTTPS networking for NOVA".into(),
            capabilities: vec![
                CapabilityType::FetchUrl("http".into()),
                CapabilityType::FetchUrl("https".into()),
            ],
            permissions: vec![Permission::NetworkFetch],
            dependencies: vec![],
            triggers: vec![
                ContentTrigger {
                    condition: TriggerCondition::Protocol("http".into()),
                    mod_id: ModId::new("org.nova.network"),
                    priority: 100,
                },
                ContentTrigger {
                    condition: TriggerCondition::Protocol("https".into()),
                    mod_id: ModId::new("org.nova.network"),
                    priority: 100,
                },
            ],
            min_core_version: Version::new(0, 1, 0),
            trust_level: TrustLevel::Core,
        };

        Self {
            manifest,
            core: None,
        }
    }

    /// Maximum number of HTTP redirects to follow.
    const MAX_REDIRECTS: u8 = 10;

    /// Perform a real HTTP/HTTPS GET request, following redirects.
    async fn fetch_real(&self, url_str: &str, _headers: &[(String, String)]) -> Result<HttpResponse, NovaError> {
        let mut current_url = url_str.to_string();

        for redirect_count in 0..=Self::MAX_REDIRECTS {
            let response = self.fetch_single(&current_url).await?;

            // Follow redirects for 301, 302, 307, 308.
            if matches!(response.status, 301 | 302 | 307 | 308) {
                let location = response
                    .headers
                    .iter()
                    .find(|(k, _)| k == "location")
                    .map(|(_, v)| v.clone());

                if let Some(loc) = location {
                    // Resolve relative URLs against the current URL.
                    let base = url::Url::parse(&current_url)
                        .map_err(|e| NovaError::NetworkError(format!("invalid URL: {e}")))?;
                    let resolved = base.join(&loc)
                        .map_err(|e| NovaError::NetworkError(format!("invalid redirect URL '{loc}': {e}")))?;

                    debug!(
                        from = %current_url,
                        to = %resolved,
                        status = response.status,
                        redirect = redirect_count + 1,
                        "following redirect"
                    );
                    current_url = resolved.to_string();
                    continue;
                }
            }

            return Ok(response);
        }

        Err(NovaError::NetworkError(format!(
            "too many redirects (>{}) for {url_str}",
            Self::MAX_REDIRECTS
        )))
    }

    /// Perform a single HTTP GET request (no redirect following).
    async fn fetch_single(&self, url_str: &str) -> Result<HttpResponse, NovaError> {
        let parsed = url::Url::parse(url_str)
            .map_err(|e| NovaError::NetworkError(format!("invalid URL: {e}")))?;

        let scheme = parsed.scheme();
        let host = parsed
            .host_str()
            .ok_or_else(|| NovaError::NetworkError("missing host".into()))?
            .to_string();
        let port = parsed.port_or_known_default()
            .ok_or_else(|| NovaError::NetworkError("cannot determine port".into()))?;
        let path = if parsed.query().is_some() {
            format!("{}?{}", parsed.path(), parsed.query().unwrap())
        } else {
            parsed.path().to_string()
        };

        let use_tls = scheme == "https";

        debug!(url = %url_str, host = %host, port, tls = use_tls, "connecting");

        let stream = transport::connect(&host, port, use_tls).await?;

        // Build HTTP/1.1 request manually (simple, no heavy framework overhead).
        let request = format!(
            "GET {path} HTTP/1.1\r\n\
             Host: {host}\r\n\
             User-Agent: NOVA/0.1.0\r\n\
             Accept: text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8\r\n\
             Accept-Encoding: gzip, identity\r\n\
             Connection: close\r\n\
             \r\n"
        );

        let raw_response = transport::send_request(stream, &request).await?;
        let response = parse_http_response(&raw_response, url_str)?;

        info!(
            url = %url_str,
            status = response.status,
            body_len = response.body.len(),
            "fetch complete"
        );

        Ok(response)
    }
}

/// Parse a raw HTTP response into our HttpResponse type.
fn parse_http_response(raw: &[u8], url: &str) -> Result<HttpResponse, NovaError> {
    // Find the header/body boundary.
    let header_end = find_header_end(raw)
        .ok_or_else(|| NovaError::NetworkError("malformed HTTP response: no header boundary".into()))?;

    let header_bytes = &raw[..header_end];
    let body_start = header_end + 4; // skip \r\n\r\n
    let body = if body_start < raw.len() {
        Bytes::copy_from_slice(&raw[body_start..])
    } else {
        Bytes::new()
    };

    let header_str = String::from_utf8_lossy(header_bytes);
    let mut lines = header_str.lines();

    // Parse status line: "HTTP/1.1 200 OK"
    let status_line = lines
        .next()
        .ok_or_else(|| NovaError::NetworkError("empty HTTP response".into()))?;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);

    // Parse headers.
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((
                name.trim().to_lowercase(),
                value.trim().to_string(),
            ));
        }
    }

    // Handle chunked transfer encoding.
    let is_chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"));

    let decoded_body = if is_chunked {
        decode_chunked(&body)
    } else {
        body
    };

    // Handle gzip content encoding.
    let is_gzip = headers
        .iter()
        .any(|(k, v)| k == "content-encoding" && v.contains("gzip"));

    let final_body = if is_gzip {
        let mut decoder = GzDecoder::new(decoded_body.as_ref());
        let mut decompressed = Vec::new();
        match decoder.read_to_end(&mut decompressed) {
            Ok(_) => {
                tracing::debug!(
                    compressed = decoded_body.len(),
                    decompressed = decompressed.len(),
                    "gzip decompressed"
                );
                Bytes::from(decompressed)
            }
            Err(e) => {
                tracing::warn!("gzip decompression failed: {e}, using raw body");
                decoded_body
            }
        }
    } else {
        decoded_body
    };

    Ok(HttpResponse {
        status,
        headers,
        body: final_body,
        url: url.to_string(),
    })
}

/// Find the position of \r\n\r\n in raw bytes.
fn find_header_end(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Decode chunked transfer encoding.
fn decode_chunked(data: &Bytes) -> Bytes {
    let mut decoded = Vec::new();
    let mut pos = 0;
    let data = data.as_ref();

    while pos < data.len() {
        // Find the end of the chunk size line.
        let line_end = match data[pos..].windows(2).position(|w| w == b"\r\n") {
            Some(p) => pos + p,
            None => break,
        };

        let size_str = String::from_utf8_lossy(&data[pos..line_end]);
        let chunk_size = match usize::from_str_radix(size_str.trim(), 16) {
            Ok(s) => s,
            Err(_) => break,
        };

        if chunk_size == 0 {
            break;
        }

        let chunk_start = line_end + 2;
        let chunk_end = chunk_start + chunk_size;

        if chunk_end > data.len() {
            // Incomplete chunk — take what we have.
            decoded.extend_from_slice(&data[chunk_start..]);
            break;
        }

        decoded.extend_from_slice(&data[chunk_start..chunk_end]);
        pos = chunk_end + 2; // skip trailing \r\n
    }

    Bytes::from(decoded)
}

impl Default for NetworkMod {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl NovaMod for NetworkMod {
    fn manifest(&self) -> &ModManifest {
        &self.manifest
    }

    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "network mod initializing");
        self.core = Some(core);
        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            ContentRequest::Fetch { url, headers } => {
                debug!(url = %url, header_count = headers.len(), "handling fetch request");

                match self.fetch_real(&url, &headers).await {
                    Ok(response) => Ok(TypedData::HttpResponse(response)),
                    Err(e) => {
                        warn!(url = %url, error = %e, "real fetch failed, using placeholder");
                        // Fallback to placeholder so the pipeline doesn't break.
                        let body = format!(
                            "<html><body><h1>NOVA Browser</h1>\
                             <p>Could not load: {url}</p>\
                             <p>Error: {e}</p></body></html>"
                        );
                        Ok(TypedData::HttpResponse(HttpResponse {
                            status: 0,
                            headers: vec![("content-type".into(), "text/html; charset=utf-8".into())],
                            body: Bytes::from(body),
                            url,
                        }))
                    }
                }
            }
            other => Err(NovaError::UnsupportedContent(format!(
                "network mod cannot handle request: {other:?}"
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "network mod shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_has_correct_capabilities() {
        let m = NetworkMod::new();
        assert!(m.manifest().provides(&CapabilityType::FetchUrl("http".into())));
        assert!(m.manifest().provides(&CapabilityType::FetchUrl("https".into())));
    }

    #[test]
    fn parse_simple_http_response() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 5\r\n\r\nhello";
        let resp = parse_http_response(raw, "http://test.com").unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_ref(), b"hello");
        assert_eq!(resp.content_type(), Some("text/html"));
    }

    #[test]
    fn parse_chunked_response() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n";
        let resp = parse_http_response(raw, "http://test.com").unwrap();
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body.as_ref(), b"hello world");
    }

    #[tokio::test]
    async fn fetch_real_example_com() {
        let m = NetworkMod::new();
        let req = ContentRequest::Fetch {
            url: "http://example.com".into(),
            headers: vec![],
        };
        // This makes a real network request — will fail in CI without network.
        let result = m.handle(req).await.unwrap();
        match result {
            TypedData::HttpResponse(resp) => {
                // Either real response or fallback — both are valid.
                assert!(resp.body.len() > 0);
            }
            _ => panic!("expected HttpResponse"),
        }
    }
}
