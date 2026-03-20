//! # fetch_api
//!
//! Implements the `fetch()` Web API for the NOVA JavaScript engine.
//!
//! ## Design
//!
//! The `fetch()` function bridges the synchronous JavaScript interpreter with
//! NOVA's async networking layer.  When JS calls `fetch(url)`, it:
//!
//! 1. Parses the URL, method, headers, and body from the JS arguments
//! 2. Routes a `ContentRequest::Fetch` through the `CoreApi` to the network mod
//! 3. Blocks on the async response using a dedicated tokio runtime
//! 4. Returns the response synchronously to the interpreter
//!
//! Because the interpreter is line-by-line and synchronous, `fetch()` blocks
//! until the HTTP response arrives.  The `Promise`-based API is simulated by
//! immediately resolving the promise with the response data.
//!
//! ## Supported API
//!
//! ```javascript
//! // Basic GET
//! fetch("https://api.example.com/data")
//!   .then(function(r) { return r.json(); })
//!   .then(function(data) { console.log(data); });
//!
//! // POST with headers
//! fetch("https://api.example.com/submit", {
//!     method: "POST",
//!     headers: { "Content-Type": "application/json" },
//!     body: JSON.stringify({ key: "value" })
//! });
//! ```

use std::sync::Arc;

use tracing::debug;

use nova_mod_api::content::{ContentRequest, TypedData};
use nova_mod_api::CoreApi;

/// Result of a fetch operation, ready to be consumed by the JS interpreter.
#[derive(Debug, Clone)]
pub struct FetchResponse {
    /// HTTP status code.
    pub status: u16,
    /// Response headers as key-value pairs.
    pub headers: Vec<(String, String)>,
    /// Response body as a UTF-8 string.
    pub body: String,
    /// The final URL (after redirects).
    pub url: String,
}

impl FetchResponse {
    /// Serialize the response to a JSON string for the JS interpreter.
    pub fn to_json_string(&self) -> String {
        let headers_json: String = self
            .headers
            .iter()
            .map(|(k, v)| {
                format!(
                    "\"{}\":\"{}\"",
                    escape_json_string(k),
                    escape_json_string(v)
                )
            })
            .collect::<Vec<_>>()
            .join(",");

        format!(
            "{{\"status\":{},\"headers\":{{{}}},\"body\":\"{}\",\"url\":\"{}\"}}",
            self.status,
            headers_json,
            escape_json_string(&self.body),
            escape_json_string(&self.url),
        )
    }
}

/// Escape a string for safe inclusion in a JSON value.
fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

/// Execute a fetch request synchronously by bridging to the async CoreApi.
///
/// This function spawns a blocking task on a dedicated tokio runtime to avoid
/// deadlocking the current async context.  If no tokio runtime is available
/// (e.g. in tests), it creates a temporary one.
///
/// # Arguments
///
/// * `core` - The CoreApi handle for routing requests to the network mod
/// * `url` - The URL to fetch
/// * `method` - HTTP method (GET, POST, etc.)
/// * `headers` - Request headers as key-value pairs
/// * `body` - Optional request body
pub fn execute_fetch(
    core: &Arc<dyn CoreApi>,
    url: &str,
    method: &str,
    headers: Vec<(String, String)>,
    _body: Option<&str>,
) -> Result<FetchResponse, String> {
    debug!(url = %url, method = %method, "fetch: starting request");

    // Build the content request.  The current ContentRequest::Fetch only
    // supports url + headers.  Method and body support will be added when the
    // network mod is extended.
    let mut req_headers = headers;

    // Add method as a pseudo-header so the network mod can read it.
    // This is a temporary approach until ContentRequest::Fetch gains
    // explicit method/body fields.
    if method != "GET" {
        req_headers.push(("x-nova-method".to_string(), method.to_string()));
    }

    let request = ContentRequest::Fetch {
        url: url.to_string(),
        headers: req_headers,
    };

    let core = Arc::clone(core);

    // We need to call an async function from synchronous code.
    // Use std::thread::spawn + a new tokio runtime to avoid blocking
    // the current tokio runtime (which would deadlock).
    let handle = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("failed to create tokio runtime: {e}"))?;

        rt.block_on(async move {
            match core.request(request).await {
                Ok(TypedData::HttpResponse(resp)) => {
                    let body = String::from_utf8_lossy(&resp.body).to_string();
                    Ok(FetchResponse {
                        status: resp.status,
                        headers: resp.headers,
                        body,
                        url: resp.url,
                    })
                }
                Ok(other) => Err(format!("unexpected response type: {other:?}")),
                Err(e) => Err(format!("fetch failed: {e}")),
            }
        })
    });

    match handle.join() {
        Ok(result) => result,
        Err(_) => Err("fetch thread panicked".to_string()),
    }
}

