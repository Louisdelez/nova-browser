//! # nova-gpu
//!
//! GPU compositor — the final stage of the rendering pipeline.
//! Takes RenderCommands from mods and draws them to the screen using wgpu.
//! This is the only part of NOVA that directly talks to the GPU.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tracing::{debug, error, info};
use uuid::Uuid;

use nova_mod_api::{
    Color, GpuBridge, GpuBufferHandle, GpuTextureHandle, NovaError, RenderCommands, RenderOp,
};

/// The GPU compositor manages the wgpu device and renders frames.
pub struct GpuCompositor {
    /// Tracked textures.
    textures: RwLock<HashMap<GpuTextureHandle, TextureInfo>>,
    /// Tracked buffers.
    buffers: RwLock<HashMap<GpuBufferHandle, BufferInfo>>,
    /// Whether the compositor is initialized.
    initialized: bool,
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
        }
    }

    /// Initialize the wgpu device and surface.
    /// Called once when the browser window is created.
    pub fn init(&mut self) -> Result<(), NovaError> {
        info!("GPU: initializing compositor");

        // In the future, this will create the wgpu::Instance, Adapter, Device, Queue.
        // For now, we mark as initialized and will use a software fallback
        // until we integrate with the UI shell's window handle.
        self.initialized = true;

        info!("GPU: compositor ready");
        Ok(())
    }

    /// Render a frame from collected render commands.
    /// This is called by the pipeline after all mods have produced their commands.
    pub fn render_frame(&self, commands: &RenderCommands) -> Result<(), NovaError> {
        if !self.initialized {
            return Err(NovaError::GpuError("compositor not initialized".into()));
        }

        debug!("GPU: rendering frame with {} ops", commands.ops.len());

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
        let mut comp = self
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
