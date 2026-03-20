//! # nova-pipeline
//!
//! The Pipeline Engine — the brain of the core.
//! Orchestrates the full journey from URL to pixels on screen.
//!
//! The pipeline doesn't do any work itself. It sequences requests
//! to the capability registry, which routes them to the right mods.

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tracing::{debug, info, warn};
use url::Url;

use nova_mod_api::{
    CapabilityType, ContentRequest, NovaError, TypedData, Viewport,
    content::{DomNode, HttpResponse},
};
use mod_css_engine::FontFaceUrl;
use nova_registry::CapabilityRegistry;

/// Maximum number of images to fetch during a navigation.
/// Pages with more images will have the rest skipped for faster initial display.
const MAX_IMAGES_TO_FETCH: usize = 20;

/// Timeout for fetching + decoding a single image.
/// If an image takes longer than this, it is skipped.
const IMAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// Sub-resources extracted from a parsed DOM tree.
#[derive(Debug, Default)]
struct SubResources {
    /// External stylesheet URLs.
    stylesheets: Vec<String>,
    /// Image entries: `(original_src, resolved_url)`.
    /// The original_src is the raw HTML attribute value (used as key in the
    /// painter's image lookup), the resolved_url is used for fetching.
    images: Vec<(String, String)>,
    /// External script URLs.
    scripts: Vec<String>,
    /// Inline script text (from `<script>...</script>`).
    inline_scripts: Vec<String>,
}

/// The pipeline engine. Stateless — each navigation creates a new pipeline run.
pub struct PipelineEngine {
    registry: Arc<CapabilityRegistry>,
}

impl PipelineEngine {
    pub fn new(registry: Arc<CapabilityRegistry>) -> Self {
        Self { registry }
    }

    /// Navigate to a URL. This is the main entry point.
    /// Orchestrates: Fetch → Detect → Parse → Style → Layout → Paint → Composite.
    pub async fn navigate(
        &self,
        url: &str,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        info!("Pipeline: navigating to '{url}'");

        // Step 1: Fetch
        let response = self.fetch(url).await?;
        let mime_type = response
            .content_type()
            .unwrap_or("text/html")
            .split(';')
            .next()
            .unwrap_or("text/html")
            .trim()
            .to_string();

        info!("Pipeline: received response, content-type = '{mime_type}'");

        // Step 2: Parse into DOM
        let dom = self.parse(&response.body, &mime_type).await?;

        // Step 3: Extract sub-resources and resolve URLs
        let base_url = Url::parse(url).ok();
        let sub_resources = match &dom {
            TypedData::Dom(node) => extract_sub_resources(node, &base_url),
            _ => SubResources::default(),
        };

        // Step 4: Fetch external stylesheets (in parallel)
        let mut stylesheets = self.fetch_stylesheets_parallel(&sub_resources.stylesheets).await;
        info!(
            "Pipeline: fetched {}/{} external stylesheets",
            stylesheets.len(),
            sub_resources.stylesheets.len()
        );

        // Step 4b: Extract and fetch @import URLs from stylesheets
        let import_sheets = self.fetch_css_imports(&stylesheets, &base_url).await;
        if !import_sheets.is_empty() {
            info!("Pipeline: fetched {} @import stylesheets", import_sheets.len());
            // Prepend @import sheets so they're processed before the importing sheets.
            let mut all = import_sheets;
            all.extend(stylesheets);
            stylesheets = all;
        }

        // Step 4c: Extract @font-face URLs from stylesheets
        let font_face_urls = self.extract_font_face_urls(&stylesheets, &base_url);
        if !font_face_urls.is_empty() {
            info!(
                "Pipeline: discovered {} @font-face URL(s):",
                font_face_urls.len()
            );
            for ff in &font_face_urls {
                info!(
                    "  font-family: \"{}\", url: {}",
                    ff.family, ff.url
                );
            }
        }

        // Step 4d: Fetch @font-face font files (in parallel)
        let custom_fonts = self.fetch_fonts_parallel(&font_face_urls).await;
        if !custom_fonts.is_empty() {
            info!(
                "Pipeline: fetched {}/{} @font-face font(s)",
                custom_fonts.len(),
                font_face_urls.len()
            );
        }

        // Step 5: Compute styles (with external stylesheets)
        let styles = self.compute_styles_with(&dom, stylesheets.clone(), &viewport).await?;

        // Step 6: Layout
        let layout_tree = self.layout(&styles, viewport).await?;

        // Step 6b: Extract background-image URLs from computed styles.
        let mut all_images = sub_resources.images.clone();
        if let TypedData::Dom(ref styled_dom) = styles {
            let bg_images = extract_background_image_urls(styled_dom, &base_url);
            if !bg_images.is_empty() {
                info!("Pipeline: found {} background-image URL(s)", bg_images.len());
                all_images.extend(bg_images);
            }
        }

        // Filter out favicon / tiny icon images (they don't contribute to page display).
        let total_before_filter = all_images.len();
        all_images.retain(|(src, _resolved)| !is_favicon_or_icon(src));
        if all_images.len() < total_before_filter {
            info!(
                "Pipeline: skipped {} favicon/icon image(s)",
                total_before_filter - all_images.len()
            );
        }

        // Limit the number of images to avoid blocking on image-heavy pages.
        if all_images.len() > MAX_IMAGES_TO_FETCH {
            info!(
                "Pipeline: capping images from {} to {} for faster display",
                all_images.len(),
                MAX_IMAGES_TO_FETCH
            );
            all_images.truncate(MAX_IMAGES_TO_FETCH);
        }

        // Step 7: Fetch and decode images (in parallel, with per-image timeout)
        let images = self.fetch_and_decode_images_parallel(&all_images).await;
        info!(
            "Pipeline: decoded {}/{} images",
            images.len(),
            all_images.len()
        );

        // Step 8: Paint (with decoded images)
        let mut render_commands = self.paint_with_images(&layout_tree, images.clone()).await?;

        // Step 8b: Inject fetched @font-face fonts into the render commands
        // so the renderer can load and use them.
        if !custom_fonts.is_empty() {
            if let TypedData::RenderCommands(ref mut cmds) = render_commands {
                cmds.fonts = custom_fonts.clone();
            }
        }

        // Step 9: Execute scripts
        self.execute_scripts(&sub_resources.scripts, &sub_resources.inline_scripts)
            .await;

        // Re-render after JS execution: JS may have modified the DOM (added/removed
        // elements, changed classes, modified styles). Re-run style computation,
        // layout, and painting to reflect JS changes.
        // For now, we only do one re-render pass. A full browser would have a
        // mutation observer and requestAnimationFrame loop.
        if !sub_resources.scripts.is_empty() || !sub_resources.inline_scripts.is_empty() {
            info!("Pipeline: re-rendering after JS execution");
            let styles2 = self.compute_styles_with(&dom, stylesheets, &viewport).await?;
            let layout2 = self.layout(&styles2, viewport).await?;
            let mut render2 = self.paint_with_images(&layout2, images).await?;
            if !custom_fonts.is_empty() {
                if let TypedData::RenderCommands(ref mut cmds) = render2 {
                    cmds.fonts = custom_fonts;
                }
            }
            render_commands = render2;
        }

        info!("Pipeline: navigation complete");
        Ok(render_commands)
    }

