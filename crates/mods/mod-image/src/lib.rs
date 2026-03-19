//! # mod-image
//!
//! NOVA Mod for image decoding (PNG, JPEG, WebP, GIF). Handles `DecodeImage`
//! capabilities for these formats.
//!
//! Decodes raw image bytes into RGBA8 pixel data using the `image` crate.
//! The decoded pixels are returned as `TypedData::Bytes` with a small header
//! (8 bytes: width u32 LE + height u32 LE) followed by `width * height * 4`
//! bytes of row-major RGBA data.

use std::sync::Arc;

use async_trait::async_trait;
use image::ImageFormat;
use semver::Version;
use tracing::{debug, info};

use nova_mod_api::{
    capability::CapabilityType,
    content::{ContentRequest, TypedData},
    error::NovaError,
    manifest::ModManifest,
    permission::{Permission, TrustLevel},
    trigger::{ContentTrigger, TriggerCondition},
    types::ModId,
    CoreApi, NovaMod,
};

/// The image decoder mod.
pub struct ImageMod {
    manifest: ModManifest,
    core: Option<Arc<dyn CoreApi>>,
}

impl ImageMod {
    /// Create a new `ImageMod` instance.
    pub fn new() -> Self {
        let manifest = ModManifest {
            id: ModId::new("org.nova.image"),
            name: "NOVA Image Decoder".into(),
            version: Version::new(0, 1, 0),
            description: "Image decoder for PNG, JPEG, WebP, and GIF".into(),
            capabilities: vec![
                CapabilityType::DecodeImage("png".into()),
                CapabilityType::DecodeImage("jpeg".into()),
                CapabilityType::DecodeImage("webp".into()),
                CapabilityType::DecodeImage("gif".into()),
            ],
            permissions: vec![Permission::GpuDecode],
            dependencies: vec![],
            triggers: vec![
                ContentTrigger {
                    condition: TriggerCondition::MimeType("image/png".into()),
                    mod_id: ModId::new("org.nova.image"),
                    priority: 100,
                },
                ContentTrigger {
                    condition: TriggerCondition::MimeType("image/jpeg".into()),
                    mod_id: ModId::new("org.nova.image"),
                    priority: 100,
                },
                ContentTrigger {
                    condition: TriggerCondition::MimeType("image/webp".into()),
                    mod_id: ModId::new("org.nova.image"),
                    priority: 100,
                },
                ContentTrigger {
                    condition: TriggerCondition::MimeType("image/gif".into()),
                    mod_id: ModId::new("org.nova.image"),
                    priority: 100,
                },
                // Magic bytes for PNG: 0x89 0x50 0x4E 0x47
                ContentTrigger {
                    condition: TriggerCondition::MagicBytes(vec![0x89, 0x50, 0x4E, 0x47]),
                    mod_id: ModId::new("org.nova.image"),
                    priority: 90,
                },
                // Magic bytes for JPEG: 0xFF 0xD8 0xFF
                ContentTrigger {
                    condition: TriggerCondition::MagicBytes(vec![0xFF, 0xD8, 0xFF]),
                    mod_id: ModId::new("org.nova.image"),
                    priority: 90,
                },
                // Magic bytes for GIF: GIF8
                ContentTrigger {
                    condition: TriggerCondition::MagicBytes(vec![0x47, 0x49, 0x46, 0x38]),
                    mod_id: ModId::new("org.nova.image"),
                    priority: 90,
                },
            ],
            min_core_version: Version::new(0, 1, 0),
            trust_level: TrustLevel::Core,
        };

        Self {
            manifest,
            core: None,
        }
    }
}

impl Default for ImageMod {
    fn default() -> Self {
        Self::new()
    }
}

/// Detect the image format from magic bytes at the start of the data.
fn detect_format(data: &[u8]) -> Option<ImageFormat> {
    if data.len() < 4 {
        return None;
    }
    // PNG: 0x89 P N G
    if data.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        return Some(ImageFormat::Png);
    }
    // JPEG: 0xFF 0xD8
    if data.starts_with(&[0xFF, 0xD8]) {
        return Some(ImageFormat::Jpeg);
    }
    // GIF: GIF8
    if data.starts_with(b"GIF8") {
        return Some(ImageFormat::Gif);
    }
    // WebP: RIFF....WEBP
    if data.len() >= 12 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some(ImageFormat::WebP);
    }
    None
}

/// Map a format hint string (e.g. "png", "jpeg") to an `ImageFormat`.
fn format_from_hint(hint: &str) -> Option<ImageFormat> {
    match hint.to_lowercase().as_str() {
        "png" => Some(ImageFormat::Png),
        "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
        "webp" => Some(ImageFormat::WebP),
        "gif" => Some(ImageFormat::Gif),
        _ => None,
    }
}

/// Encode decoded RGBA image data into the wire format returned by this mod.
///
/// Wire format: `[width: u32 LE][height: u32 LE][RGBA pixels...]`
fn encode_decoded_image(width: u32, height: u32, rgba: &[u8]) -> bytes::Bytes {
    let mut buf = Vec::with_capacity(8 + rgba.len());
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(rgba);
    bytes::Bytes::from(buf)
}

