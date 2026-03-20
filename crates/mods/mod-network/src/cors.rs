//! # CORS (Cross-Origin Resource Sharing)
//!
//! Implements CORS validation for cross-origin HTTP requests.
//! For now, violations are logged as warnings but not blocked,
//! allowing a gradual rollout of full CORS enforcement.

use tracing::warn;

/// Result of a CORS validation check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CorsResult {
    /// The request is allowed by CORS policy.
    Allowed,
    /// The request would be blocked by CORS policy (with reason).
    Blocked(String),
}

/// Configuration for CORS validation.
#[derive(Debug, Clone)]
pub struct CorsConfig {
    /// Whether to enforce CORS (if false, violations are logged but allowed).
    pub enforce: bool,
}

impl Default for CorsConfig {
    fn default() -> Self {
        Self { enforce: false }
    }
}

/// HTTP methods considered "simple" by the CORS specification.
const SIMPLE_METHODS: &[&str] = &["GET", "HEAD", "POST"];

/// Header names that are always allowed in simple requests (case-insensitive).
const SIMPLE_HEADERS: &[&str] = &[
    "accept",
    "accept-language",
    "content-language",
    "content-type",
];

/// Content-Type values allowed in simple requests.
const SIMPLE_CONTENT_TYPES: &[&str] = &[
    "application/x-www-form-urlencoded",
    "multipart/form-data",
    "text/plain",
];

/// Check whether a request qualifies as a "simple request" under CORS.
///
/// Simple requests do not trigger a preflight OPTIONS request.
/// A request is simple if:
/// - The method is GET, HEAD, or POST
/// - Only simple headers are used
/// - If Content-Type is set, it must be one of the allowed values
pub fn is_simple_request(method: &str, headers: &[(String, String)]) -> bool {
    // Check method.
    if !SIMPLE_METHODS
        .iter()
        .any(|m| m.eq_ignore_ascii_case(method))
    {
        return false;
    }

    // Check headers.
    for (name, value) in headers {
        let lower = name.to_lowercase();
        if !SIMPLE_HEADERS.contains(&lower.as_str()) {
            return false;
        }
        // If content-type is present, it must be a simple value.
        if lower == "content-type" {
            let ct_lower = value.to_lowercase();
            let base_ct = ct_lower.split(';').next().unwrap_or("").trim();
            if !SIMPLE_CONTENT_TYPES.contains(&base_ct) {
                return false;
            }
        }
    }

    true
}

/// Validate a CORS response against the request origin.
///
/// Checks the `Access-Control-Allow-Origin` header in the response.
/// Returns `CorsResult::Allowed` if the origin is permitted, or
/// `CorsResult::Blocked` with a human-readable reason otherwise.
pub fn check_cors(request_origin: &str, response_headers: &[(String, String)]) -> CorsResult {
    let allow_origin = response_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("access-control-allow-origin"))
        .map(|(_, v)| v.as_str());

    match allow_origin {
        Some("*") => CorsResult::Allowed,
        Some(origin) if origin == request_origin => CorsResult::Allowed,
        Some(origin) => CorsResult::Blocked(format!(
            "Origin '{request_origin}' not allowed by Access-Control-Allow-Origin: '{origin}'"
        )),
        None => CorsResult::Blocked(format!(
            "No Access-Control-Allow-Origin header in response (origin: '{request_origin}')"
        )),
    }
}