    /// Re-render a DOM tree without re-fetching.
    ///
    /// Runs style → layout → paint on the provided DOM, suitable for
    /// updating the display after JavaScript DOM mutations.
    pub async fn re_render(
        &self,
        dom: TypedData,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        info!("Pipeline: re-rendering after DOM mutation");
        let styles = self.compute_styles_with(&dom, vec![], &viewport).await?;
        let layout = self.layout(&styles, viewport).await?;
        let render = self.paint_with_images(&layout, vec![]).await?;
        info!("Pipeline: re-render complete");
        Ok(render)
    }

    /// Fetch a URL and parse it, returning the DOM tree without running
    /// the rest of the pipeline (styles, layout, paint).
    pub async fn fetch_and_parse(&self, url: &str) -> Result<TypedData, NovaError> {
        info!("Pipeline: fetch+parse only for '{url}'");

        let response = self.fetch(url).await?;
        let mime_type = response
            .content_type()
            .unwrap_or("text/html")
            .split(';')
            .next()
            .unwrap_or("text/html")
            .trim()
            .to_string();

        self.parse(&response.body, &mime_type).await
    }

    /// Step 1: Fetch a URL via the network mod.
    async fn fetch(&self, url: &str) -> Result<HttpResponse, NovaError> {
        let protocol = url
            .split("://")
            .next()
            .unwrap_or("https")
            .to_string();

        let cap = CapabilityType::FetchUrl(protocol);
        let request = ContentRequest::Fetch {
            url: url.to_string(),
            headers: vec![],
        };

        let result = self.registry.route(&cap, request).await?;
        match result {
            TypedData::HttpResponse(resp) => Ok(resp),
            other => Err(NovaError::Internal(format!(
                "expected HttpResponse, got {other:?}"
            ))),
        }
    }