#[async_trait]
impl NovaMod for ImageMod {
    fn manifest(&self) -> &ModManifest {
        &self.manifest
    }

    async fn init(&mut self, core: Arc<dyn CoreApi>) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "image mod initializing");
        self.core = Some(core);
        Ok(())
    }

    async fn handle(&self, request: ContentRequest) -> Result<TypedData, NovaError> {
        match request {
            ContentRequest::DecodeImage { data, format_hint } => {
                debug!(
                    data_len = data.len(),
                    format = ?format_hint,
                    "received image for decoding"
                );

                // Determine format: try magic bytes first, then hint, then let
                // the image crate guess.
                let format = detect_format(&data)
                    .or_else(|| format_hint.as_deref().and_then(format_from_hint));

                let img = if let Some(fmt) = format {
                    image::load_from_memory_with_format(&data, fmt).map_err(|e| {
                        NovaError::DecodeError(format!(
                            "failed to decode image as {fmt:?}: {e}"
                        ))
                    })?
                } else {
                    // Let the image crate auto-detect.
                    image::load_from_memory(&data).map_err(|e| {
                        NovaError::DecodeError(format!("failed to decode image: {e}"))
                    })?
                };

                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());

                debug!(
                    width = w,
                    height = h,
                    "decoded image to RGBA8 ({} bytes)",
                    rgba.as_raw().len()
                );

                let payload = encode_decoded_image(w, h, rgba.as_raw());
                Ok(TypedData::Bytes(payload))
            }
            other => Err(NovaError::UnsupportedContent(format!(
                "image mod cannot handle request: {other:?}"
            ))),
        }
    }

    async fn shutdown(&self) -> Result<(), NovaError> {
        info!(mod_id = %self.manifest.id, "image mod shutting down");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_provides_image_formats() {
        let m = ImageMod::new();
        assert!(m
            .manifest()
            .provides(&CapabilityType::DecodeImage("png".into())));
        assert!(m
            .manifest()
            .provides(&CapabilityType::DecodeImage("jpeg".into())));
        assert!(m
            .manifest()
            .provides(&CapabilityType::DecodeImage("webp".into())));
        assert!(m
            .manifest()
            .provides(&CapabilityType::DecodeImage("gif".into())));
    }

    #[test]
    fn manifest_requires_gpu_decode() {
        let m = ImageMod::new();
        assert!(m.manifest().requires_permission(&Permission::GpuDecode));
    }

    #[test]
    fn detect_png() {
        let data = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(detect_format(&data), Some(ImageFormat::Png));
    }

    #[test]
    fn detect_jpeg() {
        let data = [0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(detect_format(&data), Some(ImageFormat::Jpeg));
    }

    #[test]
    fn detect_gif() {
        assert_eq!(detect_format(b"GIF89a..."), Some(ImageFormat::Gif));
    }

    #[test]
    fn detect_webp() {
        let mut data = Vec::from(b"RIFF" as &[u8]);
        data.extend_from_slice(&[0; 4]); // file size placeholder
        data.extend_from_slice(b"WEBP");
        assert_eq!(detect_format(&data), Some(ImageFormat::WebP));
    }

    #[tokio::test]
    async fn decode_minimal_png() {
        // Create a minimal 1x1 red PNG in memory.
        let mut buf = Vec::new();
        {
            use image::{ImageBuffer, Rgba};
            let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
                ImageBuffer::from_pixel(1, 1, Rgba([255, 0, 0, 255]));
            let mut cursor = std::io::Cursor::new(&mut buf);
            img.write_to(&mut cursor, ImageFormat::Png).unwrap();
        }

        let m = ImageMod::new();
        let req = ContentRequest::DecodeImage {
            data: bytes::Bytes::from(buf),
            format_hint: Some("png".into()),
        };
        let result = m.handle(req).await.unwrap();
        match result {
            TypedData::Bytes(b) => {
                // Header: 4 bytes width + 4 bytes height = 8.
                assert!(b.len() >= 8);
                let w = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                let h = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
                assert_eq!(w, 1);
                assert_eq!(h, 1);
                // RGBA pixel: red.
                assert_eq!(&b[8..12], &[255, 0, 0, 255]);
            }
            other => panic!("expected TypedData::Bytes, got {other:?}"),
        }
    }

    #[test]
    fn encode_decoded_image_format() {
        let pixels = vec![255u8, 0, 0, 255]; // 1x1 red pixel
        let encoded = encode_decoded_image(1, 1, &pixels);
        assert_eq!(encoded.len(), 8 + 4);
        assert_eq!(u32::from_le_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]), 1);
        assert_eq!(u32::from_le_bytes([encoded[4], encoded[5], encoded[6], encoded[7]]), 1);
        assert_eq!(&encoded[8..], &[255, 0, 0, 255]);
    }
}
