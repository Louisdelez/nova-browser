//! HTTP Cache with disk-backed storage.
//!
//! Implements RFC 7234 semantics for caching HTTP responses:
//! - `Cache-Control` header parsing (max-age, no-cache, no-store, must-revalidate, public, private)
//! - `Expires` header support
//! - `ETag` and `Last-Modified` for conditional requests
//! - LRU eviction with configurable maximum cache size
//! - Disk persistence in `~/.nova/cache/`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tracing::{debug, info, warn};

use nova_mod_api::content::HttpResponse;

/// Default maximum cache size in bytes (100 MB).
const DEFAULT_MAX_SIZE: u64 = 100 * 1024 * 1024;

/// A cached HTTP response with metadata for freshness checks.
#[derive(Debug, Clone)]
pub struct CachedResponse {
    /// The cached HTTP response.
    pub response: HttpResponse,
    /// When the response was stored (seconds since UNIX epoch).
    pub stored_at: u64,
    /// The `max-age` directive in seconds, if present.
    pub max_age: Option<u64>,
    /// The `Expires` header parsed as seconds since UNIX epoch.
    pub expires: Option<u64>,
    /// The `ETag` header value, if present.
    pub etag: Option<String>,
    /// The `Last-Modified` header value, if present.
    pub last_modified: Option<String>,
    /// Whether `Cache-Control: must-revalidate` was set.
    pub must_revalidate: bool,
    /// Size of the cached body in bytes.
    pub size: u64,
}

/// Parsed Cache-Control directives.
#[derive(Debug, Default)]
pub struct CacheControl {
    /// `max-age=N` directive.
    pub max_age: Option<u64>,
    /// `no-cache` directive — may cache but must revalidate.
    pub no_cache: bool,
    /// `no-store` directive — do not cache at all.
    pub no_store: bool,
    /// `must-revalidate` directive.
    pub must_revalidate: bool,
    /// `public` directive.
    pub public: bool,
    /// `private` directive.
    pub private: bool,
    /// `s-maxage=N` directive (for shared caches).
    pub s_maxage: Option<u64>,
}

/// Parse a `Cache-Control` header value into structured directives.
pub fn parse_cache_control(value: &str) -> CacheControl {
    let mut cc = CacheControl::default();
    for directive in value.split(',') {
        let directive = directive.trim().to_lowercase();
        if directive == "no-cache" {
            cc.no_cache = true;
        } else if directive == "no-store" {
            cc.no_store = true;
        } else if directive == "must-revalidate" {
            cc.must_revalidate = true;
        } else if directive == "public" {
            cc.public = true;
        } else if directive == "private" {
            cc.private = true;
        } else if let Some(age) = directive.strip_prefix("max-age=") {
            cc.max_age = age.trim().parse().ok();
        } else if let Some(age) = directive.strip_prefix("s-maxage=") {
            cc.s_maxage = age.trim().parse().ok();
        }
    }
    cc
}

/// Generate a cache key from a URL and HTTP method.
///
/// Only GET requests are cached (POST and other methods are not cacheable
/// by default). The key is a simple combination of method + URL.
pub fn cache_key(url: &str, method: &str) -> String {
    format!("{method}:{url}")
}

/// Get the current time as seconds since UNIX epoch.
fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

/// An in-memory + disk-backed HTTP cache.
///
/// Responses are stored in memory for fast lookup and persisted to disk
/// for cross-session caching. The cache implements LRU eviction when
/// the total size exceeds the configured maximum.
pub struct HttpCache {
    /// In-memory cache entries keyed by `method:url`.
    entries: Mutex<HashMap<String, CachedResponse>>,
    /// The on-disk cache directory.
    cache_dir: PathBuf,
    /// Maximum total cache size in bytes.
    max_size: u64,
    /// Current total size of all cached bodies.
    current_size: Mutex<u64>,
}