    /// Step 2: Parse the fetched content into a DOM tree.
    async fn parse(&self, data: &Bytes, mime_type: &str) -> Result<TypedData, NovaError> {
        let cap = CapabilityType::ParseDocument(mime_type.to_string());
        let request = ContentRequest::Parse {
            data: data.clone(),
            mime_type: mime_type.to_string(),
        };

        self.registry.route(&cap, request).await
    }

    /// Compute styles with external stylesheets.
    async fn compute_styles_with(
        &self,
        dom: &TypedData,
        stylesheets: Vec<TypedData>,
        viewport: &Viewport,
    ) -> Result<TypedData, NovaError> {
        let cap = CapabilityType::ComputeStyles;
        let request = ContentRequest::ComputeStyles {
            dom: Box::new(dom.clone()),
            stylesheets,
            viewport_width: viewport.width,
        };

        self.registry.route(&cap, request).await
    }

    /// Step 4: Perform layout.
    async fn layout(
        &self,
        styled_dom: &TypedData,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        let cap = CapabilityType::Layout;
        let request = ContentRequest::Layout {
            styled_dom: Box::new(styled_dom.clone()),
            viewport,
        };

        self.registry.route(&cap, request).await
    }

    /// Paint the layout tree with decoded image data.
    async fn paint_with_images(
        &self,
        layout_tree: &TypedData,
        images: Vec<(String, Vec<u8>)>,
    ) -> Result<TypedData, NovaError> {
        let cap = CapabilityType::Paint;
        let request = ContentRequest::Paint {
            layout_tree: Box::new(layout_tree.clone()),
            images,
        };

        self.registry.route(&cap, request).await
    }

    /// Fetch external stylesheets in parallel.
    async fn fetch_stylesheets_parallel(&self, urls: &[String]) -> Vec<TypedData> {
        if urls.is_empty() {
            return vec![];
        }

        let futures: Vec<_> = urls.iter().map(|url| self.fetch_stylesheet(url)).collect();
        let results = futures::future::join_all(futures).await;

        results.into_iter().flatten().collect()
    }

    /// Fetch a single stylesheet.
    async fn fetch_stylesheet(&self, url: &str) -> Option<TypedData> {
        match self.fetch(url).await {
            Ok(response) => {
                let css = String::from_utf8_lossy(&response.body).to_string();
                debug!(url = %url, len = css.len(), "fetched external stylesheet");
                Some(TypedData::Text(css))
            }
            Err(e) => {
                warn!(url = %url, error = %e, "failed to fetch stylesheet, skipping");
                None
            }
        }
    }

    /// Extract `@import` URLs from fetched stylesheets and fetch them.
    async fn fetch_css_imports(
        &self,
        stylesheets: &[TypedData],
        base_url: &Option<Url>,
    ) -> Vec<TypedData> {
        let mut import_urls = Vec::new();

        for sheet in stylesheets {
            if let TypedData::Text(css) = sheet {
                for line in css.lines() {
                    let trimmed = line.trim();
                    if let Some(url) = parse_css_import(trimmed) {
                        let resolved = resolve_url(&url, base_url);
                        import_urls.push(resolved);
                    }
                }
            }
        }

        if import_urls.is_empty() {
            return vec![];
        }

        self.fetch_stylesheets_parallel(&import_urls).await
    }

    /// Extract `@font-face` URLs from fetched stylesheets.
    ///
    /// Scans each stylesheet for `@font-face` rules using the CSS engine's
    /// parser and resolves relative URLs against the page's base URL.
    /// Returns a list of `FontFaceUrl` entries (family + resolved URL).
    fn extract_font_face_urls(
        &self,
        stylesheets: &[TypedData],
        base_url: &Option<Url>,
    ) -> Vec<FontFaceUrl> {
        let mut result = Vec::new();
        for sheet in stylesheets {
            if let TypedData::Text(css) = sheet {
                let entries = mod_css_engine::extract_font_face_urls(css, 1280.0);
                for mut entry in entries {
                    entry.url = resolve_url(&entry.url, base_url);
                    result.push(entry);
                }
            }
        }
        result
    }

    /// Fetch `@font-face` font files in parallel.
    ///
    /// Supports `.ttf`, `.otf`, `.woff`, and `.woff2` fonts. WOFF/WOFF2 files
    /// are decompressed to TTF after fetching so that fontdue can parse them.
    ///
    /// Returns `(family_name, font_bytes)` pairs for successfully fetched fonts.
    async fn fetch_fonts_parallel(&self, font_urls: &[FontFaceUrl]) -> Vec<(String, Vec<u8>)> {
        if font_urls.is_empty() {
            return vec![];
        }

        let futures: Vec<_> = font_urls
            .iter()
            .map(|ff| self.fetch_font(&ff.family, &ff.url))
            .collect();
        let results = futures::future::join_all(futures).await;

        results.into_iter().flatten().collect()
    }

