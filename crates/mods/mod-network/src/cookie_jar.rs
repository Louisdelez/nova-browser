//! # Cookie Jar
//!
//! Implements HTTP cookie storage and retrieval per RFC 6265.
//! Parses `Set-Cookie` headers, stores cookies, and provides
//! matching cookies for outgoing requests.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};
use url::Url;

/// The `SameSite` attribute of a cookie.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SameSite {
    /// Cookie is sent in all contexts.
    None,
    /// Cookie is sent with top-level navigations and GET requests from third-party sites.
    Lax,
    /// Cookie is only sent in a first-party context.
    Strict,
}

/// A single HTTP cookie.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cookie {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Domain the cookie applies to (lowercase, may start with `.` for domain cookies).
    pub domain: String,
    /// Path the cookie applies to.
    pub path: String,
    /// Expiration time, or `None` for session cookies.
    pub expires: Option<SystemTime>,
    /// Whether the cookie should only be sent over HTTPS.
    pub secure: bool,
    /// Whether the cookie is inaccessible to JavaScript.
    pub http_only: bool,
    /// The SameSite attribute.
    pub same_site: SameSite,
}

/// Thread-safe cookie storage.
#[derive(Debug, Clone)]
pub struct CookieJar {
    cookies: Arc<Mutex<Vec<Cookie>>>,
}

