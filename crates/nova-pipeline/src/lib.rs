//! # nova-pipeline
//!
//! The Pipeline Engine — the brain of the core.
//! Orchestrates the full journey from URL to pixels on screen.
//!
//! The pipeline doesn't do any work itself. It sequences requests
//! to the capability registry, which routes them to the right mods.

use std::sync::Arc;

use bytes::Bytes;
use tracing::{debug, info, warn};
use url::Url;

use nova_mod_api::{
    CapabilityType, ContentRequest, NovaError, TypedData, Viewport,
    content::{DomNode, HttpResponse},
};
use nova_registry::CapabilityRegistry;

/// Sub-resources extracted from a parsed DOM tree.
#[derive(Debug, Default)]
struct SubResources {
    /// External stylesheet URLs.
    stylesheets: Vec<String>,
    /// Image URLs (from `<img src="...">`).
    images: Vec<String>,
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

        // Step 4: Fetch external stylesheets
        let stylesheets = self.fetch_stylesheets(&sub_resources.stylesheets).await;
        info!(
            "Pipeline: fetched {}/{} external stylesheets",
            stylesheets.len(),
            sub_resources.stylesheets.len()
        );

        // Step 5: Compute styles (with external stylesheets)
        let styles = self.compute_styles_with(&dom, stylesheets).await?;

        // Step 6: Layout
        let layout_tree = self.layout(&styles, viewport).await?;

        // Step 7: Fetch and decode images
        let images = self.fetch_and_decode_images(&sub_resources.images).await;
        info!(
            "Pipeline: decoded {}/{} images",
            images.len(),
            sub_resources.images.len()
        );

        // Step 8: Paint (with decoded images)
        let render_commands = self.paint_with_images(&layout_tree, images).await?;

        // Step 9: Execute scripts (after paint, so scripts don't block rendering)
        self.execute_scripts(&sub_resources.scripts, &sub_resources.inline_scripts)
            .await;

        info!("Pipeline: navigation complete");
        Ok(render_commands)
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
    ) -> Result<TypedData, NovaError> {
        let cap = CapabilityType::ComputeStyles;
        let request = ContentRequest::ComputeStyles {
            dom: Box::new(dom.clone()),
            stylesheets,
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

    /// Fetch external stylesheets, returning successfully fetched ones as `TypedData::Text`.
    async fn fetch_stylesheets(&self, urls: &[String]) -> Vec<TypedData> {
        let mut stylesheets = Vec::new();
        for url in urls {
            match self.fetch(url).await {
                Ok(response) => {
                    let css = String::from_utf8_lossy(&response.body).to_string();
                    debug!(url = %url, len = css.len(), "fetched external stylesheet");
                    stylesheets.push(TypedData::Text(css));
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "failed to fetch stylesheet, skipping");
                }
            }
        }
        stylesheets
    }

    /// Fetch and decode images, returning `(src_url, decoded_rgba_bytes)` pairs.
    async fn fetch_and_decode_images(&self, urls: &[String]) -> Vec<(String, Vec<u8>)> {
        let mut images = Vec::new();
        for url in urls {
            match self.fetch_and_decode_image(url).await {
                Ok(decoded) => {
                    images.push((url.clone(), decoded));
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "failed to fetch/decode image, skipping");
                }
            }
        }
        images
    }

    /// Fetch a single image URL and decode it via the image mod.
    async fn fetch_and_decode_image(&self, url: &str) -> Result<Vec<u8>, NovaError> {
        let response = self.fetch(url).await?;

        // Detect format from Content-Type or URL extension.
        let format_hint = response
            .content_type()
            .and_then(|ct| {
                let mime = ct.split(';').next().unwrap_or(ct).trim();
                match mime {
                    "image/png" => Some("png"),
                    "image/jpeg" | "image/jpg" => Some("jpeg"),
                    "image/webp" => Some("webp"),
                    "image/gif" => Some("gif"),
                    _ => None,
                }
            })
            .or_else(|| {
                // Fall back to URL extension.
                let path = url.rsplit('?').last().unwrap_or(url);
                let ext = path.rsplit('.').next().unwrap_or("");
                match ext.to_lowercase().as_str() {
                    "png" => Some("png"),
                    "jpg" | "jpeg" => Some("jpeg"),
                    "webp" => Some("webp"),
                    "gif" => Some("gif"),
                    _ => None,
                }
            })
            .unwrap_or("png")
            .to_string();

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
        // Fetch external scripts first.
        let mut scripts: Vec<String> = Vec::new();
        for url in external_urls {
            match self.fetch(url).await {
                Ok(response) => {
                    let source = String::from_utf8_lossy(&response.body).to_string();
                    debug!(url = %url, len = source.len(), "fetched external script");
                    scripts.push(source);
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "failed to fetch script, skipping");
                }
            }
        }

        // Append inline scripts (they execute in document order after externals
        // for simplicity — a real browser interleaves them).
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
                    // <img src="...">
                    if let Some(src) = attributes.iter().find(|(k, _)| k == "src") {
                        if !src.1.is_empty() {
                            let resolved = resolve_url(&src.1, base_url);
                            resources.images.push(resolved);
                        }
                    }
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
        assert_eq!(res.images[0], "https://example.com/logo.png");
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
}