    /// Fetch a single font file, returning `None` on failure or unsupported format.
    ///
    /// Supports `.ttf`, `.otf`, `.woff`, and `.woff2`. WOFF/WOFF2 files are
    /// decompressed to raw TTF/OTF bytes after fetching.
    async fn fetch_font(&self, family: &str, url: &str) -> Option<(String, Vec<u8>)> {
        // Determine the format from the URL extension.
        let path = url.split('?').next().unwrap_or(url);
        let ext = path.rsplit('.').next().unwrap_or("").to_lowercase();

        match ext.as_str() {
            "ttf" | "otf" | "woff" | "woff2" => {}
            _ => {
                warn!(
                    family = %family,
                    url = %url,
                    ext = %ext,
                    "skipping @font-face font: unrecognized font format"
                );
                return None;
            }
        }

        let response = match self.fetch(url).await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    family = %family,
                    url = %url,
                    error = %e,
                    "failed to fetch @font-face font, skipping"
                );
                return None;
            }
        };

        let raw_bytes = response.body.to_vec();
        info!(
            family = %family,
            url = %url,
            size = raw_bytes.len(),
            format = %ext,
            "fetched @font-face font"
        );

        // Decompress WOFF/WOFF2 to raw TTF/OTF bytes.
        let font_bytes = match ext.as_str() {
            "woff2" => {
                match woff2_patched::decode::convert_woff2_to_ttf(
                    &mut Cursor::new(&raw_bytes),
                ) {
                    Ok(ttf) => {
                        info!(
                            family = %family,
                            original_size = raw_bytes.len(),
                            decoded_size = ttf.len(),
                            "decoded WOFF2 → TTF"
                        );
                        ttf
                    }
                    Err(e) => {
                        warn!(
                            family = %family,
                            url = %url,
                            error = ?e,
                            "failed to decode WOFF2 font, skipping"
                        );
                        return None;
                    }
                }
            }
            "woff" => {
                match woff::version1::decompress(&raw_bytes) {
                    Some(ttf) => {
                        info!(
                            family = %family,
                            original_size = raw_bytes.len(),
                            decoded_size = ttf.len(),
                            "decoded WOFF → TTF"
                        );
                        ttf
                    }
                    None => {
                        warn!(
                            family = %family,
                            url = %url,
                            "failed to decode WOFF font, skipping"
                        );
                        return None;
                    }
                }
            }
            // TTF/OTF — already in the right format.
            _ => raw_bytes,
        };

        Some((family.to_string(), font_bytes))
    }

    /// Fetch and decode images in parallel.
    /// Input: `(original_src, resolved_url)` pairs.
    /// Output: `(original_src, decoded_bytes)` pairs (keyed by original src for painter lookup).
    async fn fetch_and_decode_images_parallel(
        &self,
        entries: &[(String, String)],
    ) -> Vec<(String, Vec<u8>)> {
        if entries.is_empty() {
            return vec![];
        }

        let futures: Vec<_> = entries
            .iter()
            .map(|(original, resolved)| self.fetch_and_decode_image_safe(original, resolved))
            .collect();
        let results = futures::future::join_all(futures).await;

        results.into_iter().flatten().collect()
    }

    /// Fetch and decode a single image, returning None on failure or timeout.
    /// Returns `(original_src, decoded_bytes)`.
    /// Each image fetch is capped at [`IMAGE_FETCH_TIMEOUT`] to prevent slow
    /// images from blocking the entire page render.
    async fn fetch_and_decode_image_safe(
        &self,
        original_src: &str,
        resolved_url: &str,
    ) -> Option<(String, Vec<u8>)> {
        match tokio::time::timeout(
            IMAGE_FETCH_TIMEOUT,
            self.fetch_and_decode_image(resolved_url),
        )
        .await
        {
            Ok(Ok(decoded)) => Some((original_src.to_string(), decoded)),
            Ok(Err(e)) => {
                warn!(url = %resolved_url, error = %e, "failed to fetch/decode image, skipping");
                None
            }
            Err(_) => {
                warn!(url = %resolved_url, timeout_secs = IMAGE_FETCH_TIMEOUT.as_secs(),
                    "image fetch timed out, skipping");
                None
            }
        }
    }

    /// Fetch a single image URL and decode it via the image mod.
    async fn fetch_and_decode_image(&self, url: &str) -> Result<Vec<u8>, NovaError> {
        let response = self.fetch(url).await?;

        // Detect format from Content-Type, URL extension, or content sniffing.
        let format_hint = detect_image_format(&response, url);

        let cap = CapabilityType::DecodeImage(format_hint.clone());
        let request = ContentRequest::DecodeImage {
            data: response.body,
            format_hint: Some(format_hint),
        };

        let result = self.registry.route(&cap, request).await?;
        match result {
            TypedData::Bytes(b) => Ok(b.to_vec()),
            other => Err(NovaError::Internal(format!(
                "expected Bytes from image decoder, got {other:?}"
            ))),
        }
    }

    /// Execute external and inline scripts via the JS mod.
    async fn execute_scripts(&self, external_urls: &[String], inline_scripts: &[String]) {
        // Fetch external scripts in parallel.
        let mut scripts: Vec<String> = Vec::new();
        if !external_urls.is_empty() {
            let futures: Vec<_> = external_urls
                .iter()
                .map(|url| self.fetch_script(url))
                .collect();
            let results = futures::future::join_all(futures).await;
            scripts.extend(results.into_iter().flatten());
        }

        // Append inline scripts.
        scripts.extend(inline_scripts.iter().cloned());

        // Execute each script.
        for (i, source) in scripts.iter().enumerate() {
            if source.trim().is_empty() {
                continue;
            }
            debug!(script_index = i, len = source.len(), "executing script");
            let cap = CapabilityType::ExecJavaScript;
            let request = ContentRequest::ExecScript {
                source: source.clone(),
                context_id: None,
            };
            match self.registry.route(&cap, request).await {
                Ok(result) => {
                    debug!(script_index = i, result = ?result, "script executed");
                }
                Err(e) => {
                    warn!(script_index = i, error = %e, "script execution failed");
                }
            }
        }
    }

    /// Fetch a single external script, returning None on failure.
    async fn fetch_script(&self, url: &str) -> Option<String> {
        match self.fetch(url).await {
            Ok(response) => {
                let source = String::from_utf8_lossy(&response.body).to_string();
                debug!(url = %url, len = source.len(), "fetched external script");
                Some(source)
            }
            Err(e) => {
                warn!(url = %url, error = %e, "failed to fetch script, skipping");
                None
            }
        }
    }
}