impl HttpCache {
    /// Create a new HTTP cache.
    ///
    /// The cache directory is created if it doesn't exist.
    /// Defaults to `~/.nova/cache/` if no explicit path is given.
    pub fn new(cache_dir: Option<PathBuf>, max_size: Option<u64>) -> Self {
        let dir = cache_dir.unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("/tmp"))
                .join(".nova")
                .join("cache")
        });

        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!(path = %dir.display(), error = %e, "failed to create cache directory");
        } else {
            info!(path = %dir.display(), "HTTP cache directory ready");
        }

        Self {
            entries: Mutex::new(HashMap::new()),
            cache_dir: dir,
            max_size: max_size.unwrap_or(DEFAULT_MAX_SIZE),
            current_size: Mutex::new(0),
        }
    }

    /// Look up a cached response for a request.
    ///
    /// Returns `None` if no entry exists or the entry is for a non-GET method.
    /// Returns `Some(cached)` even if stale — the caller should check freshness
    /// and issue conditional requests if needed.
    pub fn lookup(&self, url: &str, method: &str) -> Option<CachedResponse> {
        // Only cache GET requests.
        if method != "GET" {
            return None;
        }

        let key = cache_key(url, method);
        let entries = self.entries.lock().unwrap();
        let entry = entries.get(&key)?;

        debug!(url = %url, stored_at = entry.stored_at, "cache hit");
        Some(entry.clone())
    }

    /// Check if a cached response is still fresh.
    ///
    /// Uses `max-age` first, then falls back to `Expires`. Returns `true`
    /// if the response does not need revalidation.
    pub fn is_fresh(cached: &CachedResponse) -> bool {
        let now = now_epoch();

        // must-revalidate always requires a check.
        if cached.must_revalidate {
            return false;
        }

        // Check max-age.
        if let Some(max_age) = cached.max_age {
            let age = now.saturating_sub(cached.stored_at);
            if age < max_age {
                debug!(age, max_age, "cache entry is fresh (max-age)");
                return true;
            }
        }

        // Check Expires.
        if let Some(expires) = cached.expires {
            if now < expires {
                debug!(now, expires, "cache entry is fresh (Expires)");
                return true;
            }
        }

        debug!("cache entry is stale");
        false
    }

    /// Build conditional request headers for a stale cached response.
    ///
    /// Adds `If-None-Match` (for ETag) and/or `If-Modified-Since` (for
    /// Last-Modified) so the server can respond with 304 if unchanged.
    pub fn conditional_headers(cached: &CachedResponse) -> Vec<(String, String)> {
        let mut headers = Vec::new();

        if let Some(ref etag) = cached.etag {
            headers.push(("If-None-Match".to_string(), etag.clone()));
        }

        if let Some(ref last_modified) = cached.last_modified {
            headers.push(("If-Modified-Since".to_string(), last_modified.clone()));
        }

        headers
    }

    /// Store an HTTP response in the cache.
    ///
    /// Only stores the response if the `Cache-Control` directives allow it.
    /// Responses with `no-store` or non-2xx status codes are not cached.
    pub fn store(&self, url: &str, method: &str, response: &HttpResponse) {
        // Only cache GET requests.
        if method != "GET" {
            debug!(method, "not caching non-GET request");
            return;
        }

        // Only cache successful responses.
        if response.status < 200 || response.status >= 400 {
            debug!(status = response.status, "not caching non-2xx/3xx response");
            return;
        }

        // Parse Cache-Control.
        let cc = response
            .header("cache-control")
            .map(|v| parse_cache_control(v))
            .unwrap_or_default();

        // Do not cache if no-store.
        if cc.no_store {
            debug!(url = %url, "not caching: no-store");
            return;
        }

        // Do not cache private responses (we're a private cache, but skip
        // for simplicity if explicit max-age is missing).
        if cc.private && cc.max_age.is_none() {
            debug!(url = %url, "not caching: private without max-age");
            return;
        }

        let now = now_epoch();
        let body_size = response.body.len() as u64;

        // Parse ETag.
        let etag = response.header("etag").map(|v| v.to_string());

        // Parse Last-Modified.
        let last_modified = response.header("last-modified").map(|v| v.to_string());

        // Parse Expires as epoch seconds (simplified — just store the raw value).
        let expires = response
            .header("expires")
            .and_then(|v| parse_http_date(v));

        let cached = CachedResponse {
            response: response.clone(),
            stored_at: now,
            max_age: cc.max_age,
            expires,
            etag,
            last_modified,
            must_revalidate: cc.must_revalidate || cc.no_cache,
            size: body_size,
        };

        let key = cache_key(url, method);

        // Evict if needed.
        self.evict_if_needed(body_size);

        let mut entries = self.entries.lock().unwrap();
        let mut current_size = self.current_size.lock().unwrap();

        // Remove old entry size if replacing.
        if let Some(old) = entries.get(&key) {
            *current_size = current_size.saturating_sub(old.size);
        }

        *current_size += body_size;
        entries.insert(key.clone(), cached);

        debug!(
            url = %url,
            size = body_size,
            total_cached = entries.len(),
            total_size = *current_size,
            "stored in cache"
        );

        // Persist to disk asynchronously (best-effort).
        self.persist_entry(&key, &entries[&key]);
    }

    /// Update a cache entry with fresh headers from a 304 response.
    ///
    /// When a conditional request returns 304 Not Modified, the cached body
    /// is still valid but headers may have been updated.
    pub fn update_headers(&self, url: &str, method: &str, new_headers: &[(String, String)]) {
        let key = cache_key(url, method);
        let mut entries = self.entries.lock().unwrap();

        if let Some(entry) = entries.get_mut(&key) {
            // Update stored_at to refresh the freshness clock.
            entry.stored_at = now_epoch();

            // Update max-age if a new Cache-Control was sent.
            for (k, v) in new_headers {
                if k.to_lowercase() == "cache-control" {
                    let cc = parse_cache_control(v);
                    entry.max_age = cc.max_age;
                    entry.must_revalidate = cc.must_revalidate || cc.no_cache;
                } else if k.to_lowercase() == "etag" {
                    entry.etag = Some(v.clone());
                } else if k.to_lowercase() == "last-modified" {
                    entry.last_modified = Some(v.clone());
                } else if k.to_lowercase() == "expires" {
                    entry.expires = parse_http_date(v);
                }
            }

            debug!(url = %url, "updated cache entry headers after 304");
        }
    }

    /// Evict entries using LRU (oldest first) until there's room for `needed` bytes.
    fn evict_if_needed(&self, needed: u64) {
        let mut entries = self.entries.lock().unwrap();
        let mut current_size = self.current_size.lock().unwrap();

        while *current_size + needed > self.max_size && !entries.is_empty() {
            // Find the oldest entry.
            let oldest_key = entries
                .iter()
                .min_by_key(|(_, v)| v.stored_at)
                .map(|(k, _)| k.clone());

            if let Some(key) = oldest_key {
                if let Some(entry) = entries.remove(&key) {
                    *current_size = current_size.saturating_sub(entry.size);
                    debug!(key = %key, freed = entry.size, "evicted cache entry");

                    // Remove disk file.
                    let path = self.disk_path(&key);
                    let _ = std::fs::remove_file(&path);
                }
            } else {
                break;
            }
        }
    }

    /// Persist a cache entry to disk (best-effort).
    fn persist_entry(&self, key: &str, entry: &CachedResponse) {
        let path = self.disk_path(key);

        // Simple format: store just the body. Headers/metadata are in memory.
        if let Err(e) = std::fs::write(&path, &entry.response.body) {
            debug!(path = %path.display(), error = %e, "failed to persist cache entry");
        }
    }

    /// Get the disk path for a cache key.
    fn disk_path(&self, key: &str) -> PathBuf {
        // Hash the key for a filesystem-safe filename.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        key.hash(&mut hasher);
        let hash = hasher.finish();
        self.cache_dir.join(format!("{hash:016x}.cache"))
    }

    /// Clear all cached entries.
    pub fn clear(&self) {
        let mut entries = self.entries.lock().unwrap();
        let mut current_size = self.current_size.lock().unwrap();

        let count = entries.len();
        entries.clear();
        *current_size = 0;

        // Clear disk cache.
        if let Ok(dir) = std::fs::read_dir(&self.cache_dir) {
            for entry in dir.flatten() {
                if entry.path().extension().map(|e| e == "cache").unwrap_or(false) {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }

        if count > 0 {
            info!(cleared = count, "HTTP cache cleared");
        }
    }

    /// Get the number of cached entries.
    pub fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }

    /// Check if the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the total size of cached data in bytes.
    pub fn total_size(&self) -> u64 {
        *self.current_size.lock().unwrap()
    }

    /// Get the cache directory path.
    pub fn cache_dir(&self) -> &PathBuf {
        &self.cache_dir
    }
}