impl CookieJar {
    /// Create a new empty cookie jar.
    pub fn new() -> Self {
        Self {
            cookies: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Parse a `Set-Cookie` header value and return a `Cookie` if valid.
    ///
    /// Uses the request URL to determine the default domain and path
    /// when these attributes are not specified in the header.
    pub fn parse_set_cookie(header: &str, request_url: &Url) -> Option<Cookie> {
        let mut parts = header.split(';');

        // First part is name=value.
        let name_value = parts.next()?.trim();
        let (name, value) = name_value.split_once('=')?;
        let name = name.trim().to_string();
        let value = value.trim().to_string();

        if name.is_empty() {
            return None;
        }

        // Default domain from request URL.
        let default_domain = request_url.host_str().unwrap_or("").to_lowercase();
        // Default path: the directory of the request path.
        let default_path = {
            let p = request_url.path();
            match p.rfind('/') {
                Some(0) | None => "/".to_string(),
                Some(i) => p[..i].to_string(),
            }
        };

        let mut domain = default_domain.clone();
        let mut path = default_path;
        let mut expires: Option<SystemTime> = None;
        let mut secure = false;
        let mut http_only = false;
        let mut same_site = SameSite::Lax;

        for part in parts {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }

            if let Some((attr_name, attr_value)) = part.split_once('=') {
                let attr_name = attr_name.trim().to_lowercase();
                let attr_value = attr_value.trim();

                match attr_name.as_str() {
                    "domain" => {
                        let mut d = attr_value.to_lowercase();
                        // Strip leading dot for storage, we handle matching separately.
                        if d.starts_with('.') {
                            d = d[1..].to_string();
                        }
                        domain = d;
                    }
                    "path" => {
                        path = attr_value.to_string();
                    }
                    "expires" => {
                        if expires.is_none() {
                            // Try to parse HTTP date format.
                            expires = parse_http_date(attr_value);
                        }
                    }
                    "max-age" => {
                        if let Ok(seconds) = attr_value.parse::<i64>() {
                            if seconds <= 0 {
                                // Expired immediately.
                                expires =
                                    Some(SystemTime::UNIX_EPOCH);
                            } else {
                                expires = Some(
                                    SystemTime::now() + Duration::from_secs(seconds as u64),
                                );
                            }
                        }
                    }
                    "samesite" => {
                        same_site = match attr_value.to_lowercase().as_str() {
                            "none" => SameSite::None,
                            "strict" => SameSite::Strict,
                            _ => SameSite::Lax,
                        };
                    }
                    _ => {
                        // Unknown attribute, ignore.
                    }
                }
            } else {
                // Flag attributes (no value).
                match part.to_lowercase().as_str() {
                    "secure" => secure = true,
                    "httponly" => http_only = true,
                    _ => {}
                }
            }
        }

        // Validate: the domain must be a suffix of the request domain
        // (or equal to it).
        if domain != default_domain && !default_domain.ends_with(&format!(".{domain}")) {
            warn!(
                cookie = %name,
                domain = %domain,
                request_domain = %default_domain,
                "rejecting cookie: domain does not match request"
            );
            return None;
        }

        debug!(name = %name, domain = %domain, path = %path, "parsed Set-Cookie");

        Some(Cookie {
            name,
            value,
            domain,
            path,
            expires,
            secure,
            http_only,
            same_site,
        })
    }

    /// Store a cookie, replacing any existing cookie with the same
    /// name, domain, and path.
    pub fn store(&self, cookie: Cookie) {
        let mut cookies = self.cookies.lock().unwrap();

        // Remove existing cookie with same identity.
        cookies.retain(|c| {
            !(c.name == cookie.name && c.domain == cookie.domain && c.path == cookie.path)
        });

        debug!(
            name = %cookie.name,
            domain = %cookie.domain,
            path = %cookie.path,
            "storing cookie"
        );

        cookies.push(cookie);
    }

    /// Return all cookies that match the given URL.
    ///
    /// Matching rules:
    /// - Domain: the cookie domain must be a suffix of the request host
    ///   (or equal to it).
    /// - Path: the cookie path must be a prefix of the request path.
    /// - Secure: if the cookie has the Secure flag, the URL must use HTTPS.
    /// - Expiry: expired cookies are excluded (and removed).
    pub fn cookies_for_url(&self, url: &Url) -> Vec<Cookie> {
        let host = url.host_str().unwrap_or("").to_lowercase();
        let path = url.path();
        let is_secure = url.scheme() == "https";
        let now = SystemTime::now();

        let mut cookies = self.cookies.lock().unwrap();

        // Remove expired cookies.
        cookies.retain(|c| {
            if let Some(exp) = c.expires {
                exp > now
            } else {
                true // Session cookies never expire during the session.
            }
        });

        cookies
            .iter()
            .filter(|c| {
                // Domain matching.
                if !domain_matches(&host, &c.domain) {
                    return false;
                }
                // Path matching.
                if !path_matches(path, &c.path) {
                    return false;
                }
                // Secure check.
                if c.secure && !is_secure {
                    return false;
                }
                true
            })
            .cloned()
            .collect()
    }

    /// Format a list of cookies into a `Cookie` header value.
    ///
    /// Returns a string like `"name1=value1; name2=value2"`.
    pub fn to_cookie_header(cookies: &[Cookie]) -> String {
        cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// Return the number of stored cookies (for testing/diagnostics).
    pub fn len(&self) -> usize {
        self.cookies.lock().unwrap().len()
    }

    /// Return whether the cookie jar is empty.
    pub fn is_empty(&self) -> bool {
        self.cookies.lock().unwrap().is_empty()
    }

    /// Load cookies from a JSON file, merging them into the jar.
    ///
    /// Only loads non-expired, non-session cookies (those with an `expires` field).
    /// Silently ignores missing or malformed files.
    pub fn load_from_file(&self, path: &std::path::Path) {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) => {
                debug!(path = %path.display(), error = %e, "no cookie file to load");
                return;
            }
        };

        let loaded: Vec<Cookie> = match serde_json::from_str(&data) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "failed to parse cookie file");
                return;
            }
        };

        let now = SystemTime::now();
        let mut count = 0;
        for cookie in loaded {
            // Skip expired cookies.
            if let Some(exp) = cookie.expires {
                if exp <= now {
                    continue;
                }
            } else {
                // Session cookies are not persisted.
                continue;
            }
            self.store(cookie);
            count += 1;
        }

        debug!(path = %path.display(), count, "loaded cookies from file");
    }

    /// Save all non-session cookies to a JSON file.
    ///
    /// Only persists cookies that have an `expires` field (non-session cookies).
    /// Creates parent directories if needed.
    pub fn save_to_file(&self, path: &std::path::Path) {
        let cookies = self.cookies.lock().unwrap();
        let now = SystemTime::now();

        // Only persist non-session, non-expired cookies.
        let persistable: Vec<&Cookie> = cookies
            .iter()
            .filter(|c| {
                if let Some(exp) = c.expires {
                    exp > now
                } else {
                    false // Skip session cookies.
                }
            })
            .collect();

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(path = %path.display(), error = %e, "failed to create cookie dir");
                return;
            }
        }

        match serde_json::to_string_pretty(&persistable) {
            Ok(json) => {
                if let Err(e) = std::fs::write(path, json) {
                    warn!(path = %path.display(), error = %e, "failed to write cookie file");
                } else {
                    debug!(path = %path.display(), count = persistable.len(), "saved cookies to file");
                }
            }
            Err(e) => {
                warn!(error = %e, "failed to serialize cookies");
            }
        }
    }
}

