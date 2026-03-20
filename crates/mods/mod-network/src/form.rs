//! HTML form data encoding and submission helpers.
//!
//! Supports `application/x-www-form-urlencoded` and `multipart/form-data`
//! encoding for HTML form submissions (GET and POST).

use tracing::debug;

/// A single form field entry.
#[derive(Debug, Clone)]
pub struct FormEntry {
    /// The `name` attribute of the form field.
    pub name: String,
    /// The value of the form field.
    pub value: String,
    /// Optional filename (for file upload fields).
    pub filename: Option<String>,
    /// Optional content type (for file upload fields, defaults to `application/octet-stream`).
    pub content_type: Option<String>,
}

/// Collected form data from an HTML form, ready for encoding.
#[derive(Debug, Clone)]
pub struct FormData {
    /// The form field entries.
    pub entries: Vec<FormEntry>,
}

impl FormData {
    /// Create an empty `FormData`.
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Create `FormData` from DOM form elements.
    ///
    /// Takes a slice of `(name, value, type)` tuples representing the form
    /// fields. Fields with empty names are skipped. Submit/button/reset fields
    /// are excluded as they are not submitted with the form data.
    pub fn from_dom_form(elements: &[(String, String, String)]) -> Self {
        let entries: Vec<FormEntry> = elements
            .iter()
            .filter(|(name, _, field_type)| {
                !name.is_empty()
                    && !matches!(
                        field_type.as_str(),
                        "submit" | "button" | "reset"
                    )
            })
            .map(|(name, value, _)| FormEntry {
                name: name.clone(),
                value: value.clone(),
                filename: None,
                content_type: None,
            })
            .collect();

        debug!(count = entries.len(), "FormData created from DOM form");
        Self { entries }
    }

    /// Add a text entry to the form data.
    pub fn add(&mut self, name: &str, value: &str) {
        self.entries.push(FormEntry {
            name: name.to_string(),
            value: value.to_string(),
            filename: None,
            content_type: None,
        });
    }

    /// Add a file entry to the form data.
    pub fn add_file(&mut self, name: &str, filename: &str, content_type: &str, data: &str) {
        self.entries.push(FormEntry {
            name: name.to_string(),
            value: data.to_string(),
            filename: Some(filename.to_string()),
            content_type: Some(content_type.to_string()),
        });
    }

    /// Encode the form data as `application/x-www-form-urlencoded`.
    ///
    /// Percent-encodes special characters and replaces spaces with `+`.
    /// Returns a string like `name1=value1&name2=value2`.
    pub fn to_url_encoded(&self) -> String {
        self.entries
            .iter()
            .map(|e| {
                format!(
                    "{}={}",
                    percent_encode(&e.name),
                    percent_encode(&e.value)
                )
            })
            .collect::<Vec<_>>()
            .join("&")
    }

    /// Encode the form data as `multipart/form-data`.
    ///
    /// Returns a tuple of `(boundary, body_bytes)` where:
    /// - `boundary` is the MIME boundary string (without dashes)
    /// - `body_bytes` is the complete multipart body
    pub fn to_multipart(&self) -> (String, Vec<u8>) {
        let boundary = generate_boundary();
        let mut body = Vec::new();

        for entry in &self.entries {
            // Boundary delimiter.
            body.extend_from_slice(b"--");
            body.extend_from_slice(boundary.as_bytes());
            body.extend_from_slice(b"\r\n");

            // Content-Disposition header.
            if let Some(ref filename) = entry.filename {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                        entry.name, filename
                    )
                    .as_bytes(),
                );
                // Content-Type for file uploads.
                let ct = entry
                    .content_type
                    .as_deref()
                    .unwrap_or("application/octet-stream");
                body.extend_from_slice(format!("Content-Type: {ct}\r\n").as_bytes());
            } else {
                body.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{}\"\r\n",
                        entry.name
                    )
                    .as_bytes(),
                );
            }

            // Empty line separating headers from value.
            body.extend_from_slice(b"\r\n");
            // Value.
            body.extend_from_slice(entry.value.as_bytes());
            body.extend_from_slice(b"\r\n");
        }

        // Closing boundary.
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(b"--\r\n");

        debug!(
            boundary = %boundary,
            body_len = body.len(),
            entries = self.entries.len(),
            "multipart form data encoded"
        );

        (boundary, body)
    }
}