/// Validate that the response allows the requested HTTP method.
///
/// Checks the `Access-Control-Allow-Methods` header for preflight responses.
pub fn check_cors_method(method: &str, response_headers: &[(String, String)]) -> CorsResult {
    let allow_methods = response_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("access-control-allow-methods"))
        .map(|(_, v)| v.as_str());

    match allow_methods {
        Some(methods) => {
            let allowed: Vec<&str> = methods.split(',').map(|m| m.trim()).collect();
            if allowed.iter().any(|m| m.eq_ignore_ascii_case(method)) || allowed.contains(&"*") {
                CorsResult::Allowed
            } else {
                CorsResult::Blocked(format!(
                    "Method '{method}' not in Access-Control-Allow-Methods: '{methods}'"
                ))
            }
        }
        None => {
            // If no Allow-Methods header, simple methods are implicitly allowed.
            if SIMPLE_METHODS
                .iter()
                .any(|m| m.eq_ignore_ascii_case(method))
            {
                CorsResult::Allowed
            } else {
                CorsResult::Blocked(format!(
                    "No Access-Control-Allow-Methods header for method '{method}'"
                ))
            }
        }
    }
}

/// Validate that the response allows the requested headers.
///
/// Checks the `Access-Control-Allow-Headers` header for preflight responses.
pub fn check_cors_headers(
    request_headers: &[(String, String)],
    response_headers: &[(String, String)],
) -> CorsResult {
    let allow_headers = response_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("access-control-allow-headers"))
        .map(|(_, v)| v.as_str());

    // Collect non-simple request headers.
    let non_simple: Vec<&str> = request_headers
        .iter()
        .filter(|(k, _)| !SIMPLE_HEADERS.contains(&k.to_lowercase().as_str()))
        .map(|(k, _)| k.as_str())
        .collect();

    if non_simple.is_empty() {
        return CorsResult::Allowed;
    }

    match allow_headers {
        Some(headers_str) => {
            let allowed: Vec<String> = headers_str
                .split(',')
                .map(|h| h.trim().to_lowercase())
                .collect();
            let wildcard = allowed.contains(&"*".to_string());

            for header in &non_simple {
                let lower = header.to_lowercase();
                if !wildcard && !allowed.contains(&lower) {
                    return CorsResult::Blocked(format!(
                        "Header '{header}' not in Access-Control-Allow-Headers: '{headers_str}'"
                    ));
                }
            }
            CorsResult::Allowed
        }
        None => CorsResult::Blocked(format!(
            "No Access-Control-Allow-Headers for non-simple headers: {:?}",
            non_simple
        )),
    }
}

/// Perform a full CORS check on a response, logging warnings for violations.
///
/// Returns `CorsResult::Allowed` or `CorsResult::Blocked`. When `config.enforce`
/// is `false`, blocked results are logged as warnings but still returned so the
/// caller can decide whether to proceed.
pub fn validate_cors(
    request_origin: &str,
    request_method: &str,
    request_headers: &[(String, String)],
    response_headers: &[(String, String)],
    config: &CorsConfig,
) -> CorsResult {
    // Check origin.
    let origin_check = check_cors(request_origin, response_headers);
    if let CorsResult::Blocked(ref reason) = origin_check {
        if config.enforce {
            return origin_check;
        }
        warn!(reason = %reason, "CORS origin check failed (not enforced)");
    }

    // For non-simple requests, also check method and headers.
    if !is_simple_request(request_method, request_headers) {
        let method_check = check_cors_method(request_method, response_headers);
        if let CorsResult::Blocked(ref reason) = method_check {
            if config.enforce {
                return method_check;
            }
            warn!(reason = %reason, "CORS method check failed (not enforced)");
        }

        let header_check = check_cors_headers(request_headers, response_headers);
        if let CorsResult::Blocked(ref reason) = header_check {
            if config.enforce {
                return header_check;
            }
            warn!(reason = %reason, "CORS header check failed (not enforced)");
        }
    }

    CorsResult::Allowed
}

