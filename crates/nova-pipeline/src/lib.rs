//! # nova-pipeline
//!
//! The Pipeline Engine — the brain of the core.
//! Orchestrates the full journey from URL to pixels on screen.
//!
//! The pipeline doesn't do any work itself. It sequences requests
//! to the capability registry, which routes them to the right mods.

use std::sync::Arc;

use bytes::Bytes;
use tracing::{debug, error, info};

use nova_mod_api::{
    CapabilityType, ContentRequest, NovaError, TypedData, Viewport,
    content::HttpResponse,
};
use nova_registry::CapabilityRegistry;

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

        // Step 2: Detect content type and route to appropriate parser
        let dom = self.parse(&response.body, &mime_type).await?;

        // Step 3: Extract sub-resources from DOM (stylesheets, scripts, images)
        // For now, compute styles with an empty stylesheet list.
        let styles = self.compute_styles(&dom).await?;

        // Step 4: Layout
        let layout_tree = self.layout(&styles, viewport).await?;

        // Step 5: Paint
        let render_commands = self.paint(&layout_tree).await?;

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

    /// Step 3: Compute styles for the DOM tree.
    async fn compute_styles(&self, dom: &TypedData) -> Result<TypedData, NovaError> {
        let cap = CapabilityType::ComputeStyles;
        let request = ContentRequest::ComputeStyles {
            dom: Box::new(dom.clone()),
            stylesheets: vec![],
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

    /// Step 5: Paint the layout tree into render commands.
    async fn paint(&self, layout_tree: &TypedData) -> Result<TypedData, NovaError> {
        let cap = CapabilityType::Paint;
        let request = ContentRequest::Paint {
            layout_tree: Box::new(layout_tree.clone()),
        };

        self.registry.route(&cap, request).await
    }
}