impl Default for FormData {
    fn default() -> Self {
        Self::new()
    }
}

/// Percent-encode a string for `application/x-www-form-urlencoded`.
///
/// Unreserved characters (A-Z, a-z, 0-9, `-`, `_`, `.`, `~`) are left as-is.
/// Spaces are encoded as `+`. All other bytes are percent-encoded.
pub fn percent_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push('+'),
            _ => {
                result.push('%');
                result.push_str(&format!("{byte:02X}"));
            }
        }
    }
    result
}

/// Build a full URL for a GET form submission.
///
/// Resolves `action` against `base_url`, then appends the form data as a
/// query string. If the action URL already has a query string, the form
/// data is appended with `&`.
pub fn build_form_url(action: &str, base_url: &str, data: &FormData) -> String {
    let resolved = if action.is_empty() {
        base_url.to_string()
    } else if action.starts_with("http://") || action.starts_with("https://") {
        action.to_string()
    } else if let Ok(base) = url::Url::parse(base_url) {
        base.join(action)
            .map(|u| u.to_string())
            .unwrap_or_else(|_| action.to_string())
    } else {
        action.to_string()
    };

    let query = data.to_url_encoded();
    if query.is_empty() {
        return resolved;
    }

    if resolved.contains('?') {
        format!("{resolved}&{query}")
    } else {
        format!("{resolved}?{query}")
    }
}

/// Generate a unique boundary string for multipart encoding.
fn generate_boundary() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("----NovaFormBoundary{nanos:010}")
}

