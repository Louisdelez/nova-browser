//! # nova-gpu
//!
//! GPU compositor — the final stage of the rendering pipeline.
//! Takes RenderCommands from mods and draws them to the screen using wgpu.
//! This is the only part of NOVA that directly talks to the GPU.

pub mod compositor;
pub mod vello_backend;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::{debug, info};

use nova_mod_api::{
    GpuBridge, GpuBufferHandle, GpuTextureHandle, NovaError, RenderCommands, RenderOp,
};

use crate::compositor::LayerTree;
use crate::vello_backend::VelloBackend;

/// The GPU compositor manages the wgpu device and renders frames.
///
/// Integrates the layer-based compositor for efficient scrolling and the
/// Vello backend (stub) for future GPU-native rendering.
pub struct GpuCompositor {
    /// Tracked textures.
    textures: RwLock<HashMap<GpuTextureHandle, TextureInfo>>,
    /// Tracked buffers.
    buffers: RwLock<HashMap<GpuBufferHandle, BufferInfo>>,
    /// Whether the compositor is initialized.
    initialized: bool,
    /// Layer tree for compositing (built from render commands).
    layer_tree: Option<LayerTree>,
    /// Vello rendering backend (stub for future GPU rendering).
    vello_backend: VelloBackend,
}

struct TextureInfo {
    width: u32,
    height: u32,
}

struct BufferInfo {
    size: usize,
}

impl GpuCompositor {
    /// Create a new compositor (wgpu device creation happens at init).
    pub fn new() -> Self {
        Self {
            textures: RwLock::new(HashMap::new()),
            buffers: RwLock::new(HashMap::new()),
            initialized: false,
            layer_tree: None,
            vello_backend: VelloBackend::new(),
        }
    }

    /// Initialize the wgpu device and surface.
    /// Called once when the browser window is created.
    pub fn init(&mut self) -> Result<(), NovaError> {
        info!("GPU: initializing compositor");

        // Initialize the Vello backend (stub).
        self.vello_backend.init(1.0);

        // In the future, this will create the wgpu::Instance, Adapter, Device, Queue.
        // For now, we mark as initialized and will use a software fallback
        // until we integrate with the UI shell's window handle.
        self.initialized = true;

        info!("GPU: compositor ready (layer compositor + vello stub active)");
        Ok(())
    }

    /// Build a layer tree from render commands for efficient compositing.
    ///
    /// Call this after navigation or when render commands change. The layer
    /// tree enables smooth scrolling by caching layer content and only
    /// updating transforms.
    pub fn build_layer_tree(
        &mut self,
        commands: &RenderCommands,
        viewport_width: f32,
        viewport_height: f32,
        url_bar_height: f32,
    ) {
        let tree = LayerTree::from_render_commands(
            commands,
            viewport_width,
            viewport_height,
            url_bar_height,
        );
        info!(
            layers = tree.layer_count(),
            "Layer tree built for compositing"
        );
        self.layer_tree = Some(tree);
    }

    /// Get a reference to the current layer tree (if built).
    pub fn layer_tree(&self) -> Option<&LayerTree> {
        self.layer_tree.as_ref()
    }

    /// Get a mutable reference to the current layer tree.
    pub fn layer_tree_mut(&mut self) -> Option<&mut LayerTree> {
        self.layer_tree.as_mut()
    }

    /// Get a reference to the Vello backend.
    pub fn vello_backend(&self) -> &VelloBackend {
        &self.vello_backend
    }

    /// Get a mutable reference to the Vello backend.
    pub fn vello_backend_mut(&mut self) -> &mut VelloBackend {
        &mut self.vello_backend
    }

    /// Render a set of `RenderOp`s to an RGBA pixel buffer using the Vello GPU backend.
    ///
    /// This is the primary entry point for GPU-accelerated rendering. It delegates
    /// to `VelloBackend::render_to_texture`, which will use the GPU if available
    /// and fall back to software rendering otherwise.
    pub fn render_to_pixels(
        &mut self,
        commands: &RenderCommands,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        self.vello_backend
            .render_to_texture(&commands.ops, width, height)
    }

    /// Render a frame from collected render commands.
    /// This is called by the pipeline after all mods have produced their commands.
    pub fn render_frame(&self, commands: &RenderCommands) -> Result<(), NovaError> {
        if !self.initialized {
            return Err(NovaError::GpuError("compositor not initialized".into()));
        }

        debug!("GPU: rendering frame with {} ops", commands.ops.len());

        // If we have a layer tree, use it for compositing info.
        if let Some(ref tree) = self.layer_tree {
            debug!(
                layers = tree.layer_count(),
                needs_repaint = tree.needs_repaint(),
                "GPU: compositing with layer tree"
            );
        }

        // For now, log what we would render.
        // Full wgpu integration comes when we connect to the UI shell window.
        for op in &commands.ops {
            match op {
                RenderOp::FillRect { x, y, width, height, color } => {
                    debug!("  FillRect ({x},{y} {width}x{height}) color=({:.2},{:.2},{:.2})", color.r, color.g, color.b);
                }
                RenderOp::DrawText { x, y, text, font_size, .. } => {
                    debug!("  DrawText ({x},{y}) size={font_size} \"{text}\"");
                }
                RenderOp::DrawTexture { x, y, width, height, texture } => {
                    debug!("  DrawTexture ({x},{y} {width}x{height}) tex={:?}", texture);
                }
                _ => {
                    debug!("  {:?}", op);
                }
            }
        }

        Ok(())
    }
}

impl Default for GpuCompositor {
    fn default() -> Self {
        Self::new()
    }
}

/// Implementation of GpuBridge that mods use to submit render commands.
pub struct GpuBridgeImpl {
    compositor: Arc<RwLock<GpuCompositor>>,
}

impl GpuBridgeImpl {
    pub fn new(compositor: Arc<RwLock<GpuCompositor>>) -> Self {
        Self { compositor }
    }
}

impl GpuBridge for GpuBridgeImpl {
    fn alloc_buffer(&self, size: usize) -> Result<GpuBufferHandle, NovaError> {
        let handle = GpuBufferHandle::new();
        let comp = self
            .compositor
            .write()
            .map_err(|e| NovaError::GpuError(format!("lock poisoned: {e}")))?;
        comp.buffers
            .write()
            .map_err(|e| NovaError::GpuError(format!("lock poisoned: {e}")))?
            .insert(handle, BufferInfo { size });
        debug!("GPU: allocated buffer {:?} ({size} bytes)", handle);
        Ok(handle)
    }

    fn submit(&self, commands: RenderCommands) -> Result<(), NovaError> {
        let comp = self
            .compositor
            .read()
            .map_err(|e| NovaError::GpuError(format!("lock poisoned: {e}")))?;
        comp.render_frame(&commands)
    }

    fn alloc_texture(&self, width: u32, height: u32) -> Result<GpuTextureHandle, NovaError> {
        let handle = GpuTextureHandle::new();
        let comp = self
            .compositor
            .read()
            .map_err(|e| NovaError::GpuError(format!("lock poisoned: {e}")))?;
        comp.textures
            .write()
            .map_err(|e| NovaError::GpuError(format!("lock poisoned: {e}")))?
            .insert(handle, TextureInfo { width, height });
        debug!("GPU: allocated texture {:?} ({width}x{height})", handle);
        Ok(handle)
    }
}
