//! # Downloads Manager
//!
//! Tracks download status, manages download destinations, and detects
//! downloadable content based on HTTP response headers and MIME types.
//! Downloads are saved to `~/Downloads/` by default.

use std::path::PathBuf;

use tracing::{debug, info, warn};

/// The status of a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadStatus {
    /// Download is in progress. Contains bytes downloaded so far.
    Downloading { bytes_downloaded: u64 },
    /// Download completed successfully.
    Complete,
    /// Download failed with an error message.
    Failed(String),
}

/// A single download entry.
#[derive(Debug, Clone)]
pub struct Download {
    /// The URL being downloaded.
    pub url: String,
    /// The filename (extracted from URL or Content-Disposition).
    pub filename: String,
    /// Full path where the file is saved.
    pub path: PathBuf,
    /// Total size in bytes (if known from Content-Length).
    pub total_bytes: Option<u64>,
    /// Current download status.
    pub status: DownloadStatus,
}

/// The downloads manager.
#[derive(Debug, Default)]
pub struct DownloadsManager {
    /// All downloads (active and completed).
    downloads: Vec<Download>,
    /// The default download directory.
    download_dir: PathBuf,
}

impl DownloadsManager {
    /// Create a new downloads manager.
    pub fn new() -> Self {
        let download_dir = dirs::download_dir()
            .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("./downloads"));

        Self {
            downloads: Vec::new(),
            download_dir,
        }
    }

    /// Check if a response should be treated as a download rather than displayed.
    ///
    /// A response is downloadable if:
    /// - It has a `Content-Disposition: attachment` header, or
    /// - Its MIME type is a binary/non-displayable type.
    pub fn is_downloadable(
        content_disposition: Option<&str>,
        content_type: Option<&str>,
    ) -> bool {
        // Check Content-Disposition header.
        if let Some(cd) = content_disposition {
            if cd.to_lowercase().contains("attachment") {
                return true;
            }
        }

        // Check MIME type for binary/non-displayable content.
        if let Some(ct) = content_type {
            let mime = ct.split(';').next().unwrap_or(ct).trim().to_lowercase();
            return matches!(
                mime.as_str(),
                "application/octet-stream"
                    | "application/zip"
                    | "application/gzip"
                    | "application/x-tar"
                    | "application/x-gzip"
                    | "application/x-bzip2"
                    | "application/x-7z-compressed"
                    | "application/x-rar-compressed"
                    | "application/pdf"
                    | "application/x-msdownload"
                    | "application/x-executable"
                    | "application/x-deb"
                    | "application/x-rpm"
                    | "application/x-apple-diskimage"
                    | "application/vnd.debian.binary-package"
                    | "application/java-archive"
            );
        }

        false
    }

    /// Extract a filename from a URL or Content-Disposition header.
    pub fn extract_filename(url: &str, content_disposition: Option<&str>) -> String {
        // Try Content-Disposition first.
        if let Some(cd) = content_disposition {
            if let Some(filename) = Self::parse_content_disposition_filename(cd) {
                return filename;
            }
        }

        // Fall back to URL path.
        if let Ok(parsed) = url::Url::parse(url) {
            let path = parsed.path();
            if let Some(name) = path.rsplit('/').next() {
                if !name.is_empty() && name.contains('.') {
                    return name.to_string();
                }
            }
        }

        // Last resort: generic filename.
        "download".to_string()
    }

    /// Parse the `filename=` parameter from a Content-Disposition header.
    fn parse_content_disposition_filename(cd: &str) -> Option<String> {
        for part in cd.split(';') {
            let part = part.trim();
            if let Some(rest) = part.strip_prefix("filename=") {
                let name = rest
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'');
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
            if let Some(rest) = part.strip_prefix("filename*=") {
                // RFC 5987 encoding: charset'language'value
                if let Some(value) = rest.split('\'').nth(2) {
                    let decoded = percent_decode(value);
                    if !decoded.is_empty() {
                        return Some(decoded);
                    }
                }
            }
        }
        None
    }

    /// Start a download, saving the content to the downloads directory.
    ///
    /// Returns the download index for tracking progress.
    pub fn start_download(
        &mut self,
        url: &str,
        filename: &str,
        total_bytes: Option<u64>,
    ) -> usize {
        // Ensure the download directory exists.
        if let Err(e) = std::fs::create_dir_all(&self.download_dir) {
            warn!(error = %e, "failed to create download directory");
        }

        let path = self.download_dir.join(filename);
        let download = Download {
            url: url.to_string(),
            filename: filename.to_string(),
            path,
            total_bytes,
            status: DownloadStatus::Downloading {
                bytes_downloaded: 0,
            },
        };

        info!(
            url = url,
            filename = filename,
            "download started"
        );

        let idx = self.downloads.len();
        self.downloads.push(download);
        idx
    }

    /// Write downloaded data to disk and mark the download as complete.
    pub fn complete_download(&mut self, idx: usize, data: &[u8]) {
        if let Some(dl) = self.downloads.get_mut(idx) {
            match std::fs::write(&dl.path, data) {
                Ok(()) => {
                    dl.status = DownloadStatus::Complete;
                    info!(
                        filename = %dl.filename,
                        size = data.len(),
                        path = %dl.path.display(),
                        "download complete"
                    );
                }
                Err(e) => {
                    dl.status = DownloadStatus::Failed(format!("write error: {e}"));
                    warn!(
                        filename = %dl.filename,
                        error = %e,
                        "download write failed"
                    );
                }
            }
        }
    }

    /// Mark a download as failed.
    pub fn fail_download(&mut self, idx: usize, reason: &str) {
        if let Some(dl) = self.downloads.get_mut(idx) {
            dl.status = DownloadStatus::Failed(reason.to_string());
            warn!(filename = %dl.filename, reason = reason, "download failed");
        }
    }

    /// Get all downloads.
    pub fn all(&self) -> &[Download] {
        &self.downloads
    }

    /// Get the count of active (in-progress) downloads.
    pub fn active_count(&self) -> usize {
        self.downloads
            .iter()
            .filter(|d| matches!(d.status, DownloadStatus::Downloading { .. }))
            .count()
    }

    /// Get the download directory path.
    pub fn download_dir(&self) -> &PathBuf {
        &self.download_dir
    }
}