/// Parse form submission attributes from an HTML form element.
///
/// Returns `(action, method, enctype)` with defaults applied.
pub fn parse_form_attributes(
    action: Option<&str>,
    method: Option<&str>,
    enctype: Option<&str>,
) -> (String, String, String) {
    let action = action.unwrap_or("").to_string();
    let method = method
        .map(|m| m.to_lowercase())
        .unwrap_or_else(|| "get".into());
    let enctype = enctype
        .unwrap_or("application/x-www-form-urlencoded")
        .to_string();
    (action, method, enctype)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_encode_simple_text() {
        assert_eq!(percent_encode("hello"), "hello");
    }

    #[test]
    fn url_encode_spaces() {
        assert_eq!(percent_encode("hello world"), "hello+world");
    }

    #[test]
    fn url_encode_special_chars() {
        assert_eq!(percent_encode("a=b&c=d"), "a%3Db%26c%3Dd");
    }

    #[test]
    fn url_encode_unicode() {
        let encoded = percent_encode("café");
        assert!(encoded.contains("%C3%A9")); // é = 0xC3 0xA9 in UTF-8
    }

    #[test]
    fn url_encode_preserves_unreserved() {
        assert_eq!(percent_encode("A-Z_a.z~0"), "A-Z_a.z~0");
    }

    #[test]
    fn form_data_to_url_encoded() {
        let mut data = FormData::new();
        data.add("q", "rust programming");
        data.add("page", "1");
        assert_eq!(data.to_url_encoded(), "q=rust+programming&page=1");
    }

    #[test]
    fn form_data_to_url_encoded_empty() {
        let data = FormData::new();
        assert_eq!(data.to_url_encoded(), "");
    }

    #[test]
    fn form_data_from_dom_form() {
        let elements = vec![
            ("q".into(), "test".into(), "text".into()),
            ("".into(), "ignored".into(), "text".into()),      // empty name => skipped
            ("btn".into(), "Go".into(), "submit".into()),       // submit => skipped
        ];
        let data = FormData::from_dom_form(&elements);
        assert_eq!(data.entries.len(), 1);
        assert_eq!(data.entries[0].name, "q");
        assert_eq!(data.entries[0].value, "test");
    }

    #[test]
    fn form_data_from_dom_excludes_button_reset() {
        let elements = vec![
            ("name".into(), "value".into(), "text".into()),
            ("btn".into(), "Click".into(), "button".into()),
            ("rst".into(), "Reset".into(), "reset".into()),
        ];
        let data = FormData::from_dom_form(&elements);
        assert_eq!(data.entries.len(), 1);
    }

    #[test]
    fn multipart_boundary_format() {
        let mut data = FormData::new();
        data.add("field", "value");
        let (boundary, body) = data.to_multipart();
        let body_str = String::from_utf8_lossy(&body);

        assert!(boundary.starts_with("----NovaFormBoundary"));
        assert!(body_str.contains(&format!("--{boundary}")));
        assert!(body_str.contains("Content-Disposition: form-data; name=\"field\""));
        assert!(body_str.contains("value"));
        assert!(body_str.ends_with(&format!("--{boundary}--\r\n")));
    }

    #[test]
    fn multipart_with_file() {
        let mut data = FormData::new();
        data.add_file("upload", "test.txt", "text/plain", "file contents");
        let (_, body) = data.to_multipart();
        let body_str = String::from_utf8_lossy(&body);

        assert!(body_str.contains("filename=\"test.txt\""));
        assert!(body_str.contains("Content-Type: text/plain"));
        assert!(body_str.contains("file contents"));
    }

    #[test]
    fn build_form_url_get_simple() {
        let mut data = FormData::new();
        data.add("q", "rust");
        let url = build_form_url("/search", "https://example.com/page", &data);
        assert_eq!(url, "https://example.com/search?q=rust");
    }

    #[test]
    fn build_form_url_get_empty_action() {
        let mut data = FormData::new();
        data.add("q", "test");
        let url = build_form_url("", "https://example.com/page", &data);
        assert_eq!(url, "https://example.com/page?q=test");
    }

    #[test]
    fn build_form_url_get_absolute_action() {
        let mut data = FormData::new();
        data.add("q", "test");
        let url = build_form_url("https://other.com/search", "https://example.com/", &data);
        assert_eq!(url, "https://other.com/search?q=test");
    }

    #[test]
    fn build_form_url_existing_query_string() {
        let mut data = FormData::new();
        data.add("extra", "1");
        let url = build_form_url("/search?lang=en", "https://example.com/", &data);
        assert_eq!(url, "https://example.com/search?lang=en&extra=1");
    }

    #[test]
    fn build_form_url_no_data() {
        let data = FormData::new();
        let url = build_form_url("/search", "https://example.com/", &data);
        assert_eq!(url, "https://example.com/search");
    }

    #[test]
    fn parse_form_attributes_defaults() {
        let (action, method, enctype) = parse_form_attributes(None, None, None);
        assert_eq!(action, "");
        assert_eq!(method, "get");
        assert_eq!(enctype, "application/x-www-form-urlencoded");
    }

    #[test]
    fn parse_form_attributes_post() {
        let (_, method, _) = parse_form_attributes(Some("/submit"), Some("POST"), None);
        assert_eq!(method, "post");
    }

    #[test]
    fn parse_form_attributes_multipart() {
        let (_, _, enctype) = parse_form_attributes(
            Some("/upload"),
            Some("post"),
            Some("multipart/form-data"),
        );
        assert_eq!(enctype, "multipart/form-data");
    }

    #[test]
    fn form_data_multiple_fields_url_encoded() {
        let mut data = FormData::new();
        data.add("first", "John");
        data.add("last", "Doe");
        data.add("email", "john@example.com");
        let encoded = data.to_url_encoded();
        assert_eq!(encoded, "first=John&last=Doe&email=john%40example.com");
    }
}