/// Detect the image format from Content-Type header, URL extension, or content sniffing.
fn detect_image_format(response: &HttpResponse, url: &str) -> String {
    // Try Content-Type header first.
    if let Some(ct) = response.content_type() {
        let mime = ct.split(';').next().unwrap_or(ct).trim();
        match mime {
            "image/png" => return "png".into(),
            "image/jpeg" | "image/jpg" => return "jpeg".into(),
            "image/webp" => return "webp".into(),
            "image/gif" => return "gif".into(),
            "image/svg+xml" => return "svg".into(),
            _ => {}
        }
    }

    // Try URL extension.
    let path = url.split('?').next().unwrap_or(url);
    if let Some(ext) = path.rsplit('.').next() {
        match ext.to_lowercase().as_str() {
            "png" => return "png".into(),
            "jpg" | "jpeg" => return "jpeg".into(),
            "webp" => return "webp".into(),
            "gif" => return "gif".into(),
            "svg" => return "svg".into(),
            _ => {}
        }
    }

    // Try content sniffing on the response body.
    let body = &response.body;
    if body.len() >= 4 {
        if body.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
            return "png".into();
        }
        if body.starts_with(&[0xFF, 0xD8]) {
            return "jpeg".into();
        }
        if body.starts_with(b"GIF8") {
            return "gif".into();
        }
        if body.len() >= 12 && body.starts_with(b"RIFF") && &body[8..12] == b"WEBP" {
            return "webp".into();
        }
        // Check for SVG.
        let check_len = body.len().min(512);
        let prefix = String::from_utf8_lossy(&body[..check_len]).to_lowercase();
        if prefix.contains("<svg") || prefix.contains("<!doctype svg") {
            return "svg".into();
        }
    }

    // Default: let the decoder auto-detect.
    "auto".into()
}

/// Parse a CSS `@import` directive and return the URL.
///
/// Supports:
/// - `@import url("style.css");`
/// - `@import url(style.css);`
/// - `@import "style.css";`
fn parse_css_import(line: &str) -> Option<String> {
    let line = line.trim();
    if !line.starts_with("@import") {
        return None;
    }
    let rest = line.strip_prefix("@import")?.trim();

    // @import url("...") or @import url(...)
    if let Some(inner) = rest.strip_prefix("url(") {
        let inner = inner.trim_start_matches(|c: char| c == '"' || c == '\'');
        let end = inner.find(|c: char| c == '"' || c == '\'' || c == ')');
        return end.map(|i| inner[..i].to_string());
    }

    // @import "..." or @import '...'
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &rest[1..];
        let end = inner.find(quote);
        return end.map(|i| inner[..i].to_string());
    }

    None
}