/// Simple percent-decoding for filenames.
fn percent_decode(s: &str) -> String {
    let mut result = Vec::new();
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(
                &String::from_utf8_lossy(&bytes[i + 1..i + 3]),
                16,
            ) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_attachment_download() {
        assert!(DownloadsManager::is_downloadable(
            Some("attachment; filename=\"file.zip\""),
            None
        ));
    }

    #[test]
    fn detect_binary_download() {
        assert!(DownloadsManager::is_downloadable(
            None,
            Some("application/octet-stream")
        ));
        assert!(DownloadsManager::is_downloadable(
            None,
            Some("application/zip")
        ));
    }

    #[test]
    fn html_is_not_download() {
        assert!(!DownloadsManager::is_downloadable(
            None,
            Some("text/html; charset=utf-8")
        ));
    }

    #[test]
    fn extract_filename_from_cd() {
        let name = DownloadsManager::extract_filename(
            "https://example.com/dl",
            Some("attachment; filename=\"report.pdf\""),
        );
        assert_eq!(name, "report.pdf");
    }

    #[test]
    fn extract_filename_from_url() {
        let name = DownloadsManager::extract_filename(
            "https://example.com/files/archive.tar.gz",
            None,
        );
        assert_eq!(name, "archive.tar.gz");
    }

    #[test]
    fn extract_filename_fallback() {
        let name = DownloadsManager::extract_filename(
            "https://example.com/",
            None,
        );
        assert_eq!(name, "download");
    }

    #[test]
    fn percent_decode_basic() {
        assert_eq!(percent_decode("hello%20world"), "hello world");
        assert_eq!(percent_decode("file%2Fname.txt"), "file/name.txt");
    }

    #[test]
    fn download_lifecycle() {
        let mut mgr = DownloadsManager::new();
        let idx = mgr.start_download("https://example.com/file.zip", "file.zip", Some(1024));
        assert_eq!(mgr.active_count(), 1);
        assert!(matches!(
            mgr.all()[idx].status,
            DownloadStatus::Downloading { .. }
        ));

        mgr.fail_download(idx, "test error");
        assert_eq!(mgr.active_count(), 0);
        assert!(matches!(mgr.all()[idx].status, DownloadStatus::Failed(_)));
    }
}