impl Default for HttpCache {
    fn default() -> Self {
        Self::new(None, None)
    }
}

/// Parse a simplified HTTP date string into seconds since UNIX epoch.
///
/// Supports the most common format: `Thu, 01 Jan 2099 00:00:00 GMT`.
/// Returns `None` if the date cannot be parsed.
fn parse_http_date(s: &str) -> Option<u64> {
    // Very simplified parser for "Day, DD Mon YYYY HH:MM:SS GMT".
    let parts: Vec<&str> = s.split_whitespace().collect();
    if parts.len() < 4 {
        return None;
    }

    let day: u64 = parts.get(1)?.parse().ok()?;
    let month = match parts.get(2)?.to_lowercase().as_str() {
        "jan" => 1u64,
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
    let year: u64 = parts.get(3)?.parse().ok()?;

    // Rough approximation: days since epoch.
    let days_since_epoch = (year - 1970) * 365 + (year - 1970) / 4 + (month - 1) * 30 + day;
    Some(days_since_epoch * 86400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cache_control_basic() {
        let cc = parse_cache_control("max-age=3600, public");
        assert_eq!(cc.max_age, Some(3600));
        assert!(cc.public);
        assert!(!cc.no_store);
        assert!(!cc.no_cache);
    }

    #[test]
    fn parse_cache_control_no_store() {
        let cc = parse_cache_control("no-store");
        assert!(cc.no_store);
        assert!(cc.max_age.is_none());
    }

    #[test]
    fn parse_cache_control_no_cache() {
        let cc = parse_cache_control("no-cache, must-revalidate");
        assert!(cc.no_cache);
        assert!(cc.must_revalidate);
    }

    #[test]
    fn parse_cache_control_private() {
        let cc = parse_cache_control("private, max-age=600");
        assert!(cc.private);
        assert_eq!(cc.max_age, Some(600));
    }

    #[test]
    fn cache_key_format() {
        assert_eq!(cache_key("https://example.com", "GET"), "GET:https://example.com");
    }

    #[test]
    fn cache_store_and_lookup() {
        let dir = std::env::temp_dir().join("nova-cache-test-store");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = HttpCache::new(Some(dir.clone()), Some(1024 * 1024));

        let response = HttpResponse {
            status: 200,
            headers: vec![
                ("cache-control".to_string(), "max-age=3600".to_string()),
                ("etag".to_string(), "\"abc123\"".to_string()),
            ],
            body: Bytes::from("hello world"),
            url: "https://example.com".to_string(),
        };

        cache.store("https://example.com", "GET", &response);
        assert_eq!(cache.len(), 1);

        let cached = cache.lookup("https://example.com", "GET");
        assert!(cached.is_some());
        let cached = cached.unwrap();
        assert_eq!(cached.response.status, 200);
        assert_eq!(cached.max_age, Some(3600));
        assert_eq!(cached.etag, Some("\"abc123\"".to_string()));
        assert!(HttpCache::is_fresh(&cached));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_no_store_not_cached() {
        let dir = std::env::temp_dir().join("nova-cache-test-nostore");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = HttpCache::new(Some(dir.clone()), Some(1024 * 1024));

        let response = HttpResponse {
            status: 200,
            headers: vec![
                ("cache-control".to_string(), "no-store".to_string()),
            ],
            body: Bytes::from("secret"),
            url: "https://example.com/secret".to_string(),
        };

        cache.store("https://example.com/secret", "GET", &response);
        assert_eq!(cache.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_post_not_cached() {
        let dir = std::env::temp_dir().join("nova-cache-test-post");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = HttpCache::new(Some(dir.clone()), Some(1024 * 1024));

        let response = HttpResponse {
            status: 200,
            headers: vec![],
            body: Bytes::from("result"),
            url: "https://example.com/api".to_string(),
        };

        cache.store("https://example.com/api", "POST", &response);
        assert_eq!(cache.len(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn conditional_headers_with_etag() {
        let cached = CachedResponse {
            response: HttpResponse {
                status: 200,
                headers: vec![],
                body: Bytes::new(),
                url: String::new(),
            },
            stored_at: 0,
            max_age: None,
            expires: None,
            etag: Some("\"abc\"".to_string()),
            last_modified: Some("Thu, 01 Jan 2025 00:00:00 GMT".to_string()),
            must_revalidate: false,
            size: 0,
        };

        let headers = HttpCache::conditional_headers(&cached);
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0, "If-None-Match");
        assert_eq!(headers[0].1, "\"abc\"");
        assert_eq!(headers[1].0, "If-Modified-Since");
    }

    #[test]
    fn cache_clear() {
        let dir = std::env::temp_dir().join("nova-cache-test-clear");
        let _ = std::fs::remove_dir_all(&dir);
        let cache = HttpCache::new(Some(dir.clone()), Some(1024 * 1024));

        let response = HttpResponse {
            status: 200,
            headers: vec![("cache-control".to_string(), "max-age=3600".to_string())],
            body: Bytes::from("data"),
            url: "https://example.com".to_string(),
        };

        cache.store("https://example.com", "GET", &response);
        assert_eq!(cache.len(), 1);

        cache.clear();
        assert_eq!(cache.len(), 0);
        assert_eq!(cache.total_size(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn freshness_check_max_age() {
        let now = now_epoch();
        let cached = CachedResponse {
            response: HttpResponse {
                status: 200,
                headers: vec![],
                body: Bytes::new(),
                url: String::new(),
            },
            stored_at: now,
            max_age: Some(3600),
            expires: None,
            etag: None,
            last_modified: None,
            must_revalidate: false,
            size: 0,
        };

        assert!(HttpCache::is_fresh(&cached));
    }

    #[test]
    fn freshness_stale_entry() {
        let cached = CachedResponse {
            response: HttpResponse {
                status: 200,
                headers: vec![],
                body: Bytes::new(),
                url: String::new(),
            },
            stored_at: 0, // epoch = 1970, definitely stale
            max_age: Some(60),
            expires: None,
            etag: None,
            last_modified: None,
            must_revalidate: false,
            size: 0,
        };

        assert!(!HttpCache::is_fresh(&cached));
    }

    #[test]
    fn must_revalidate_always_stale() {
        let now = now_epoch();
        let cached = CachedResponse {
            response: HttpResponse {
                status: 200,
                headers: vec![],
                body: Bytes::new(),
                url: String::new(),
            },
            stored_at: now,
            max_age: Some(3600),
            expires: None,
            etag: None,
            last_modified: None,
            must_revalidate: true,
            size: 0,
        };

        assert!(!HttpCache::is_fresh(&cached));
    }
}