/// Walk a DOM tree and extract sub-resource URLs (stylesheets, images, scripts).
///
/// Relative URLs are resolved against `base_url` when available.
fn extract_sub_resources(node: &DomNode, base_url: &Option<Url>) -> SubResources {
    let mut resources = SubResources::default();
    walk_dom_for_resources(node, base_url, &mut resources);
    resources
}

/// Recursive DOM walker that collects sub-resource references.
fn walk_dom_for_resources(node: &DomNode, base_url: &Option<Url>, resources: &mut SubResources) {
    match node {
        DomNode::Element {
            tag,
            attributes,
            children,
        } => {
            let tag_lower = tag.to_lowercase();
            match tag_lower.as_str() {
                "link" => {
                    // <link rel="stylesheet" href="...">
                    let rel = attributes
                        .iter()
                        .find(|(k, _)| k == "rel")
                        .map(|(_, v)| v.to_lowercase());
                    if rel.as_deref() == Some("stylesheet") {
                        if let Some(href) = attributes.iter().find(|(k, _)| k == "href") {
                            let resolved = resolve_url(&href.1, base_url);
                            resources.stylesheets.push(resolved);
                        }
                    }
                }
                "img" => {
                    // <img src="..."> or <img srcset="...">
                    // Prefer srcset if available, fall back to src.
                    if let Some(srcset) = attributes.iter().find(|(k, _)| k == "srcset") {
                        if let Some(best) = pick_srcset_url(&srcset.1) {
                            let resolved = resolve_url(&best, base_url);
                            resources.images.push((best, resolved));
                        }
                    } else if let Some(src) = attributes.iter().find(|(k, _)| k == "src") {
                        if !src.1.is_empty() {
                            let resolved = resolve_url(&src.1, base_url);
                            resources.images.push((src.1.clone(), resolved));
                        }
                    }
                }
                "picture" => {
                    // <picture> contains <source> and <img> children.
                    // We extract <source srcset="..."> and the fallback <img>.
                    for child in children {
                        if let DomNode::Element {
                            tag: child_tag,
                            attributes: child_attrs,
                            ..
                        } = child
                        {
                            if child_tag == "source" {
                                if let Some(srcset) =
                                    child_attrs.iter().find(|(k, _)| k == "srcset")
                                {
                                    if let Some(best) = pick_srcset_url(&srcset.1) {
                                        let resolved = resolve_url(&best, base_url);
                                        resources.images.push((best, resolved));
                                    }
                                }
                            }
                        }
                    }
                    // Continue to recurse into children to get the <img> fallback.
                }
                "script" => {
                    // External: <script src="...">
                    if let Some(src) = attributes.iter().find(|(k, _)| k == "src") {
                        if !src.1.is_empty() {
                            let resolved = resolve_url(&src.1, base_url);
                            resources.scripts.push(resolved);
                        }
                    } else {
                        // Inline: <script>code here</script>
                        let mut inline_text = String::new();
                        collect_text_content(children, &mut inline_text);
                        if !inline_text.trim().is_empty() {
                            resources.inline_scripts.push(inline_text);
                        }
                    }
                }
                _ => {}
            }

            // Recurse into children.
            for child in children {
                walk_dom_for_resources(child, base_url, resources);
            }
        }
        DomNode::Document { children } => {
            for child in children {
                walk_dom_for_resources(child, base_url, resources);
            }
        }
        _ => {}
    }
}

/// Pick the best URL from a `srcset` attribute value.
///
/// Parses entries like `"image-1x.png 1x, image-2x.png 2x"` and picks
/// the first one (simplest heuristic — a real browser would pick based on DPR).
fn pick_srcset_url(srcset: &str) -> Option<String> {
    srcset
        .split(',')
        .next()
        .and_then(|entry| {
            let parts: Vec<&str> = entry.trim().split_whitespace().collect();
            parts.first().map(|url| url.to_string())
        })
        .filter(|url| !url.is_empty())
}

/// Collect text content from DOM children (for inline `<script>` elements).
fn collect_text_content(children: &[DomNode], buf: &mut String) {
    for child in children {
        match child {
            DomNode::Text(text) => buf.push_str(text),
            DomNode::Element { children, .. } => collect_text_content(children, buf),
            _ => {}
        }
    }
}

