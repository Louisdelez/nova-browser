//! # Content Security Policy (CSP)
//!
//! Parses and enforces `Content-Security-Policy` HTTP response headers.
//! Supports the most common directives: `default-src`, `script-src`,
//! `style-src`, `img-src`, `connect-src`, `frame-src`, and `font-src`.

use std::collections::HashMap;

use tracing::{debug, warn};

/// A parsed Content Security Policy.
#[derive(Debug, Clone, Default)]
pub struct CspPolicy {
    /// Directives mapped to their allowed source lists.
    directives: HashMap<String, Vec<CspSource>>,
}

/// A single CSP source expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CspSource {
    /// `'self'` — same origin as the document.
    SelfOrigin,
    /// `'none'` — no sources allowed.
    None,
    /// `'unsafe-inline'` — allows inline scripts/styles.
    UnsafeInline,
    /// `'unsafe-eval'` — allows `eval()` and similar.
    UnsafeEval,
    /// `*` — wildcard, allows any source.
    Wildcard,
    /// A specific host/domain pattern (e.g. `https://cdn.example.com`, `*.example.com`).
    Host(String),
    /// A scheme source (e.g. `https:`, `data:`).
    Scheme(String),
}

impl CspPolicy {
    /// Parse a `Content-Security-Policy` header value into a `CspPolicy`.
    ///
    /// Directives are separated by `;`. Each directive has a name followed
    /// by a space-separated list of source expressions.
    ///
    /// # Example
    /// ```
    /// use nova_security::csp::CspPolicy;
    /// let policy = CspPolicy::parse("default-src 'self'; script-src 'none'");
    /// assert!(!policy.check_allowed("script-src", "https://evil.com", None));
    /// ```
    pub fn parse(header: &str) -> Self {
        let mut directives = HashMap::new();

        for directive_str in header.split(';') {
            let trimmed = directive_str.trim();
            if trimmed.is_empty() {
                continue;
            }

            let mut parts = trimmed.split_whitespace();
            let name = match parts.next() {
                Some(n) => n.to_lowercase(),
                None => continue,
            };

            let sources: Vec<CspSource> = parts.map(|s| Self::parse_source(s)).collect();

            if !sources.is_empty() {
                directives.insert(name, sources);
            }
        }

        debug!(
            directive_count = directives.len(),
            "parsed CSP policy"
        );

        Self { directives }
    }

    /// Parse a single source expression.
    fn parse_source(s: &str) -> CspSource {
        match s.to_lowercase().as_str() {
            "'self'" => CspSource::SelfOrigin,
            "'none'" => CspSource::None,
            "'unsafe-inline'" => CspSource::UnsafeInline,
            "'unsafe-eval'" => CspSource::UnsafeEval,
            "*" => CspSource::Wildcard,
            other => {
                if other.ends_with(':') && !other.contains('/') {
                    CspSource::Scheme(other.to_string())
                } else {
                    CspSource::Host(other.to_string())
                }
            }
        }
    }

    /// Check whether a resource URL is allowed under a given directive.
    ///
    /// `directive` is the CSP directive name (e.g. `"script-src"`).
    /// `resource_url` is the URL of the resource being loaded.
    /// `page_origin` is the origin of the page (for `'self'` matching).
    ///
    /// If the directive is not present, falls back to `default-src`.
    /// If neither is present, the resource is allowed (no policy = no restriction).
    ///
    /// Returns `true` if the resource is allowed, `false` if blocked.
    pub fn check_allowed(
        &self,
        directive: &str,
        resource_url: &str,
        page_origin: Option<&str>,
    ) -> bool {
        // Look up the specific directive, fall back to default-src.
        let sources = self
            .directives
            .get(directive)
            .or_else(|| self.directives.get("default-src"));

        let sources = match sources {
            Some(s) => s,
            None => return true, // No policy for this directive type.
        };

        // 'none' blocks everything.
        if sources.iter().any(|s| matches!(s, CspSource::None)) {
            warn!(
                directive = directive,
                url = resource_url,
                "CSP: blocked by 'none' directive"
            );
            return false;
        }

        // '*' allows everything.
        if sources.iter().any(|s| matches!(s, CspSource::Wildcard)) {
            return true;
        }

        let resource_origin = extract_origin(resource_url);

        for source in sources {
            match source {
                CspSource::SelfOrigin => {
                    if let Some(page_orig) = page_origin {
                        if let Some(ref res_orig) = resource_origin {
                            if res_orig == page_orig {
                                return true;
                            }
                        }
                    }
                }
                CspSource::Host(pattern) => {
                    if host_matches(pattern, resource_url) {
                        return true;
                    }
                }
                CspSource::Scheme(scheme) => {
                    if resource_url.starts_with(scheme) {
                        return true;
                    }
                }
                CspSource::UnsafeInline | CspSource::UnsafeEval => {
                    // These apply to inline scripts/styles, not URL-fetched resources.
                    // For URL checks we skip them.
                }
                CspSource::None | CspSource::Wildcard => {
                    // Already handled above.
                }
            }
        }

        warn!(
            directive = directive,
            url = resource_url,
            "CSP: resource blocked by policy"
        );
        false
    }

    /// Check if the policy has any directives at all.
    pub fn is_empty(&self) -> bool {
        self.directives.is_empty()
    }

    /// Check if `'unsafe-inline'` is allowed for a given directive.
    pub fn allows_unsafe_inline(&self, directive: &str) -> bool {
        let sources = self
            .directives
            .get(directive)
            .or_else(|| self.directives.get("default-src"));

        match sources {
            Some(sources) => sources.iter().any(|s| {
                matches!(
                    s,
                    CspSource::UnsafeInline | CspSource::Wildcard
                )
            }),
            None => true, // No policy = allowed.
        }
    }

