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
const MAX_IMAGES_TO_FETCH: usize = 50;

/// Timeout for fetching + decoding a single image.
/// If an image takes longer than this, it is skipped.
const IMAGE_FETCH_TIMEOUT: Duration = Duration::from_secs(2);

/// Timeout for fetching a single @font-face font file.
/// Fonts that take longer than this are skipped — the renderer will
/// fall back to system fonts.
const FONT_FETCH_TIMEOUT: Duration = Duration::from_secs(1);

/// Maximum number of external scripts to fetch and execute.
/// Pages with many external scripts are capped to avoid long JS execution
/// blocking the initial render.
const MAX_EXTERNAL_SCRIPTS: usize = 5;

/// Result of executing scripts with a DOM tree.
///
/// Contains the (potentially mutated) DOM plus any SPA URL changes
/// signalled via `history.pushState()` or `history.replaceState()`.
struct ScriptExecResult {
    /// The DOM tree after script execution.
    dom: DomNode,
    /// URL set by `history.pushState()` — update URL bar without navigation.
    push_state_url: Option<String>,
    /// URL set by `history.replaceState()` — replace current history entry.
    replace_state_url: Option<String>,
}

/// Extract a redirect URL from `<noscript><meta http-equiv="refresh" content="...">`.
///
/// Many sites (Google, etc.) include a `<noscript>` block with a meta refresh
/// that redirects to a simpler, JS-free version of the page. Since NOVA's JS
/// engine is limited, following this redirect gives a much better rendering.
fn extract_noscript_redirect(node: &DomNode, base_url: &Option<Url>) -> Option<String> {
    match node {
        DomNode::Element { tag, children, .. } => {
            if tag == "noscript" {
                // Look for <meta http-equiv="refresh"> inside noscript.
                for child in children {
                    if let DomNode::Element { tag: t, attributes, .. } = child {
                        if t == "meta" {
                            let is_refresh = attributes.iter().any(|(k, v)| {
                                k.eq_ignore_ascii_case("http-equiv")
                                    && v.eq_ignore_ascii_case("refresh")
                            });
                            if is_refresh {
                                if let Some((_, content)) = attributes.iter().find(|(k, _)| k.eq_ignore_ascii_case("content")) {
                                    // Parse "0;url=..." or "0; url=..."
                                    if let Some(url_part) = content.split("url=").nth(1).or_else(|| content.split("URL=").nth(1)) {
                                        let raw_url = url_part.trim().trim_matches('"').trim_matches('\'');
                                        // Resolve relative URL.
                                        let resolved = if let Some(base) = base_url {
                                            base.join(raw_url).map(|u| u.to_string()).unwrap_or_else(|_| raw_url.to_string())
                                        } else {
                                            raw_url.to_string()
                                        };
                                        return Some(resolved);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // Recurse into children.
            for child in children {
                if let Some(url) = extract_noscript_redirect(child, base_url) {
                    return Some(url);
                }
            }
            None
        }
        DomNode::Document { children } => {
            for child in children {
                if let Some(url) = extract_noscript_redirect(child, base_url) {
                    return Some(url);
                }
            }
            None
        }
        _ => None,
    }
}

/// Check if a URL is a Google homepage (www.google.com, google.com, etc.).
fn is_google_homepage(url: &str) -> bool {
    if let Ok(parsed) = Url::parse(url) {
        if let Some(host) = parsed.host_str() {
            let is_google = host == "www.google.com"
                || host == "google.com"
                || host.ends_with(".google.com");
            let path = parsed.path();
            // Only inject on homepage-like paths, not on /search results.
            let is_homepage = path == "/" || path == "/webhp" || path.is_empty();
            return is_google && is_homepage;
        }
    }
    false
}

/// Inject a visible Google search form into the DOM.
///
/// Always injects the form regardless of whether Google's own `<input name="q">`
/// exists, because Google's version is typically hidden inside `<noscript>` or a
/// `display:none` container.  The injected form uses explicit inline styles so
/// it is always visible.
fn inject_google_search_form(node: &mut DomNode) {
    // Find <body> and append the form.
    match node {
        DomNode::Element { tag, children, .. } => {
            if tag == "body" {
                // Build the search form as DOM nodes with rounded, centered styling.
                let form = DomNode::Element {
                    tag: "form".into(),
                    attributes: vec![
                        ("action".into(), "/search".into()),
                        ("method".into(), "GET".into()),
                    ],
                    children: vec![
                        DomNode::Element {
                            tag: "div".into(),
                            attributes: vec![
                                ("style".into(), "display:block;text-align:center;margin:20px auto;max-width:584px".into()),
                            ],
                            children: vec![
                                DomNode::Element {
                                    tag: "input".into(),
                                    attributes: vec![
                                        ("name".into(), "q".into()),
                                        ("type".into(), "text".into()),
                                        ("style".into(), "width:100%;padding:12px 20px;font-size:16px;border:1px solid #dfe1e5;border-radius:24px;box-sizing:border-box".into()),
                                        ("autocomplete".into(), "off".into()),
                                        ("title".into(), "Search".into()),
                                    ],
                                    children: vec![],
                                },
                                DomNode::Element {
                                    tag: "br".into(),
                                    attributes: vec![],
                                    children: vec![],
                                },
                                DomNode::Element {
                                    tag: "input".into(),
                                    attributes: vec![
                                        ("type".into(), "submit".into()),
                                        ("value".into(), "Google Search".into()),
                                        ("style".into(), "margin-top:12px;padding:8px 20px;background-color:#f8f9fa;border:1px solid #f8f9fa;border-radius:4px;font-size:14px;color:#3c4043;margin-right:8px".into()),
                                    ],
                                    children: vec![],
                                },
                                DomNode::Element {
                                    tag: "input".into(),
                                    attributes: vec![
                                        ("type".into(), "submit".into()),
                                        ("value".into(), "I'm Feeling Lucky".into()),
                                        ("name".into(), "btnI".into()),
                                        ("style".into(), "padding:8px 20px;background-color:#f8f9fa;border:1px solid #f8f9fa;border-radius:4px;font-size:14px;color:#3c4043".into()),
                                    ],
                                    children: vec![],
                                },
                            ],
                        },
                    ],
                };
                // Insert after the <center> element (which contains the logo)
                // so the search form appears right below the logo.
                let center_idx = children.iter().position(|c| {
                    matches!(c, DomNode::Element { tag, .. } if tag == "center")
                });
                if let Some(idx) = center_idx {
                    children.insert(idx + 1, form);
                } else {
                    // Fallback: insert after first few elements (skip header)
                    let insert_pos = children.len().min(2);
                    children.insert(insert_pos, form);
                }
                return;
            }
            for child in children.iter_mut() {
                inject_google_search_form(child);
            }
        }
        DomNode::Document { children } => {
            for child in children.iter_mut() {
                inject_google_search_form(child);
            }
        }
        _ => {}
    }
}

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

        // Handle about: URLs — these are internal pages that bypass the network.
        if url.starts_with("about:") {
            return self.navigate_about_url(url, viewport).await;
        }

        // Step 1: Fetch (with error page fallback)
        let response = match self.fetch(url).await {
            Ok(resp) => resp,
            Err(e) => {
                warn!("Pipeline: fetch failed for '{url}': {e}");
                return self.render_error_page(url, &e.to_string(), viewport).await;
            }
        };
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
        let mut dom = self.parse(&response.body, &mime_type).await?;

        // Step 2b: Check for <noscript> meta-refresh redirects.
        // Sites like Google serve a <noscript> block with a meta refresh to a
        // simpler HTML version when JS is limited.  We NO LONGER follow these
        // redirects because our JS engine is now capable enough to render the
        // normal page.  Instead, we un-hide <noscript> content as a fallback
        // (see step 5b below) if JS execution fails to produce interactive
        // elements like an <input>.
        if let TypedData::Dom(ref node) = dom {
            if let Some(redirect_url) = extract_noscript_redirect(node, &Url::parse(url).ok()) {
                info!("Pipeline: found noscript redirect to '{redirect_url}' (NOT following — trying JS rendering first)");
            }
        }

        // Step 2c: Google search form injection — disabled.
        // The native Google form now renders correctly with inline submit
        // buttons and proper table/form layout.

        // Step 3: Extract sub-resources and resolve URLs
        let base_url = Url::parse(url).ok();
        let sub_resources = match &dom {
            TypedData::Dom(node) => extract_sub_resources(node, &base_url),
            _ => SubResources::default(),
        };

        // Step 3b: Extract page title and favicon URL from the DOM.
        let (page_title, favicon_url) = match &dom {
            TypedData::Dom(node) => (
                extract_page_title(node),
                extract_favicon_url(node, &base_url),
            ),
            _ => (None, None),
        };
        if let Some(ref title) = page_title {
            info!("Pipeline: page title = '{title}'");
        }
        if let Some(ref fav) = favicon_url {
            info!("Pipeline: favicon URL = '{fav}'");
        }

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

        // Step 5: Execute scripts WITH DOM before style computation.
        // This lets JS mutations (createElement, appendChild, innerHTML, etc.)
        // be reflected in the initial render — no re-render pass needed.
        let mut spa_push_url: Option<String> = None;
        let mut spa_replace_url: Option<String> = None;
        let final_dom = if let TypedData::Dom(ref node) = dom {
            if let Some(result) = self.execute_scripts_with_dom(
                node,
                &sub_resources.scripts,
                &sub_resources.inline_scripts,
            ).await {
                info!("Pipeline: JS modified DOM, using mutated DOM for rendering");
                spa_push_url = result.push_state_url;
                spa_replace_url = result.replace_state_url;
                TypedData::Dom(result.dom)
            } else {
                dom.clone()
            }
        } else {
            dom.clone()
        };

        // Step 5b: Check if the DOM contains interactive elements.
        // If not, un-hide <noscript> content as a fallback.  Many sites
        // (including Google) wrap a working HTML form inside <noscript>;
        // if our JS engine failed to create the dynamic version, showing
        // the noscript content gives the user a usable page.
        let final_dom = if let TypedData::Dom(ref node) = final_dom {
            if !dom_has_interactive_element(node) {
                info!("Pipeline: DOM lacks interactive elements after JS — un-hiding <noscript> content");
                let mut unhidden = node.clone();
                unhide_noscript_content(&mut unhidden);
                TypedData::Dom(unhidden)
            } else {
                final_dom
            }
        } else {
            final_dom
        };

        // Step 6: Compute styles on the (possibly mutated) DOM
        let styles = self.compute_styles_with(&final_dom, stylesheets, &viewport).await?;

        // Step 7: Layout
        let layout_tree = self.layout(&styles, viewport).await?;

        // Step 7b: Extract background-image URLs from computed styles.
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

        // Step 8: Fetch and decode images (in parallel, with per-image timeout)
        let images = self.fetch_and_decode_images_parallel(&all_images).await;
        info!(
            "Pipeline: decoded {}/{} images",
            images.len(),
            all_images.len()
        );

        // Step 9: Paint (with decoded images)
        let mut render_commands = self.paint_with_images(&layout_tree, images).await?;

        // Step 9b: Inject fetched @font-face fonts into the render commands
        // so the renderer can load and use them.
        if !custom_fonts.is_empty() {
            if let TypedData::RenderCommands(ref mut cmds) = render_commands {
                cmds.fonts = custom_fonts;
            }
        }

        // Step 10: Inject SPA URL changes (from history.pushState/replaceState)
        // into the render commands so the shell can update the URL bar.
        if spa_push_url.is_some() || spa_replace_url.is_some() {
            if let TypedData::RenderCommands(ref mut cmds) = render_commands {
                cmds.spa_push_url = spa_push_url;
                cmds.spa_replace_url = spa_replace_url;
            }
        }

        // Step 10b: Inject page title and favicon URL.
        if let TypedData::RenderCommands(ref mut cmds) = render_commands {
            cmds.page_title = page_title;
            cmds.favicon_url = favicon_url;
        }

        info!("Pipeline: navigation complete");
        Ok(render_commands)
    }

    /// Handle `about:` URLs — internal browser pages.
    ///
    /// Returns rendered content for `about:blank`, `about:version`, and other
    /// internal pages without making any network requests.
    async fn navigate_about_url(
        &self,
        url: &str,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        let page_name = url.strip_prefix("about:").unwrap_or("");
        info!("Pipeline: handling about:{page_name}");

        let html = match page_name {
            "blank" | "" => {
                // about:blank — empty white page.
                String::new()
            }
            "version" => {
                // about:version — browser version info page.
                build_version_page_html()
            }
            other => {
                // Unknown about: page — show a simple message.
                format!(
                    r#"<html><head><title>about:{other}</title>
<style>body {{ font-family: sans-serif; padding: 40px; color: #333; }}
h1 {{ color: #666; }}</style></head>
<body><h1>about:{other}</h1><p>This page does not exist.</p></body></html>"#
                )
            }
        };

        if html.is_empty() {
            // about:blank — return minimal empty render commands.
            return Ok(TypedData::RenderCommands(
                nova_mod_api::RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None },
            ));
        }

        // Parse and render the HTML through the pipeline.
        self.render_html_string(&html, viewport).await
    }

    /// Render an error page when navigation fails.
    ///
    /// Shows a styled "This site can't be reached" page with the error
    /// details and the URL that failed.
    async fn render_error_page(
        &self,
        url: &str,
        error: &str,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        let (error_title, error_detail) = classify_error_str(error);
        let escaped_url = html_escape(url);
        let escaped_detail = html_escape(&error_detail);

        let html = format!(
            r#"<html><head><title>{error_title}</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  margin: 0; padding: 60px 40px; background: #f8f9fa; color: #202124; }}
.container {{ max-width: 600px; margin: 0 auto; }}
.icon {{ font-size: 48px; color: #9aa0a6; margin-bottom: 20px; }}
h1 {{ font-size: 20px; font-weight: normal; color: #202124; margin-bottom: 8px; }}
.url {{ font-size: 14px; color: #5f6368; word-break: break-all; margin-bottom: 20px; }}
.detail {{ font-size: 14px; color: #5f6368; line-height: 1.6; margin-bottom: 24px;
  padding: 16px; background: #fff; border: 1px solid #dadce0; border-radius: 8px; }}
.actions {{ margin-top: 24px; }}
.btn {{ display: inline-block; padding: 8px 24px; background: #1a73e8; color: white;
  border-radius: 4px; text-decoration: none; font-size: 14px; }}
</style></head>
<body><div class="container">
<div class="icon">:(</div>
<h1>{error_title}</h1>
<p class="url">{escaped_url}</p>
<div class="detail">{escaped_detail}</div>
<div class="actions"><a class="btn" href="{escaped_url}">Reload</a></div>
</div></body></html>"#
        );

        // Try to render the error page. If even that fails, return a minimal error.
        match self.render_html_string(&html, viewport).await {
            Ok(result) => Ok(result),
            Err(_) => {
                // Absolute fallback: empty render commands.
                Ok(TypedData::RenderCommands(
                    nova_mod_api::RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None },
                ))
            }
        }
    }

    /// Render an SSL/TLS certificate error page.
    ///
    /// Shows a "Your connection is not private" warning page.
    pub async fn render_ssl_error_page(
        &self,
        url: &str,
        error_msg: &str,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        let escaped_url = html_escape(url);
        let escaped_error = html_escape(error_msg);

        let html = format!(
            r#"<html><head><title>Privacy error</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  margin: 0; padding: 60px 40px; background: #fff; color: #202124; }}
.container {{ max-width: 600px; margin: 0 auto; }}
.shield {{ font-size: 64px; color: #ea4335; margin-bottom: 20px; text-align: center; }}
h1 {{ font-size: 22px; font-weight: normal; color: #ea4335; }}
.url {{ font-size: 14px; color: #5f6368; word-break: break-all; margin-bottom: 16px; }}
.warning {{ font-size: 14px; color: #202124; line-height: 1.6; padding: 16px;
  background: #fce8e6; border: 1px solid #f5c6cb; border-radius: 8px; margin-bottom: 16px; }}
.detail {{ font-size: 13px; color: #5f6368; line-height: 1.5; }}
</style></head>
<body><div class="container">
<div class="shield">!</div>
<h1>Your connection is not private</h1>
<p class="url">{escaped_url}</p>
<div class="warning">Attackers might be trying to steal your information from this site
(for example, passwords, messages, or credit cards).</div>
<p class="detail">Error: {escaped_error}</p>
</div></body></html>"#
        );

        match self.render_html_string(&html, viewport).await {
            Ok(result) => Ok(result),
            Err(_) => Ok(TypedData::RenderCommands(
                nova_mod_api::RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None },
            )),
        }
    }

    /// Render an HTML string through the parse → style → layout → paint pipeline.
    ///
    /// Useful for rendering internal pages (about:, error pages, view-source)
    /// without making any network requests.
    pub async fn render_html_string(
        &self,
        html: &str,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        let data = bytes::Bytes::from(html.to_string());
        let dom = self.parse(&data, "text/html").await?;
        let styles = self.compute_styles_with(&dom, vec![], &viewport).await?;
        let layout = self.layout(&styles, viewport).await?;
        self.paint_with_images(&layout, vec![]).await
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

    /// Navigate via POST request (for form submissions).
    pub async fn navigate_post(
        &self,
        url: &str,
        body: Vec<u8>,
        content_type: &str,
        viewport: Viewport,
    ) -> Result<TypedData, NovaError> {
        info!(url = %url, body_len = body.len(), "Pipeline: POST navigation");
        let request = ContentRequest::FetchWithBody {
            url: url.to_string(),
            method: "POST".to_string(),
            headers: vec![("content-type".to_string(), content_type.to_string())],
            body: Some(body.into()),
        };
        let response = match self.registry.route(&CapabilityType::FetchUrl("https".into()), request).await {
            Ok(TypedData::HttpResponse(r)) => r,
            Ok(other) => return Err(NovaError::Internal(format!("expected HttpResponse, got {other:?}"))),
            Err(e) => return Err(e),
        };
        let dom = self.parse(&response.body, "text/html").await?;
        let styles = self.compute_styles_with(&dom, vec![], &viewport).await?;
        let layout = self.layout(&styles, viewport).await?;
        self.paint_with_images(&layout, vec![]).await
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
            canvas_pixels: vec![],
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
            .map(|ff| {
                let family = ff.family.clone();
                let url = ff.url.clone();
                async move {
                    match tokio::time::timeout(
                        FONT_FETCH_TIMEOUT,
                        self.fetch_font(&family, &url),
                    ).await {
                        Ok(result) => result,
                        Err(_) => {
                            warn!(
                                family = %family,
                                url = %url,
                                timeout_secs = FONT_FETCH_TIMEOUT.as_secs(),
                                "font fetch timed out, skipping"
                            );
                            None
                        }
                    }
                }
            })
            .collect();
        let results = futures::future::join_all(futures).await;

        results.into_iter().flatten().collect()
    }

    /// Fetch a single font file, returning `None` on failure, unsupported format,
    /// or timeout.
    ///
    /// Supports `.ttf`, `.otf`, `.woff`, and `.woff2`. WOFF/WOFF2 files are
    /// decompressed to raw TTF/OTF bytes after fetching.
    ///
    /// Each font fetch is capped at [`FONT_FETCH_TIMEOUT`] to prevent slow
    /// fonts from blocking the entire page render.
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
    /// Execute scripts without DOM access (fire-and-forget).
    ///
    /// Kept as a fallback for cases where DOM mutation is not needed.
    /// The primary path is [`execute_scripts_with_dom`](Self::execute_scripts_with_dom).
    #[allow(dead_code)]
    async fn execute_scripts_fire_and_forget(&self, external_urls: &[String], inline_scripts: &[String]) {
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

    /// Execute scripts with a live DOM tree, returning the mutated DOM.
    ///
    /// Each script is executed in order within a shared JS context. The DOM
    /// returned by each script is fed into the next one so that mutations
    /// accumulate. Returns `None` when there are no scripts to run.
    async fn execute_scripts_with_dom(
        &self,
        dom: &DomNode,
        external_urls: &[String],
        inline_scripts: &[String],
    ) -> Option<ScriptExecResult> {
        if external_urls.is_empty() && inline_scripts.is_empty() {
            return None;
        }

        // Fetch external scripts in parallel, capped to MAX_EXTERNAL_SCRIPTS.
        let mut scripts: Vec<String> = Vec::new();
        if !external_urls.is_empty() {
            let capped_urls: Vec<_> = external_urls.iter().take(MAX_EXTERNAL_SCRIPTS).collect();
            if external_urls.len() > MAX_EXTERNAL_SCRIPTS {
                info!(
                    total = external_urls.len(),
                    fetched = MAX_EXTERNAL_SCRIPTS,
                    "capping external scripts for faster initial render"
                );
            }
            let futures: Vec<_> = capped_urls
                .iter()
                .map(|url| self.fetch_script(url))
                .collect();
            let results = futures::future::join_all(futures).await;
            scripts.extend(results.into_iter().flatten());
        }

        // Append inline scripts.
        scripts.extend(inline_scripts.iter().cloned());

        // Execute all scripts with the DOM, using a shared context_id.
        let mut current_dom = dom.clone();
        let context_id = 1u64;
        let mut last_push_state_url: Option<String> = None;
        let mut last_replace_state_url: Option<String> = None;

        for (i, source) in scripts.iter().enumerate() {
            if source.trim().is_empty() {
                continue;
            }
            debug!(script_index = i, len = source.len(), "executing script with DOM");
            let request = ContentRequest::ExecScriptWithDom {
                source: source.clone(),
                dom: Box::new(current_dom.clone()),
                context_id: Some(context_id),
            };
            match self
                .registry
                .route(&CapabilityType::ExecJavaScript, request)
                .await
            {
                Ok(TypedData::JsResultWithDom { dom, push_state_url, replace_state_url, .. }) => {
                    debug!(script_index = i, "script mutated DOM");
                    current_dom = *dom;
                    if push_state_url.is_some() {
                        last_push_state_url = push_state_url;
                    }
                    if replace_state_url.is_some() {
                        last_replace_state_url = replace_state_url;
                    }
                }
                Ok(TypedData::JsResult(_)) => {
                    debug!(script_index = i, "script did not return DOM, keeping current");
                }
                Ok(other) => {
                    debug!(script_index = i, result = ?other, "unexpected result type");
                }
                Err(e) => {
                    warn!(script_index = i, error = %e, "script execution failed");
                }
            }
        }

        Some(ScriptExecResult {
            dom: current_dom,
            push_state_url: last_push_state_url,
            replace_state_url: last_replace_state_url,
        })
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

/// Check if a DOM tree contains an interactive element (`<input>`, `<textarea>`,
/// `<select>`, or `<button>`).  Used to decide whether to un-hide `<noscript>`
/// content as a fallback when JS fails to create the expected dynamic UI.
fn dom_has_interactive_element(node: &DomNode) -> bool {
    match node {
        DomNode::Element { tag, children, .. } => {
            let t = tag.to_lowercase();
            if t == "input" || t == "textarea" || t == "select" || t == "button" {
                return true;
            }
            // Skip noscript children — those are the fallback content.
            if t == "noscript" {
                return false;
            }
            children.iter().any(dom_has_interactive_element)
        }
        DomNode::Document { children } => children.iter().any(dom_has_interactive_element),
        _ => false,
    }
}

/// Replace `<noscript>` elements with their children so the content becomes
/// visible.  This is used as a fallback when JS execution fails to produce
/// interactive elements.
///
/// The function replaces each `<noscript>` tag with a `<div>` tag, keeping
/// the original children intact.  A `<noscript>` meta-refresh redirect is
/// removed (its children are dropped) to avoid rendering the redirect URL
/// as visible text.
fn unhide_noscript_content(node: &mut DomNode) {
    match node {
        DomNode::Element { tag, children, attributes } => {
            if tag.eq_ignore_ascii_case("noscript") {
                // Check if this noscript contains a meta-refresh redirect.
                let has_meta_refresh = children.iter().any(|child| {
                    if let DomNode::Element { tag: t, attributes: attrs, .. } = child {
                        t.eq_ignore_ascii_case("meta")
                            && attrs.iter().any(|(k, v)| {
                                k.eq_ignore_ascii_case("http-equiv")
                                    && v.eq_ignore_ascii_case("refresh")
                            })
                    } else {
                        false
                    }
                });

                if has_meta_refresh {
                    // Remove the redirect — don't show it as visible content.
                    *tag = "div".to_string();
                    children.clear();
                    attributes.clear();
                } else {
                    // Convert noscript to a visible div, keeping children.
                    *tag = "div".to_string();
                    attributes.clear();
                }
            }
            // Recurse.
            for child in children.iter_mut() {
                unhide_noscript_content(child);
            }
        }
        DomNode::Document { children } => {
            for child in children.iter_mut() {
                unhide_noscript_content(child);
            }
        }
        _ => {}
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

/// Extract the page title from the `<title>` element in the DOM.
///
/// Walks the DOM tree looking for `<head><title>...</title></head>` and returns
/// the text content. Returns `None` if no title element is found.
fn extract_page_title(node: &DomNode) -> Option<String> {
    match node {
        DomNode::Element { tag, children, .. } => {
            if tag.eq_ignore_ascii_case("title") {
                // Collect all text children.
                let text: String = children
                    .iter()
                    .filter_map(|c| {
                        if let DomNode::Text(t) = c { Some(t.as_str()) } else { None }
                    })
                    .collect();
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            for child in children {
                if let Some(title) = extract_page_title(child) {
                    return Some(title);
                }
            }
            None
        }
        DomNode::Document { children } => {
            for child in children {
                if let Some(title) = extract_page_title(child) {
                    return Some(title);
                }
            }
            None
        }
        _ => None,
    }
}

/// Extract the favicon URL from `<link rel="icon" href="...">` in the DOM.
///
/// Looks for `<link>` elements with `rel="icon"`, `rel="shortcut icon"`, or
/// `rel="apple-touch-icon"` and returns the resolved `href` URL.
fn extract_favicon_url(node: &DomNode, base_url: &Option<Url>) -> Option<String> {
    match node {
        DomNode::Element { tag, attributes, children, .. } => {
            if tag.eq_ignore_ascii_case("link") {
                let rel = attributes
                    .iter()
                    .find(|(k, _)| k == "rel")
                    .map(|(_, v)| v.to_lowercase());
                if matches!(rel.as_deref(), Some("icon") | Some("shortcut icon") | Some("apple-touch-icon")) {
                    if let Some((_, href)) = attributes.iter().find(|(k, _)| k == "href") {
                        if !href.is_empty() {
                            return Some(resolve_url(href, base_url));
                        }
                    }
                }
            }
            for child in children {
                if let Some(url) = extract_favicon_url(child, base_url) {
                    return Some(url);
                }
            }
            None
        }
        DomNode::Document { children } => {
            for child in children {
                if let Some(url) = extract_favicon_url(child, base_url) {
                    return Some(url);
                }
            }
            None
        }
        _ => None,
    }
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

/// Build the HTML for the `about:version` page.
fn build_version_page_html() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<html><head><title>About NOVA</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif;
  margin: 0; padding: 40px; background: #f8f9fa; color: #202124; }}
.container {{ max-width: 600px; margin: 0 auto; }}
h1 {{ font-size: 28px; color: #1a73e8; margin-bottom: 8px; }}
.version {{ font-size: 16px; color: #5f6368; margin-bottom: 24px; }}
table {{ border-collapse: collapse; width: 100%; }}
td {{ padding: 8px 12px; font-size: 14px; border-bottom: 1px solid #e8eaed; }}
td:first-child {{ font-weight: 500; color: #202124; width: 180px; }}
td:last-child {{ color: #5f6368; }}
</style></head>
<body><div class="container">
<h1>NOVA Browser</h1>
<p class="version">Version {version}</p>
<table>
<tr><td>Browser</td><td>NOVA</td></tr>
<tr><td>Version</td><td>{version}</td></tr>
<tr><td>Architecture</td><td>Micro-kernel + Mods</td></tr>
<tr><td>Rendering Engine</td><td>Vello + wgpu (GPU) / FreeType (software)</td></tr>
<tr><td>HTML Parser</td><td>html5ever</td></tr>
<tr><td>CSS Engine</td><td>cssparser + custom</td></tr>
<tr><td>Layout Engine</td><td>Custom + Taffy (Flexbox/Grid)</td></tr>
<tr><td>JavaScript</td><td>QuickJS</td></tr>
<tr><td>Language</td><td>Rust (Edition 2024)</td></tr>
</table>
</div></body></html>"#
    )
}

/// Classify an error into a user-friendly title and detail message.
fn classify_error(error: &NovaError) -> (String, String) {
    match error {
        NovaError::DnsError(host) => (
            "This site can't be reached".into(),
            format!("{host}'s server IP address could not be found. Check your internet connection and DNS settings."),
        ),
        NovaError::NetworkError(msg) => (
            "This site can't be reached".into(),
            format!("A network error occurred: {msg}"),
        ),
        NovaError::TlsError(msg) => (
            "Your connection is not private".into(),
            format!("SSL/TLS certificate error: {msg}"),
        ),
        NovaError::RequestTimeout(cap) => (
            "This site took too long to respond".into(),
            format!("The request for {cap:?} timed out. The server may be overloaded or your connection is slow."),
        ),
        _ => (
            "This page can't be displayed".into(),
            format!("{error}"),
        ),
    }
}

/// Classify an error string into a user-friendly title and detail message.
fn classify_error_str(error: &str) -> (String, String) {
    let lower = error.to_lowercase();
    if lower.contains("dns") || lower.contains("resolve") {
        ("This site can't be reached".into(), error.to_string())
    } else if lower.contains("tls") || lower.contains("ssl") || lower.contains("certificate") {
        ("Your connection is not private".into(), error.to_string())
    } else if lower.contains("timeout") {
        ("This site took too long to respond".into(), error.to_string())
    } else {
        ("This page can't be displayed".into(), error.to_string())
    }
}

/// Escape HTML special characters in a string.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#x27;")
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
        assert!(MAX_IMAGES_TO_FETCH <= 100);
    }

    #[test]
    fn dom_has_interactive_element_finds_input() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "input".into(),
                    attributes: vec![("type".into(), "text".into())],
                    children: vec![],
                }],
            }],
        };
        assert!(dom_has_interactive_element(&dom));
    }

    #[test]
    fn dom_has_interactive_element_ignores_noscript_input() {
        // An <input> inside <noscript> should not count as an interactive
        // element — it's the fallback content.
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "noscript".into(),
                    attributes: vec![],
                    children: vec![DomNode::Element {
                        tag: "input".into(),
                        attributes: vec![],
                        children: vec![],
                    }],
                }],
            }],
        };
        assert!(!dom_has_interactive_element(&dom));
    }

    #[test]
    fn dom_has_interactive_element_empty() {
        let dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "body".into(),
                attributes: vec![],
                children: vec![DomNode::Text("Hello".into())],
            }],
        };
        assert!(!dom_has_interactive_element(&dom));
    }

    #[test]
    fn unhide_noscript_converts_to_div() {
        let mut dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "noscript".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "form".into(),
                    attributes: vec![],
                    children: vec![DomNode::Element {
                        tag: "input".into(),
                        attributes: vec![],
                        children: vec![],
                    }],
                }],
            }],
        };
        unhide_noscript_content(&mut dom);
        // The <noscript> should now be a <div>.
        match &dom {
            DomNode::Document { children } => {
                match &children[0] {
                    DomNode::Element { tag, children: inner, .. } => {
                        assert_eq!(tag, "div");
                        assert_eq!(inner.len(), 1); // <form> preserved
                    }
                    _ => panic!("expected element"),
                }
            }
            _ => panic!("expected document"),
        }
    }

    #[test]
    fn unhide_noscript_removes_meta_refresh() {
        let mut dom = DomNode::Document {
            children: vec![DomNode::Element {
                tag: "noscript".into(),
                attributes: vec![],
                children: vec![DomNode::Element {
                    tag: "meta".into(),
                    attributes: vec![
                        ("http-equiv".into(), "refresh".into()),
                        ("content".into(), "0;url=https://example.com".into()),
                    ],
                    children: vec![],
                }],
            }],
        };
        unhide_noscript_content(&mut dom);
        // The meta-refresh noscript should be converted to an empty div.
        match &dom {
            DomNode::Document { children } => {
                match &children[0] {
                    DomNode::Element { tag, children: inner, .. } => {
                        assert_eq!(tag, "div");
                        assert!(inner.is_empty(), "meta-refresh noscript should have no children");
                    }
                    _ => panic!("expected element"),
                }
            }
            _ => panic!("expected document"),
        }
    }

    #[test]
    fn html_escape_special_chars() {
        assert_eq!(html_escape("<script>alert('xss');</script>"),
            "&lt;script&gt;alert(&#x27;xss&#x27;);&lt;/script&gt;");
        assert_eq!(html_escape("a & b"), "a &amp; b");
        assert_eq!(html_escape("\"quoted\""), "&quot;quoted&quot;");
    }

    #[test]
    fn classify_dns_error() {
        let (title, _detail) = classify_error(&NovaError::DnsError("example.com".into()));
        assert!(title.contains("can't be reached"));
    }

    #[test]
    fn classify_tls_error() {
        let (title, _detail) = classify_error(&NovaError::TlsError("cert expired".into()));
        assert!(title.contains("not private"));
    }

    #[test]
    fn classify_network_error() {
        let (title, _detail) = classify_error(&NovaError::NetworkError("connection refused".into()));
        assert!(title.contains("can't be reached"));
    }

    #[test]
    fn build_version_page_contains_nova() {
        let html = build_version_page_html();
        assert!(html.contains("NOVA Browser"));
        assert!(html.contains(env!("CARGO_PKG_VERSION")));
    }
}