/// Parse a JSON-encoded headers object into key-value pairs.
///
/// Accepts a simplified JSON format: `{"key":"value","key2":"value2"}`.
/// Falls back gracefully on malformed input.
pub fn parse_headers_json(json: &str) -> Vec<(String, String)> {
    let json = json.trim();
    if json.is_empty() || json == "{}" || json == "null" || json == "undefined" {
        return Vec::new();
    }

    let mut headers = Vec::new();
    let inner = json
        .strip_prefix('{')
        .and_then(|s| s.strip_suffix('}'))
        .unwrap_or(json);

    // Simple parser for "key":"value" pairs.
    let mut remaining = inner.trim();
    while !remaining.is_empty() {
        // Skip leading comma/whitespace.
        remaining = remaining.trim_start_matches(',').trim();
        if remaining.is_empty() {
            break;
        }

        // Read key (quoted string).
        if let Some((key, rest)) = parse_json_string(remaining) {
            remaining = rest.trim();
            // Skip colon.
            remaining = remaining.strip_prefix(':').unwrap_or(remaining).trim();
            // Read value.
            if let Some((value, rest)) = parse_json_string(remaining) {
                headers.push((key, value));
                remaining = rest.trim();
            } else {
                break;
            }
        } else {
            break;
        }
    }

    headers
}

/// Parse a JSON-quoted string, returning the unquoted content and remaining input.
fn parse_json_string(s: &str) -> Option<(String, &str)> {
    let s = s.trim();
    if !s.starts_with('"') {
        return None;
    }

    let mut chars = s[1..].char_indices();
    let mut result = String::new();

    while let Some((i, c)) = chars.next() {
        match c {
            '"' => {
                return Some((result, &s[i + 2..]));
            }
            '\\' => {
                if let Some((_, escaped)) = chars.next() {
                    match escaped {
                        '"' => result.push('"'),
                        '\\' => result.push('\\'),
                        'n' => result.push('\n'),
                        'r' => result.push('\r'),
                        't' => result.push('\t'),
                        _ => {
                            result.push('\\');
                            result.push(escaped);
                        }
                    }
                }
            }
            _ => result.push(c),
        }
    }

    None // Unterminated string.
}

/// Parse fetch arguments from the interpreter's call expression.
///
/// Supports:
/// - `fetch("url")` — simple GET
/// - `fetch("url", { method: "POST", headers: {...}, body: "..." })` — full form
///
/// Returns `(url, method, headers, body)`.
pub fn parse_fetch_call(args: &str) -> (String, String, Vec<(String, String)>, Option<String>) {
    let args = args.trim();

    // Split on the first comma that's not inside quotes or braces.
    let (url_part, opts_part) = split_fetch_args(args);

    let url = unquote_simple(url_part.trim());
    let mut method = "GET".to_string();
    let mut headers = Vec::new();
    let mut body = None;

    if !opts_part.is_empty() {
        // Parse the options object.
        let opts = opts_part.trim();
        let inner = opts
            .strip_prefix('{')
            .and_then(|s| s.strip_suffix('}'))
            .unwrap_or(opts);

        // Extract method.
        if let Some(m) = extract_object_string_value(inner, "method") {
            method = m.to_uppercase();
        }

        // Extract headers (as a nested object).
        if let Some(h) = extract_object_value(inner, "headers") {
            headers = parse_headers_json(h);
        }

        // Extract body.
        if let Some(b) = extract_object_string_value(inner, "body") {
            body = Some(b);
        }
    }

    (url, method, headers, body)
}

/// Split fetch arguments into URL and options parts, respecting nesting.
fn split_fetch_args(args: &str) -> (&str, &str) {
    let mut depth = 0;
    let mut in_quote = false;
    let mut quote_char = '"';

    for (i, c) in args.char_indices() {
        match c {
            '"' | '\'' if !in_quote => {
                in_quote = true;
                quote_char = c;
            }
            c if c == quote_char && in_quote => {
                in_quote = false;
            }
            '{' | '[' if !in_quote => depth += 1,
            '}' | ']' if !in_quote => depth -= 1,
            ',' if !in_quote && depth == 0 => {
                return (&args[..i], args[i + 1..].trim());
            }
            _ => {}
        }
    }

    (args, "")
}

/// Remove surrounding quotes from a simple string.
fn unquote_simple(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"'))
        || (s.starts_with('\'') && s.ends_with('\''))
        || (s.starts_with('`') && s.ends_with('`'))
    {
        s[1..s.len() - 1].to_owned()
    } else {
        s.to_owned()
    }
}