    /// Check if `'unsafe-eval'` is allowed for a given directive.
    pub fn allows_unsafe_eval(&self, directive: &str) -> bool {
        let sources = self
            .directives
            .get(directive)
            .or_else(|| self.directives.get("default-src"));

        match sources {
            Some(sources) => sources.iter().any(|s| {
                matches!(
                    s,
                    CspSource::UnsafeEval | CspSource::Wildcard
                )
            }),
            None => true,
        }
    }
}

/// Extract the origin (`scheme://host[:port]`) from a URL.
fn extract_origin(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let scheme = parsed.scheme();
    let host = parsed.host_str()?;
    match parsed.port() {
        Some(port) => Some(format!("{scheme}://{host}:{port}")),
        None => Some(format!("{scheme}://{host}")),
    }
}

/// Check if a CSP host pattern matches a resource URL.
///
/// Supports:
/// - Exact host match: `https://cdn.example.com`
/// - Wildcard subdomain: `*.example.com`
/// - Scheme + host: `https://example.com`
fn host_matches(pattern: &str, url: &str) -> bool {
    let parsed_url = match url::Url::parse(url) {
        Ok(u) => u,
        Err(_) => return false,
    };

    let url_host = match parsed_url.host_str() {
        Some(h) => h.to_lowercase(),
        None => return false,
    };

    // Try to parse pattern as a URL.
    if let Ok(parsed_pattern) = url::Url::parse(pattern) {
        let pattern_host = match parsed_pattern.host_str() {
            Some(h) => h.to_lowercase(),
            None => return false,
        };

        // Check scheme match.
        if parsed_pattern.scheme() != parsed_url.scheme() {
            return false;
        }

        return hosts_match(&pattern_host, &url_host);
    }

    // Pattern might be just a host with optional wildcard.
    let lower_pattern = pattern.to_lowercase();
    hosts_match(&lower_pattern, &url_host)
}

/// Match a host pattern (possibly with `*.` prefix) against a host.
fn hosts_match(pattern: &str, host: &str) -> bool {
    if pattern == host {
        return true;
    }

    if let Some(suffix) = pattern.strip_prefix("*.") {
        // *.example.com matches sub.example.com and example.com
        if host == suffix || host.ends_with(&format!(".{suffix}")) {
            return true;
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_policy() {
        let policy = CspPolicy::parse("default-src 'self'; script-src 'none'");
        assert!(!policy.is_empty());
        assert_eq!(policy.directives.len(), 2);
    }

    #[test]
    fn check_self_allowed() {
        let policy = CspPolicy::parse("default-src 'self'");
        assert!(policy.check_allowed(
            "script-src",
            "https://example.com/app.js",
            Some("https://example.com")
        ));
    }

    #[test]
    fn check_self_blocked() {
        let policy = CspPolicy::parse("default-src 'self'");
        assert!(!policy.check_allowed(
            "script-src",
            "https://evil.com/bad.js",
            Some("https://example.com")
        ));
    }

    #[test]
    fn check_none_blocks_everything() {
        let policy = CspPolicy::parse("script-src 'none'");
        assert!(!policy.check_allowed(
            "script-src",
            "https://example.com/app.js",
            Some("https://example.com")
        ));
    }

    #[test]
    fn check_wildcard_allows_everything() {
        let policy = CspPolicy::parse("script-src *");
        assert!(policy.check_allowed(
            "script-src",
            "https://any.site/any.js",
            Some("https://example.com")
        ));
    }

    #[test]
    fn check_specific_host() {
        let policy = CspPolicy::parse("script-src https://cdn.example.com");
        assert!(policy.check_allowed(
            "script-src",
            "https://cdn.example.com/lib.js",
            Some("https://example.com")
        ));
        assert!(!policy.check_allowed(
            "script-src",
            "https://evil.com/bad.js",
            Some("https://example.com")
        ));
    }

    #[test]
    fn check_wildcard_subdomain() {
        let policy = CspPolicy::parse("img-src *.example.com");
        assert!(policy.check_allowed(
            "img-src",
            "https://cdn.example.com/img.png",
            None
        ));
        assert!(policy.check_allowed(
            "img-src",
            "https://images.example.com/photo.jpg",
            None
        ));
    }

    #[test]
    fn fallback_to_default_src() {
        let policy = CspPolicy::parse("default-src 'self'");
        // img-src not specified, falls back to default-src.
        assert!(policy.check_allowed(
            "img-src",
            "https://example.com/logo.png",
            Some("https://example.com")
        ));
        assert!(!policy.check_allowed(
            "img-src",
            "https://other.com/logo.png",
            Some("https://example.com")
        ));
    }

    #[test]
    fn no_policy_allows_all() {
        let policy = CspPolicy::parse("");
        assert!(policy.check_allowed(
            "script-src",
            "https://any.site/anything.js",
            None
        ));
    }

    #[test]
    fn scheme_source() {
        let policy = CspPolicy::parse("img-src https: data:");
        assert!(policy.check_allowed("img-src", "https://example.com/img.png", None));
        assert!(policy.check_allowed("img-src", "data:image/png;base64,abc", None));
        assert!(!policy.check_allowed("img-src", "http://example.com/img.png", None));
    }

    #[test]
    fn unsafe_inline_check() {
        let policy = CspPolicy::parse("script-src 'self' 'unsafe-inline'");
        assert!(policy.allows_unsafe_inline("script-src"));
        assert!(!policy.allows_unsafe_eval("script-src"));
    }

    #[test]
    fn unsafe_eval_check() {
        let policy = CspPolicy::parse("script-src 'self' 'unsafe-eval'");
        assert!(policy.allows_unsafe_eval("script-src"));
        assert!(!policy.allows_unsafe_inline("script-src"));
    }
}