impl Default for CookieJar {
    fn default() -> Self {
        Self::new()
    }
}

/// Check if the request host matches the cookie domain.
///
/// - `example.com` matches cookie domain `example.com`
/// - `sub.example.com` matches cookie domain `example.com`
/// - `example.com` does NOT match cookie domain `sub.example.com`
fn domain_matches(host: &str, cookie_domain: &str) -> bool {
    if host == cookie_domain {
        return true;
    }
    // host must end with ".{cookie_domain}"
    host.ends_with(&format!(".{cookie_domain}"))
}

/// Check if the request path matches the cookie path.
///
/// - `/path/sub` matches cookie path `/path`
/// - `/path` matches cookie path `/path`
/// - `/other` does NOT match cookie path `/path`
fn path_matches(request_path: &str, cookie_path: &str) -> bool {
    if request_path == cookie_path {
        return true;
    }
    if request_path.starts_with(cookie_path) {
        // The cookie path must be a proper prefix ending with `/`
        // or the next char in the request path must be `/`.
        if cookie_path.ends_with('/') {
            return true;
        }
        if request_path.as_bytes().get(cookie_path.len()) == Some(&b'/') {
            return true;
        }
    }
    false
}

/// Parse a simple HTTP date string into a `SystemTime`.
///
/// Supports the most common format: `Thu, 01 Dec 2025 00:00:00 GMT`
fn parse_http_date(s: &str) -> Option<SystemTime> {
    // Simple parser for "Day, DD Mon YYYY HH:MM:SS GMT"
    let s = s.trim();
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }

    // Try to find day, month, year, time.
    let (day, month_str, year, time_str) = if parts.len() >= 6 {
        // "Thu, 01 Dec 2025 00:00:00 GMT"
        (parts[1].trim_end_matches(','), parts[2], parts[3], parts[4])
    } else {
        return None;
    };

    let day: u64 = day.parse().ok()?;
    let year: u64 = year.parse().ok()?;
    let month: u64 = match month_str.to_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };

    let time_parts: Vec<&str> = time_str.split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u64 = time_parts[0].parse().ok()?;
    let minute: u64 = time_parts[1].parse().ok()?;
    let second: u64 = time_parts[2].parse().ok()?;

    // Convert to seconds since epoch (simplified, no leap seconds).
    let mut total_days: u64 = 0;
    for y in 1970..year {
        total_days += if is_leap_year(y) { 366 } else { 365 };
    }
    let days_in_months = [31, if is_leap_year(year) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    for m in 0..(month - 1) as usize {
        total_days += days_in_months[m];
    }
    total_days += day - 1;

    let total_seconds = total_days * 86400 + hour * 3600 + minute * 60 + second;

    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(total_seconds))
}