/// Extract a string value from a simple JS object literal.
///
/// Looks for `key: "value"` or `key: 'value'` patterns.
fn extract_object_string_value(obj: &str, key: &str) -> Option<String> {
    // Try patterns: key: "value", key:"value", "key": "value"
    for pattern in &[
        format!("{key}:"),
        format!("{key} :"),
        format!("\"{key}\":"),
        format!("\"{key}\" :"),
    ] {
        if let Some(pos) = obj.find(pattern.as_str()) {
            let after = obj[pos + pattern.len()..].trim();
            if after.starts_with('"') || after.starts_with('\'') {
                let quote = after.chars().next().unwrap();
                let end = after[1..].find(quote)?;
                return Some(after[1..1 + end].to_string());
            }
        }
    }
    None
}

/// Extract a nested object value from a simple JS object literal.
///
/// Looks for `key: { ... }` and returns the full `{ ... }` substring.
fn extract_object_value<'a>(obj: &'a str, key: &str) -> Option<&'a str> {
    for pattern in &[
        format!("{key}:"),
        format!("{key} :"),
        format!("\"{key}\":"),
        format!("\"{key}\" :"),
    ] {
        if let Some(pos) = obj.find(pattern.as_str()) {
            let after = obj[pos + pattern.len()..].trim();
            if after.starts_with('{') {
                // Find matching closing brace.
                let mut depth = 0;
                for (i, c) in after.char_indices() {
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(&after[..=i]);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    None
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_json_basic() {
        assert_eq!(escape_json_string("hello"), "hello");
        assert_eq!(escape_json_string("he\"llo"), "he\\\"llo");
        assert_eq!(escape_json_string("line\nnew"), "line\\nnew");
    }

    #[test]
    fn parse_headers_json_basic() {
        let json = r#"{"Content-Type":"application/json","Accept":"text/html"}"#;
        let headers = parse_headers_json(json);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0], ("Content-Type".to_string(), "application/json".to_string()));
        assert_eq!(headers[1], ("Accept".to_string(), "text/html".to_string()));
    }

    #[test]
    fn parse_headers_json_empty() {
        assert!(parse_headers_json("{}").is_empty());
        assert!(parse_headers_json("").is_empty());
        assert!(parse_headers_json("null").is_empty());
    }

    #[test]
    fn parse_fetch_call_simple_get() {
        let (url, method, headers, body) = parse_fetch_call(r#""https://example.com/api""#);
        assert_eq!(url, "https://example.com/api");
        assert_eq!(method, "GET");
        assert!(headers.is_empty());
        assert!(body.is_none());
    }

    #[test]
    fn parse_fetch_call_post() {
        let args = r#""https://example.com/api", { method: "POST", body: "hello" }"#;
        let (url, method, _headers, body) = parse_fetch_call(args);
        assert_eq!(url, "https://example.com/api");
        assert_eq!(method, "POST");
        assert_eq!(body, Some("hello".to_string()));
    }

    #[test]
    fn parse_fetch_call_with_headers() {
        let args = r#""https://example.com/api", { method: "POST", headers: {"Content-Type":"application/json"}, body: "data" }"#;
        let (url, method, headers, body) = parse_fetch_call(args);
        assert_eq!(url, "https://example.com/api");
        assert_eq!(method, "POST");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Content-Type");
        assert_eq!(body, Some("data".to_string()));
    }

    #[test]
    fn fetch_response_to_json() {
        let resp = FetchResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "text/html".to_string())],
            body: "hello".to_string(),
            url: "https://example.com".to_string(),
        };
        let json = resp.to_json_string();
        assert!(json.contains("\"status\":200"));
        assert!(json.contains("\"body\":\"hello\""));
        assert!(json.contains("\"content-type\":\"text/html\""));
    }

    #[test]
    fn fetch_response_escapes_special_chars() {
        let resp = FetchResponse {
            status: 200,
            headers: vec![],
            body: "line1\nline2\t\"quoted\"".to_string(),
            url: "https://example.com".to_string(),
        };
        let json = resp.to_json_string();
        assert!(json.contains("\\n"));
        assert!(json.contains("\\t"));
        assert!(json.contains("\\\"quoted\\\""));
    }

    #[test]
    fn unquote_simple_works() {
        assert_eq!(unquote_simple("\"hello\""), "hello");
        assert_eq!(unquote_simple("'hello'"), "hello");
        assert_eq!(unquote_simple("`hello`"), "hello");
        assert_eq!(unquote_simple("hello"), "hello");
    }

    #[test]
    fn split_fetch_args_no_opts() {
        let (url, opts) = split_fetch_args(r#""https://example.com""#);
        assert_eq!(url, r#""https://example.com""#);
        assert_eq!(opts, "");
    }

    #[test]
    fn split_fetch_args_with_opts() {
        let (url, opts) = split_fetch_args(r#""https://example.com", { method: "POST" }"#);
        assert_eq!(url, r#""https://example.com""#);
        assert!(opts.contains("method"));
    }
}