/// Resolve a potentially relative URL against a base URL.
fn resolve_url(href: &str, base_url: &Option<Url>) -> String {
    if let Some(base) = base_url {
        match base.join(href) {
            Ok(resolved) => resolved.to_string(),
            Err(_) => href.to_string(),
        }
    } else {
        href.to_string()
    }
}

/// Extract background-image URLs from a styled DOM tree.
///
/// Walks all elements with `data-nova-style` and looks for `background-image: url(...)`.
fn extract_background_image_urls(node: &DomNode, base_url: &Option<Url>) -> Vec<(String, String)> {
    let mut result = Vec::new();
    walk_for_bg_images(node, base_url, &mut result);
    result
}

fn walk_for_bg_images(node: &DomNode, base_url: &Option<Url>, out: &mut Vec<(String, String)>) {
    match node {
        DomNode::Element { attributes, children, .. } => {
            if let Some(style) = attributes.iter().find(|(k, _)| k == "data-nova-style") {
                for decl in style.1.split(';') {
                    let parts: Vec<&str> = decl.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let prop = parts[0].trim();
                        let val = parts[1].trim();
                        if (prop == "background-image" || prop == "background") && val.contains("url(") {
                            if let Some(url) = extract_css_url_from_value(val) {
                                let resolved = resolve_url(&url, base_url);
                                out.push((url, resolved));
                            }
                        }
                    }
                }
            }
            for child in children {
                walk_for_bg_images(child, base_url, out);
            }
        }
        DomNode::Document { children } => {
            for child in children {
                walk_for_bg_images(child, base_url, out);
            }
        }
        _ => {}
    }
}

/// Extract a URL from a CSS `url(...)` value string.
fn extract_css_url_from_value(value: &str) -> Option<String> {
    let idx = value.find("url(")?;
    let after = &value[idx + 4..];
    let trimmed = after.trim_start();
    let url_str = if trimmed.starts_with('"') {
        let inner = &trimmed[1..];
        &inner[..inner.find('"')?]
    } else if trimmed.starts_with('\'') {
        let inner = &trimmed[1..];
        &inner[..inner.find('\'')?]
    } else {
        &trimmed[..trimmed.find(')')?]
    };
    let url_str = url_str.trim();
    if url_str.is_empty() { None } else { Some(url_str.to_string()) }
}

