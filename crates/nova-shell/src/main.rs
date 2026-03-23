//! NOVA Browser — main entry point.
//!
//! Initializes the micro-kernel, loads essential mods,
//! navigates to a URL, and opens a GPU-accelerated window.
//! Navigation happens in-place within the window (no restart needed).

use std::sync::Arc;

use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use nova_core::NovaCore;
use nova_mod_api::content::TypedData;
use nova_mod_api::{RenderCommands, Viewport};
use nova_shell::{startup, window::BrowserWindow};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing (structured logging).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(true)
        .init();

    println!();
    println!("  ╔══════════════════════════════════════╗");
    println!("  ║          NOVA BROWSER v0.1.0         ║");
    println!("  ║     Micro-Kernel + Mods Architecture ║");
    println!("  ╚══════════════════════════════════════╝");
    println!();

    // Step 1: Create the core (micro-kernel).
    info!("Starting NOVA core...");
    let core = Arc::new(NovaCore::new());

    // Step 2: Initialize (GPU compositor, etc.).
    core.init().await?;

    // Step 3: Load the essential mod pack.
    startup::load_essential_mods(&core).await?;

    // Step 4: Navigate to the initial URL.
    let width = 1280u32;
    let height = 720u32;
    let viewport = Viewport {
        width: width as f32,
        height: height as f32,
        scale_factor: 1.0,
    };

    let initial_url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://example.com".into());

    info!("Navigating to {initial_url}...");
    let render_commands = match core.navigate(&initial_url, viewport).await {
        Ok(TypedData::RenderCommands(cmds)) => {
            info!("Navigation successful! {} render ops", cmds.ops.len());
            cmds
        }
        Ok(_) => {
            info!("Navigation returned non-RenderCommands, using empty frame");
            RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None }
        }
        Err(e) => {
            error!("Navigation failed: {e}");
            RenderCommands { ops: vec![], fonts: vec![], spa_push_url: None, spa_replace_url: None, page_title: None, favicon_url: None }
        }
    };

    // Step 5: Open the window. Navigation happens in-place via the URL bar.
    let title = format!("NOVA - {initial_url}");
    info!("Opening window: {width}x{height}");

    let browser = BrowserWindow::new(
        width,
        height,
        &title,
        render_commands,
        &initial_url,
        core,
    );
    browser.run()?;

    info!("NOVA exited.");
    Ok(())
}