/// Extract the origin from a URL string.
///
/// Returns `scheme://host[:port]`, or `None` if the URL cannot be parsed.
pub fn extract_origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    match parsed.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_get_is_simple() {
        assert!(is_simple_request("GET", &[]));
        assert!(is_simple_request("HEAD", &[]));
        assert!(is_simple_request("POST", &[]));
    }

    #[test]
    fn put_delete_not_simple() {
        assert!(!is_simple_request("PUT", &[]));
        assert!(!is_simple_request("DELETE", &[]));
        assert!(!is_simple_request("PATCH", &[]));
    }

    #[test]
    fn simple_headers_allowed() {
        let headers = vec![
            ("Accept".to_string(), "text/html".to_string()),
            ("Content-Language".to_string(), "en".to_string()),
        ];
        assert!(is_simple_request("GET", &headers));
    }

    #[test]
    fn custom_header_not_simple() {
        let headers = vec![("X-Custom-Header".to_string(), "value".to_string())];
        assert!(!is_simple_request("GET", &headers));
    }

    #[test]
    fn post_with_json_content_type_not_simple() {
        let headers = vec![(
            "Content-Type".to_string(),
            "application/json".to_string(),
        )];
        assert!(!is_simple_request("POST", &headers));
    }

    #[test]
    fn post_with_form_content_type_is_simple() {
        let headers = vec![(
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".to_string(),
        )];
        assert!(is_simple_request("POST", &headers));
    }

    #[test]
    fn cors_wildcard_allows_all() {
        let headers = vec![(
            "Access-Control-Allow-Origin".to_string(),
            "*".to_string(),
        )];
        assert_eq!(
            check_cors("http://example.com", &headers),
            CorsResult::Allowed
        );
    }

    #[test]
    fn cors_matching_origin_allowed() {
        let headers = vec![(
            "Access-Control-Allow-Origin".to_string(),
            "http://example.com".to_string(),
        )];
        assert_eq!(
            check_cors("http://example.com", &headers),
            CorsResult::Allowed
        );
    }

    #[test]
    fn cors_mismatched_origin_blocked() {
        let headers = vec![(
            "Access-Control-Allow-Origin".to_string(),
            "http://other.com".to_string(),
        )];
        let result = check_cors("http://example.com", &headers);
        assert!(matches!(result, CorsResult::Blocked(_)));
    }

    #[test]
    fn cors_missing_header_blocked() {
        let headers: Vec<(String, String)> = vec![];
        let result = check_cors("http://example.com", &headers);
        assert!(matches!(result, CorsResult::Blocked(_)));
    }

    #[test]
    fn cors_method_allowed() {
        let headers = vec![(
            "Access-Control-Allow-Methods".to_string(),
            "GET, POST, PUT".to_string(),
        )];
        assert_eq!(
            check_cors_method("PUT", &headers),
            CorsResult::Allowed
        );
    }

    #[test]
    fn cors_method_blocked() {
        let headers = vec![(
            "Access-Control-Allow-Methods".to_string(),
            "GET, POST".to_string(),
        )];
        let result = check_cors_method("DELETE", &headers);
        assert!(matches!(result, CorsResult::Blocked(_)));
    }

    #[test]
    fn extract_origin_basic() {
        assert_eq!(
            extract_origin("https://example.com/path?q=1"),
            Some("https://example.com".to_string())
        );
    }

    #[test]
    fn extract_origin_with_port() {
        assert_eq!(
            extract_origin("http://localhost:3000/api"),
            Some("http://localhost:3000".to_string())
        );
    }

    #[test]
    fn validate_cors_non_enforced_allows() {
        let config = CorsConfig { enforce: false };
        let response_headers: Vec<(String, String)> = vec![];
        let result = validate_cors(
            "http://example.com",
            "GET",
            &[],
            &response_headers,
            &config,
        );
        // Non-enforced mode should still return Allowed.
        assert_eq!(result, CorsResult::Allowed);
    }

    #[test]
    fn validate_cors_enforced_blocks() {
        let config = CorsConfig { enforce: true };
        let response_headers: Vec<(String, String)> = vec![];
        let result = validate_cors(
            "http://example.com",
            "GET",
            &[],
            &response_headers,
            &config,
        );
        assert!(matches!(result, CorsResult::Blocked(_)));
    }
}
