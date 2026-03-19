//! Startup sequence — initializes the core and loads the essential mod pack.

use std::sync::Arc;

use tracing::info;

use nova_core::NovaCore;
use nova_mod_api::NovaError;

/// Load all essential mods into the core.
/// These mods form the "essential pack" — enough to render basic web pages.
pub async fn load_essential_mods(core: &NovaCore) -> Result<(), NovaError> {
    info!("Loading essential mod pack...");

    // Network (HTTP/HTTPS)
    core.load_mod(Box::new(mod_network::NetworkMod::new()))
        .await?;

    // HTML parser
    core.load_mod(Box::new(mod_html_parser::HtmlParserMod::new()))
        .await?;

    // CSS engine
    core.load_mod(Box::new(mod_css_engine::CssEngineMod::new()))
        .await?;

    // Layout engine
    core.load_mod(Box::new(mod_layout::LayoutMod::new()))
        .await?;

    // Painter
    core.load_mod(Box::new(mod_painter::PainterMod::new()))
        .await?;

    // JavaScript (placeholder)
    core.load_mod(Box::new(mod_js::JsMod::new())).await?;

    // Image decoder (placeholder)
    core.load_mod(Box::new(mod_image::ImageMod::new())).await?;

    let caps = core.capabilities().await;
    info!("Essential pack loaded: {} capabilities registered", caps.len());
    for cap in &caps {
        info!("  -> {cap}");
    }

    Ok(())
}