/// Check if a year is a leap year.
fn is_leap_year(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn parse_simple_cookie() {
        let url = test_url("https://example.com/path");
        let cookie = CookieJar::parse_set_cookie("session=abc123", &url).unwrap();
        assert_eq!(cookie.name, "session");
        assert_eq!(cookie.value, "abc123");
        assert_eq!(cookie.domain, "example.com");
        assert_eq!(cookie.path, "/");
        assert!(!cookie.secure);
        assert!(!cookie.http_only);
    }

    #[test]
    fn parse_cookie_with_attributes() {
        let url = test_url("https://example.com/app/page");
        let header = "id=42; Domain=example.com; Path=/app; Secure; HttpOnly; SameSite=Strict";
        let cookie = CookieJar::parse_set_cookie(header, &url).unwrap();
        assert_eq!(cookie.name, "id");
        assert_eq!(cookie.value, "42");
        assert_eq!(cookie.domain, "example.com");
        assert_eq!(cookie.path, "/app");
        assert!(cookie.secure);
        assert!(cookie.http_only);
        assert_eq!(cookie.same_site, SameSite::Strict);
    }

    #[test]
    fn parse_cookie_with_domain_dot_prefix() {
        let url = test_url("https://sub.example.com/");
        let header = "tok=x; Domain=.example.com";
        let cookie = CookieJar::parse_set_cookie(header, &url).unwrap();
        // Leading dot is stripped.
        assert_eq!(cookie.domain, "example.com");
    }

    #[test]
    fn parse_cookie_max_age() {
        let url = test_url("https://example.com/");
        let header = "temp=1; Max-Age=3600";
        let cookie = CookieJar::parse_set_cookie(header, &url).unwrap();
        assert!(cookie.expires.is_some());
        // Should expire roughly 1 hour from now.
        let exp = cookie.expires.unwrap();
        let now = SystemTime::now();
        let diff = exp.duration_since(now).unwrap();
        assert!(diff.as_secs() >= 3590 && diff.as_secs() <= 3610);
    }

    #[test]
    fn parse_cookie_max_age_zero() {
        let url = test_url("https://example.com/");
        let header = "old=val; Max-Age=0";
        let cookie = CookieJar::parse_set_cookie(header, &url).unwrap();
        assert!(cookie.expires.is_some());
        // Should already be expired.
        assert!(cookie.expires.unwrap() <= SystemTime::now());
    }

    #[test]
    fn parse_cookie_expires_date() {
        let url = test_url("https://example.com/");
        let header = "persist=yes; Expires=Thu, 01 Dec 2030 00:00:00 GMT";
        let cookie = CookieJar::parse_set_cookie(header, &url).unwrap();
        assert!(cookie.expires.is_some());
        // Should be in the future.
        assert!(cookie.expires.unwrap() > SystemTime::now());
    }

    #[test]
    fn reject_cookie_wrong_domain() {
        let url = test_url("https://example.com/");
        let header = "bad=1; Domain=evil.com";
        let result = CookieJar::parse_set_cookie(header, &url);
        assert!(result.is_none());
    }

    #[test]
    fn reject_empty_name() {
        let url = test_url("https://example.com/");
        let result = CookieJar::parse_set_cookie("=value", &url);
        assert!(result.is_none());
    }

    #[test]
    fn store_and_retrieve() {
        let jar = CookieJar::new();
        let url = test_url("https://example.com/app");
        let cookie = CookieJar::parse_set_cookie("key=val", &url).unwrap();
        jar.store(cookie);

        let cookies = jar.cookies_for_url(&url);
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "key");
        assert_eq!(cookies[0].value, "val");
    }

    #[test]
    fn store_replaces_existing() {
        let jar = CookieJar::new();
        let url = test_url("https://example.com/");
        let c1 = CookieJar::parse_set_cookie("key=old", &url).unwrap();
        let c2 = CookieJar::parse_set_cookie("key=new", &url).unwrap();
        jar.store(c1);
        jar.store(c2);

        assert_eq!(jar.len(), 1);
        let cookies = jar.cookies_for_url(&url);
        assert_eq!(cookies[0].value, "new");
    }

    #[test]
    fn domain_matching_subdomain() {
        let jar = CookieJar::new();
        let url = test_url("https://sub.example.com/");
        let cookie = CookieJar::parse_set_cookie("key=val; Domain=example.com", &url).unwrap();
        jar.store(cookie);

        // Should match sub.example.com.
        let cookies = jar.cookies_for_url(&test_url("https://sub.example.com/"));
        assert_eq!(cookies.len(), 1);

        // Should match other.example.com too.
        let cookies = jar.cookies_for_url(&test_url("https://other.example.com/"));
        assert_eq!(cookies.len(), 1);

        // Should NOT match notexample.com.
        let cookies = jar.cookies_for_url(&test_url("https://notexample.com/"));
        assert_eq!(cookies.len(), 0);
    }

    #[test]
    fn path_matching() {
        let jar = CookieJar::new();
        let url = test_url("https://example.com/app/page");
        let cookie = CookieJar::parse_set_cookie("key=val; Path=/app", &url).unwrap();
        jar.store(cookie);

        // /app/page should match.
        let cookies = jar.cookies_for_url(&test_url("https://example.com/app/page"));
        assert_eq!(cookies.len(), 1);

        // /app should match.
        let cookies = jar.cookies_for_url(&test_url("https://example.com/app"));
        assert_eq!(cookies.len(), 1);

        // /other should NOT match.
        let cookies = jar.cookies_for_url(&test_url("https://example.com/other"));
        assert_eq!(cookies.len(), 0);

        // /application should NOT match (not a path boundary).
        let cookies = jar.cookies_for_url(&test_url("https://example.com/application"));
        assert_eq!(cookies.len(), 0);
    }

    #[test]
    fn secure_cookie_not_sent_over_http() {
        let jar = CookieJar::new();
        let url = test_url("https://example.com/");
        let cookie = CookieJar::parse_set_cookie("sec=val; Secure", &url).unwrap();
        jar.store(cookie);

        // Should match HTTPS.
        let cookies = jar.cookies_for_url(&test_url("https://example.com/"));
        assert_eq!(cookies.len(), 1);

        // Should NOT match HTTP.
        let cookies = jar.cookies_for_url(&test_url("http://example.com/"));
        assert_eq!(cookies.len(), 0);
    }

    #[test]
    fn expired_cookies_removed() {
        let jar = CookieJar::new();
        let url = test_url("https://example.com/");

        // Store an already-expired cookie.
        let mut cookie = CookieJar::parse_set_cookie("old=val", &url).unwrap();
        cookie.expires = Some(SystemTime::UNIX_EPOCH);
        jar.store(cookie);

        // Should not be returned.
        let cookies = jar.cookies_for_url(&url);
        assert_eq!(cookies.len(), 0);

        // Should have been cleaned up.
        assert_eq!(jar.len(), 0);
    }

    #[test]
    fn to_cookie_header_format() {
        let url = test_url("https://example.com/");
        let c1 = CookieJar::parse_set_cookie("a=1", &url).unwrap();
        let c2 = CookieJar::parse_set_cookie("b=2", &url).unwrap();
        let header = CookieJar::to_cookie_header(&[c1, c2]);
        assert_eq!(header, "a=1; b=2");
    }

    #[test]
    fn parse_samesite_none() {
        let url = test_url("https://example.com/");
        let cookie =
            CookieJar::parse_set_cookie("k=v; SameSite=None; Secure", &url).unwrap();
        assert_eq!(cookie.same_site, SameSite::None);
    }

    #[test]
    fn default_path_from_request() {
        let url = test_url("https://example.com/a/b/c");
        let cookie = CookieJar::parse_set_cookie("k=v", &url).unwrap();
        assert_eq!(cookie.path, "/a/b");
    }
}