/// Check if an image source looks like a favicon or small icon that does not
/// contribute to visible page content.
///
/// Matches paths like `/favicon.ico`, `/favicon-32x32.png`,
/// `apple-touch-icon.png`, etc.
fn is_favicon_or_icon(src: &str) -> bool {
    // Normalise to just the path/filename for matching.
    let path = src.split('?').next().unwrap_or(src);
    let filename = path.rsplit('/').next().unwrap_or(path).to_lowercase();

    // Exact or prefix matches.
    if filename == "favicon.ico" || filename.starts_with("favicon") {
        return true;
    }
    if filename.contains("apple-touch-icon") {
        return true;
    }
    // Common patterns: mstile-*, browserconfig-*, site.webmanifest icons
    if filename.starts_with("mstile-") || filename.starts_with("browserconfig") {
        return true;
    }
    // Data URIs for tiny 1x1 tracking pixels.
    if src.starts_with("data:image/gif;base64,R0lGOD") && src.len() < 120 {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_stylesheet_links() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "html".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "head".into(),
                    attributes: vec![],
                    children: vec![DomNode::Element {
                        tag: "link".into(),
                        attributes: vec![
                            ("rel".into(), "stylesheet".into()),
                            ("href".into(), "style.css".into()),
                        ],
                        children: vec![],
                    }],
                }],
            }],
        };
        let base = Url::parse("https://example.com/page").ok();
        let res = extract_sub_resources(&dom, &base);
        assert_eq!(res.stylesheets.len(), 1);
        assert_eq!(res.stylesheets[0], "https://example.com/style.css");
    }

    #[test]
    fn extract_images() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "img".into(),
                    attributes: vec![("src".into(), "/logo.png".into())],
                    children: vec![],
                }],
            }],
        };
        let base = Url::parse("https://example.com/page").ok();
        let res = extract_sub_resources(&dom, &base);
        assert_eq!(res.images.len(), 1);
        assert_eq!(res.images[0].0, "/logo.png"); // original src
        assert_eq!(res.images[0].1, "https://example.com/logo.png"); // resolved
    }

    #[test]
    fn extract_scripts() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "script".into(),
                        attributes: vec![("src".into(), "app.js".into())],
                        children: vec![],
                    },
                    DomNode::Element {
                        tag: "script".into(),
                        attributes: vec![],
                        children: vec![DomNode::Text("console.log('hi')".into())],
                    },
                ],
            }],
        };
        let base = Url::parse("https://example.com/").ok();
        let res = extract_sub_resources(&dom, &base);
        assert_eq!(res.scripts.len(), 1);
        assert_eq!(res.scripts[0], "https://example.com/app.js");
        assert_eq!(res.inline_scripts.len(), 1);
        assert_eq!(res.inline_scripts[0], "console.log('hi')");
    }

    #[test]
    fn resolve_absolute_url_unchanged() {
        let base = Url::parse("https://example.com/").ok();
        let result = resolve_url("https://cdn.example.com/style.css", &base);
        assert_eq!(result, "https://cdn.example.com/style.css");
    }

    #[test]
    fn resolve_relative_url() {
        let base = Url::parse("https://example.com/pages/about").ok();
        let result = resolve_url("../style.css", &base);
        assert_eq!(result, "https://example.com/style.css");
    }

    #[test]
    fn resolve_without_base() {
        let result = resolve_url("style.css", &None);
        assert_eq!(result, "style.css");
    }

    #[test]
    fn empty_img_src_ignored() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "img".into(),
                attributes: vec![("src".into(), "".into())],
                children: vec![],
            }],
        };
        let res = extract_sub_resources(&dom, &None);
        assert!(res.images.is_empty());
    }

    #[test]
    fn parse_import_url_quoted() {
        assert_eq!(
            parse_css_import("@import \"reset.css\";"),
            Some("reset.css".into())
        );
    }

    #[test]
    fn parse_import_url_function() {
        assert_eq!(
            parse_css_import("@import url(\"styles/main.css\");"),
            Some("styles/main.css".into())
        );
    }

    #[test]
    fn parse_import_url_unquoted() {
        assert_eq!(
            parse_css_import("@import url(base.css);"),
            Some("base.css".into())
        );
    }

    #[test]
    fn parse_import_not_import() {
        assert_eq!(parse_css_import("body { color: red; }"), None);
    }

    #[test]
    fn pick_srcset_first() {
        assert_eq!(
            pick_srcset_url("small.jpg 1x, large.jpg 2x"),
            Some("small.jpg".into())
        );
    }

    #[test]
    fn pick_srcset_single() {
        assert_eq!(
            pick_srcset_url("image.webp 480w"),
            Some("image.webp".into())
        );
    }

    #[test]
    fn extract_picture_source() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "picture".into(),
                attributes: vec![],
                children: vec![
                    DomNode::Element {
                        tag: "source".into(),
                        attributes: vec![("srcset".into(), "photo.webp".into())],
                        children: vec![],
                    },
                    DomNode::Element {
                        tag: "img".into(),
                        attributes: vec![("src".into(), "photo.jpg".into())],
                        children: vec![],
                    },
                ],
            }],
        };
        let res = extract_sub_resources(&dom, &None);
        // Should extract both the <source srcset> and the <img src>.
        assert_eq!(res.images.len(), 2);
        assert!(res.images.iter().any(|(orig, _)| orig == "photo.webp"));
        assert!(res.images.iter().any(|(orig, _)| orig == "photo.jpg"));
    }

    #[test]
    fn extract_img_srcset() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "img".into(),
                attributes: vec![
                    ("src".into(), "fallback.jpg".into()),
                    ("srcset".into(), "better.webp 2x".into()),
                ],
                children: vec![],
            }],
        };
        let res = extract_sub_resources(&dom, &None);
        // srcset takes precedence over src.
        assert_eq!(res.images.len(), 1);
        assert_eq!(res.images[0].0, "better.webp");
    }

    #[test]
    fn favicon_detection() {
        assert!(is_favicon_or_icon("/favicon.ico"));
        assert!(is_favicon_or_icon("https://example.com/favicon.ico"));
        assert!(is_favicon_or_icon("/favicon-32x32.png"));
        assert!(is_favicon_or_icon("/apple-touch-icon.png"));
        assert!(is_favicon_or_icon("/icons/apple-touch-icon-180x180.png"));
        assert!(is_favicon_or_icon("mstile-150x150.png"));
        // Normal images should not be detected as favicons.
        assert!(!is_favicon_or_icon("/images/logo.png"));
        assert!(!is_favicon_or_icon("https://cdn.example.com/photo.jpg"));
        assert!(!is_favicon_or_icon("/hero-banner.webp"));
    }

    #[test]
    fn image_cap_constant() {
        // Sanity check: the cap should be a reasonable number.
        assert!(MAX_IMAGES_TO_FETCH > 0);
        assert!(MAX_IMAGES_TO_FETCH <= 50);
    }
}
