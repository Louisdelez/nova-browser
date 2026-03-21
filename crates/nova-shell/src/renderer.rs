//! Software renderer — converts RenderCommands into a pixel buffer.
//!
//! This is a simple CPU rasterizer for phase 1.
//! It renders RenderOps into an RGBA pixel buffer that gets uploaded to a wgpu texture.
//! In the future, this will be replaced by Vello (GPU-native rendering).
//!
//! Text rendering uses `fontdue` for real TTF/OTF glyph rasterization with anti-aliasing.
//! If no font file is found, falls back to a built-in bitmap font.

use std::collections::HashMap;
use std::io::Cursor;

use nova_mod_api::{Color, RenderCommands, RenderOp};

use crate::text_shaping::TextShaper;

// ---------------------------------------------------------------------------
// WOFF/WOFF2 decompression helpers
// ---------------------------------------------------------------------------

/// Decompress a font blob if it is WOFF or WOFF2 encoded.
///
/// Returns `Some(decompressed_ttf_bytes)` on success (or if the input is
/// already a plain TTF/OTF). Returns `None` if the data is clearly corrupt
/// or decompression fails.
fn decompress_font_data(raw: &[u8]) -> Option<Vec<u8>> {
    if raw.len() < 4 {
        return None;
    }

    let sig = &raw[..4];

    // WOFF2 signature: "wOF2"
    if sig == b"wOF2" {
        match woff2_patched::convert_woff2_to_ttf(&mut Cursor::new(raw)) {
            Ok(ttf) => return Some(ttf),
            Err(e) => {
                tracing::warn!("WOFF2 decompression failed: {e}");
                return None;
            }
        }
    }

    // WOFF1 signature: "wOFF"
    if sig == b"wOFF" {
        match woff::version1::decompress(raw) {
            Some(ttf) => return Some(ttf),
            None => {
                tracing::warn!("WOFF decompression failed");
                return None;
            }
        }
    }

    // Already a plain TTF/OTF — return as-is.
    Some(raw.to_vec())
}

// ---------------------------------------------------------------------------
// Clip rectangle
// ---------------------------------------------------------------------------

/// An axis-aligned clip rectangle used to restrict rendering.
#[derive(Debug, Clone, Copy)]
struct ClipRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

// ---------------------------------------------------------------------------
// Font rendering
// ---------------------------------------------------------------------------

/// A cached rasterized glyph.
struct CachedGlyph {
    /// Glyph bitmap (coverage values 0–255), row-major.
    bitmap: Vec<u8>,
    metrics: fontdue::Metrics,
}

/// FreeType-based glyph rasterizer for high-quality anti-aliased text.
///
/// Uses FreeType2 (the same library as Chromium/Firefox on Linux) for
/// glyph rasterization with proper hinting and anti-aliasing.
struct FreeTypeRasterizer {
    library: freetype::Library,
    /// Regular face bytes (kept alive for FreeType).
    regular_bytes: Vec<u8>,
    bold_bytes: Option<Vec<u8>>,
    italic_bytes: Option<Vec<u8>>,
    /// Custom font bytes keyed by lowercased family name.
    custom_bytes: HashMap<String, Vec<u8>>,
    /// Glyph cache: (family_or_variant, char, font_size*10) → (bitmap, width, height, bearing_x, bearing_y, advance)
    cache: HashMap<(String, char, u32), FtCachedGlyph>,
}

/// A cached FreeType glyph.
struct FtCachedGlyph {
    bitmap: Vec<u8>,
    width: i32,
    height: i32,
    bearing_x: i32,
    bearing_y: i32,
    advance_x: f32,
}

impl FreeTypeRasterizer {
    fn new(regular_bytes: Vec<u8>) -> Option<Self> {
        let library = freetype::Library::init().ok()?;
        // Validate the font data.
        let face = library.new_memory_face(regular_bytes.clone(), 0).ok()?;
        drop(face);
        Some(Self {
            library,
            regular_bytes,
            bold_bytes: None,
            italic_bytes: None,
            custom_bytes: HashMap::new(),
            cache: HashMap::new(),
        })
    }

    fn set_bold(&mut self, bytes: Vec<u8>) {
        if self.library.new_memory_face(bytes.clone(), 0).is_ok() {
            self.bold_bytes = Some(bytes);
        }
    }

    fn set_italic(&mut self, bytes: Vec<u8>) {
        if self.library.new_memory_face(bytes.clone(), 0).is_ok() {
            self.italic_bytes = Some(bytes);
        }
    }

    fn load_custom_font(&mut self, family: &str, bytes: Vec<u8>) {
        let key = family.to_lowercase();
        if self.custom_bytes.contains_key(&key) {
            return;
        }
        // Decompress WOFF/WOFF2 if needed.
        let bytes = match decompress_font_data(&bytes) {
            Some(b) => b,
            None => return,
        };
        // Evict oldest entry if cache is full (~20 fonts).
        if self.custom_bytes.len() >= 20 {
            if let Some(evict_key) = self.custom_bytes.keys().next().cloned() {
                self.custom_bytes.remove(&evict_key);
                // Also evict associated glyph cache entries.
                self.cache.retain(|(k, _, _), _| k != &evict_key);
            }
        }
        if self.library.new_memory_face(bytes.clone(), 0).is_ok() {
            self.custom_bytes.insert(key, bytes);
        }
    }

    fn has_custom_font(&self, family: &str) -> bool {
        self.custom_bytes.contains_key(&family.to_lowercase())
    }

    /// Get the font bytes for a given variant/family.
    fn font_bytes_for(&self, variant_key: &str) -> &[u8] {
        match variant_key {
            "bold" => self.bold_bytes.as_deref().unwrap_or(&self.regular_bytes),
            "italic" => self.italic_bytes.as_deref().unwrap_or(&self.regular_bytes),
            "regular" => &self.regular_bytes,
            other => self.custom_bytes.get(other).map(|b| b.as_slice()).unwrap_or(&self.regular_bytes),
        }
    }

    /// Rasterize a character and return the cached glyph.
    fn rasterize(&mut self, ch: char, font_size: f32, variant_key: &str) -> &FtCachedGlyph {
        let cache_key = (variant_key.to_string(), ch, (font_size * 10.0).round() as u32);
        if !self.cache.contains_key(&cache_key) {
            let bytes = self.font_bytes_for(variant_key).to_vec();
            let glyph = self.rasterize_uncached(ch, font_size, &bytes);
            self.cache.insert(cache_key.clone(), glyph);
        }
        self.cache.get(&cache_key).unwrap()
    }

    /// Rasterize a glyph by its glyph ID (from rustybuzz shaping) and return
    /// the cached glyph. Uses `load_glyph` instead of `load_char` for correct
    /// rendering of shaped text (ligatures, alternate forms, etc.).
    fn rasterize_glyph_id(&mut self, glyph_id: u32, font_size: f32, variant_key: &str) -> &FtCachedGlyph {
        // Use a separate cache key space: char '\0' with the glyph_id encoded in the size slot
        // would collide, so we use a distinct key format: variant + '\0' char + glyph_id.
        let cache_key = (variant_key.to_string(), '\0', glyph_id * 1000 + (font_size * 10.0).round() as u32);
        if !self.cache.contains_key(&cache_key) {
            let bytes = self.font_bytes_for(variant_key).to_vec();
            let glyph = self.rasterize_glyph_id_uncached(glyph_id, font_size, &bytes);
            self.cache.insert(cache_key.clone(), glyph);
        }
        self.cache.get(&cache_key).unwrap()
    }

    /// Rasterize a glyph by glyph ID (uncached).
    fn rasterize_glyph_id_uncached(&self, glyph_id: u32, font_size: f32, font_bytes: &[u8]) -> FtCachedGlyph {
        let empty = FtCachedGlyph { bitmap: vec![], width: 0, height: 0, bearing_x: 0, bearing_y: 0, advance_x: font_size * 0.5 };
        let face = match self.library.new_memory_face(font_bytes.to_vec(), 0) {
            Ok(f) => f,
            Err(_) => return empty,
        };

        let _ = face.set_char_size((font_size * 64.0) as isize, 0, 72, 72);

        let load_flags = freetype::face::LoadFlag::RENDER | freetype::face::LoadFlag::TARGET_LIGHT;
        if face.load_glyph(glyph_id, load_flags).is_err() {
            return empty;
        }

        let glyph = face.glyph();
        let bmp = glyph.bitmap();
        let width = bmp.width();
        let height = bmp.rows();
        let bearing_x = glyph.bitmap_left();
        let bearing_y = glyph.bitmap_top();
        let advance_x = glyph.advance().x as f32 / 64.0;

        let buffer = bmp.buffer();
        let pitch = bmp.pitch().unsigned_abs() as usize;
        let mut bitmap = Vec::with_capacity((width * height) as usize);
        for row in 0..height as usize {
            let start = row * pitch;
            let end = start + width as usize;
            if end <= buffer.len() {
                bitmap.extend_from_slice(&buffer[start..end]);
            } else {
                bitmap.extend(std::iter::repeat(0).take(width as usize));
            }
        }

        FtCachedGlyph { bitmap, width, height, bearing_x, bearing_y, advance_x }
    }

    fn rasterize_uncached(&self, ch: char, font_size: f32, font_bytes: &[u8]) -> FtCachedGlyph {
        let empty = FtCachedGlyph { bitmap: vec![], width: 0, height: 0, bearing_x: 0, bearing_y: 0, advance_x: font_size * 0.5 };
        let face = match self.library.new_memory_face(font_bytes.to_vec(), 0) {
            Ok(f) => f,
            Err(_) => return empty,
        };

        // Explicitly select Unicode charmap.
        let _ = face.set_char_size((font_size * 64.0) as isize, 0, 72, 72);

        // Load glyph by Unicode code point with anti-aliasing and light hinting.
        let load_flags = freetype::face::LoadFlag::RENDER | freetype::face::LoadFlag::TARGET_LIGHT;
        if face.load_char(ch as usize, load_flags).is_err() {
            return empty;
        }

        let glyph = face.glyph();
        let bmp = glyph.bitmap();
        let width = bmp.width();
        let height = bmp.rows();
        let bearing_x = glyph.bitmap_left();
        let bearing_y = glyph.bitmap_top();
        let advance_x = glyph.advance().x as f32 / 64.0;

        // Copy bitmap data.
        let buffer = bmp.buffer();
        let pitch = bmp.pitch().unsigned_abs() as usize;
        let mut bitmap = Vec::with_capacity((width * height) as usize);
        for row in 0..height as usize {
            let start = row * pitch;
            let end = start + width as usize;
            if end <= buffer.len() {
                bitmap.extend_from_slice(&buffer[start..end]);
            } else {
                bitmap.extend(std::iter::repeat(0).take(width as usize));
            }
        }

        FtCachedGlyph { bitmap, width, height, bearing_x, bearing_y, advance_x }
    }
}

/// Which font variant to use for rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum FontVariant {
    Regular,
    Bold,
    Italic,
    BoldItalic,
}

impl FontVariant {
    /// Determine the variant from CSS font-weight and font-style values.
    fn from_css(weight: Option<u16>, style: Option<&str>) -> Self {
        let is_bold = weight.unwrap_or(400) >= 700;
        let is_italic = style
            .map(|s| s == "italic" || s == "oblique")
            .unwrap_or(false);
        match (is_bold, is_italic) {
            (true, true) => FontVariant::BoldItalic,
            (true, false) => FontVariant::Bold,
            (false, true) => FontVariant::Italic,
            (false, false) => FontVariant::Regular,
        }
    }
}

/// Font renderer backed by `fontdue` with support for bold/italic variants.
///
/// Loads up to 4 TTF fonts at startup (regular, bold, italic, bold-italic)
/// and rasterizes glyphs on demand, caching them in a `HashMap` keyed by
/// `(variant, char, font_size_in_tenths)`.
///
/// Optionally includes a [`TextShaper`] for rustybuzz-based text shaping
/// (ligatures, kerning, BiDi, complex scripts). When available, the shaper
/// is used for text measurement and glyph positioning while fontdue handles
/// the actual rasterization.
struct FontRenderer {
    regular: fontdue::Font,
    bold: Option<fontdue::Font>,
    italic: Option<fontdue::Font>,
    bold_italic: Option<fontdue::Font>,
    /// Cache keyed by (variant, character, font_size * 10 as u32).
    cache: HashMap<(FontVariant, char, u32), CachedGlyph>,
    /// Custom fonts loaded from `@font-face` rules, keyed by family name
    /// (case-insensitive — keys are stored lowercased).
    custom_fonts: HashMap<String, fontdue::Font>,
    /// Cache for custom font glyphs, keyed by (family_lowercase, character, font_size * 10).
    custom_cache: HashMap<(String, char, u32), CachedGlyph>,
    /// Optional rustybuzz text shaper for kerning, ligatures, and BiDi.
    text_shaper: Option<TextShaper>,
    /// FreeType rasterizer for high-quality text rendering (like Chromium/Firefox).
    ft_rasterizer: Option<FreeTypeRasterizer>,
}

impl FontRenderer {
    /// Resolve a single font family name to a TTF file path using fontconfig.
    ///
    /// Handles `system-ui`, `sans-serif`, `serif`, `monospace` generic
    /// families, and arbitrary family names. Returns `None` if fontconfig
    /// is not available or no matching font is found.
    ///
    /// Common web font names (Arial, Helvetica, Times New Roman, etc.) are
    /// mapped to available system fonts when fontconfig cannot find the
    /// exact name.
    fn resolve_font_family(family: &str) -> Option<std::path::PathBuf> {
        use std::sync::Mutex;
        use std::collections::HashMap as StdHashMap;

        // Cache fontconfig results to avoid re-initializing for every call.
        static CACHE: std::sync::LazyLock<Mutex<StdHashMap<String, Option<std::path::PathBuf>>>> =
            std::sync::LazyLock::new(|| Mutex::new(StdHashMap::new()));

        let cleaned = family.trim().trim_matches('"').trim_matches('\'');

        // Map CSS generic families to fontconfig patterns.
        let fc_family = match cleaned {
            "system-ui" => "sans-serif",
            "sans-serif" => "sans-serif",
            "serif" => "serif",
            "monospace" => "monospace",
            "cursive" => "cursive",
            "fantasy" => "fantasy",
            other => other,
        };

        let key = fc_family.to_lowercase();
        if let Ok(cache) = CACHE.lock() {
            if let Some(cached) = cache.get(&key) {
                return cached.clone();
            }
        }

        // Try fontconfig first.
        let result = fontconfig::Fontconfig::new()
            .and_then(|fc| fc.find(fc_family, None))
            .map(|font| font.path);

        // If fontconfig found something, cache and return.
        if result.is_some() {
            if let Ok(mut cache) = CACHE.lock() {
                cache.insert(key, result.clone());
            }
            return result;
        }

        // Fontconfig didn't find it -- try common web font name aliases.
        let alias = match cleaned.to_lowercase().as_str() {
            "arial" | "helvetica" | "helvetica neue" => Some("Liberation Sans"),
            "times new roman" | "times" | "georgia" => Some("Liberation Serif"),
            "courier new" | "courier" => Some("Liberation Mono"),
            "verdana" | "tahoma" | "trebuchet ms" | "lucida grande" => Some("Liberation Sans"),
            "palatino" | "palatino linotype" | "book antiqua" => Some("Liberation Serif"),
            "impact" => Some("Liberation Sans"),
            // Generic CSS families that fontconfig missed -- try known system fonts.
            "cursive" | "fantasy" => Some("DejaVu Sans"),
            _ => None,
        };

        let fallback_result = alias.and_then(|alias_name| {
            fontconfig::Fontconfig::new()
                .and_then(|fc| fc.find(alias_name, None))
                .map(|font| font.path)
        });

        if let Ok(mut cache) = CACHE.lock() {
            cache.insert(key, fallback_result.clone());
        }

        fallback_result
    }

    /// Resolve a CSS font-family stack (comma-separated list) to a system
    /// font path.
    ///
    /// Tries each candidate in order, resolving through fontconfig and
    /// web-font name aliases. Returns the first successful match.
    fn resolve_font_stack(font_family_css: &str) -> Option<std::path::PathBuf> {
        for candidate in font_family_css.split(',') {
            let candidate = candidate.trim().trim_matches('"').trim_matches('\'');
            if candidate.is_empty() {
                continue;
            }
            if let Some(path) = Self::resolve_font_family(candidate) {
                return Some(path);
            }
        }
        None
    }

    /// Try to create a `FontRenderer` from the bundled DejaVu Sans fonts.
    ///
    /// Loads the regular variant (required) plus bold, italic, and bold-italic
    /// variants if available. Returns `None` if the regular font is not found.
    ///
    /// On Linux, uses fontconfig to resolve `sans-serif` as the default font
    /// if the bundled DejaVu Sans is not available.
    fn new() -> Option<Self> {
        // Load the default font.
        let regular_bytes = Self::find_font_bytes("DejaVuSans.ttf")
            .or_else(|| {
                let path = Self::resolve_font_family("sans-serif")?;
                let bytes = std::fs::read(&path).ok()?;
                tracing::info!("Loaded system sans-serif font from {}", path.display());
                Some(bytes)
            })?;

        // Initialize the FreeType rasterizer (same as Chromium/Firefox on Linux).
        let mut ft_rasterizer = FreeTypeRasterizer::new(regular_bytes.clone());
        if ft_rasterizer.is_some() {
            tracing::info!("FreeType rasterizer initialized for high-quality text rendering");
        }

        // Initialize the rustybuzz text shaper.
        let mut text_shaper = TextShaper::new(&regular_bytes);
        if text_shaper.is_some() {
            tracing::info!("rustybuzz TextShaper initialized for text shaping");
        }

        let settings = fontdue::FontSettings::default();
        let regular = fontdue::Font::from_bytes(regular_bytes, settings).ok()?;

        let bold_bytes = Self::find_font_bytes("DejaVuSans-Bold.ttf");
        let bold = bold_bytes
            .as_ref()
            .and_then(|b| fontdue::Font::from_bytes(b.clone(), fontdue::FontSettings::default()).ok());
        if let (Some(shaper), Some(bytes)) = (&mut text_shaper, &bold_bytes) {
            shaper.set_bold(bytes);
        }
        if let (Some(ft), Some(bytes)) = (&mut ft_rasterizer, &bold_bytes) {
            ft.set_bold(bytes.clone());
        }

        let italic_bytes = Self::find_font_bytes("DejaVuSans-Oblique.ttf");
        let italic = italic_bytes
            .as_ref()
            .and_then(|b| fontdue::Font::from_bytes(b.clone(), fontdue::FontSettings::default()).ok());
        if let (Some(shaper), Some(bytes)) = (&mut text_shaper, &italic_bytes) {
            shaper.set_italic(bytes);
        }
        if let (Some(ft), Some(bytes)) = (&mut ft_rasterizer, &italic_bytes) {
            ft.set_italic(bytes.clone());
        }

        let bold_italic = Self::find_font_bytes("DejaVuSans-BoldOblique.ttf")
            .and_then(|b| fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).ok());

        tracing::info!(
            bold = bold.is_some(),
            italic = italic.is_some(),
            bold_italic = bold_italic.is_some(),
            text_shaper = text_shaper.is_some(),
            freetype = ft_rasterizer.is_some(),
            "Font renderer initialized"
        );

        Some(Self {
            regular,
            bold,
            italic,
            bold_italic,
            cache: HashMap::new(),
            custom_fonts: HashMap::new(),
            custom_cache: HashMap::new(),
            text_shaper,
            ft_rasterizer,
        })
    }

    /// Maximum number of custom fonts to keep in the cache.
    ///
    /// Once this limit is reached, the oldest entries are evicted to make
    /// room for new fonts. This prevents unbounded memory growth when
    /// browsing pages that reference many different font families.
    const MAX_CUSTOM_FONTS: usize = 20;

    /// Load a custom font from raw TTF/OTF bytes.
    ///
    /// The font is stored under the given `family` name (lowercased for
    /// case-insensitive lookup). If parsing fails, the font is silently
    /// skipped with a warning. The cache is limited to
    /// [`Self::MAX_CUSTOM_FONTS`] entries.
    fn load_custom_font(&mut self, family: &str, data: Vec<u8>) {
        let key = family.to_lowercase();
        if self.custom_fonts.contains_key(&key) {
            return; // already loaded
        }
        // Decompress WOFF/WOFF2 if needed.
        let data = match decompress_font_data(&data) {
            Some(b) => b,
            None => return,
        };
        // Evict oldest entries if we've hit the cache limit.
        if self.custom_fonts.len() >= Self::MAX_CUSTOM_FONTS {
            // Remove the first key (arbitrary but deterministic for HashMap).
            if let Some(evict_key) = self.custom_fonts.keys().next().cloned() {
                tracing::debug!(family = %evict_key, "evicting cached font (limit reached)");
                self.custom_fonts.remove(&evict_key);
                self.custom_cache.retain(|(k, _, _), _| k != &evict_key);
            }
        }
        // Also load into the text shaper if available.
        if let Some(ref mut shaper) = self.text_shaper {
            shaper.load_custom_font(family, &data);
        }
        let settings = fontdue::FontSettings::default();
        match fontdue::Font::from_bytes(data, settings) {
            Ok(font) => {
                tracing::info!(family = %family, "loaded custom @font-face font");
                self.custom_fonts.insert(key, font);
            }
            Err(e) => {
                tracing::warn!(family = %family, error = %e, "failed to parse @font-face font data");
            }
        }
    }

    /// Rasterize a glyph using a custom font, returning it from cache if available.
    fn rasterize_custom(&mut self, family_lower: &str, ch: char, font_size: f32) -> Option<&CachedGlyph> {
        let key = (family_lower.to_string(), ch, (font_size * 10.0).round() as u32);
        if !self.custom_cache.contains_key(&key) {
            let font = self.custom_fonts.get(family_lower)?;
            let (metrics, bitmap) = font.rasterize(ch, font_size);
            self.custom_cache.insert(key.clone(), CachedGlyph { bitmap, metrics });
        }
        self.custom_cache.get(&key)
    }

    /// Locate font bytes from well-known paths for a given filename.
    fn find_font_bytes(filename: &str) -> Option<Vec<u8>> {
        // 1. Look next to the workspace root (assets/fonts/).
        let manifest_dir = env!("CARGO_MANIFEST_DIR");
        let workspace_root = std::path::Path::new(manifest_dir)
            .parent() // crates/
            .and_then(|p| p.parent()); // workspace root

        if let Some(root) = workspace_root {
            let path = root.join("assets/fonts").join(filename);
            if let Ok(bytes) = std::fs::read(&path) {
                // Validate TTF/OTF magic bytes to reject Git LFS placeholders.
                if bytes.len() > 4
                    && (bytes[0..4] == [0x00, 0x01, 0x00, 0x00]  // TrueType
                        || bytes[0..4] == [0x4F, 0x54, 0x54, 0x4F]  // OpenType (OTTO)
                        || bytes[0..4] == [0x74, 0x72, 0x75, 0x65]) // TrueType (true)
                {
                    tracing::info!("Loaded font from {}", path.display());
                    return Some(bytes);
                } else {
                    tracing::warn!("Skipping invalid font file at {} (not a TTF/OTF)", path.display());
                }
            }
        }

        // 2. Common system paths (Linux / macOS) — only for regular.
        if filename == "DejaVuSans.ttf" {
            let system_paths: &[&str] = &[
                "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
                "/usr/share/fonts/TTF/DejaVuSans.ttf",
                "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
                "/System/Library/Fonts/Helvetica.ttc",
                "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
                "/usr/share/fonts/liberation-sans/LiberationSans-Regular.ttf",
                "/usr/share/fonts/TTF/LiberationSans-Regular.ttf",
                "/usr/share/fonts/truetype/freefont/FreeSans.ttf",
                "/usr/share/fonts/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
                "/usr/share/fonts/google-noto/NotoSans-Regular.ttf",
            ];

            for path in system_paths {
                if let Ok(bytes) = std::fs::read(path) {
                    tracing::info!("Loaded system font from {path}");
                    return Some(bytes);
                }
            }
        }

        // System paths for bold/italic variants.
        let system_filename = match filename {
            "DejaVuSans-Bold.ttf" => Some("/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf"),
            "DejaVuSans-Oblique.ttf" => Some("/usr/share/fonts/truetype/dejavu/DejaVuSans-Oblique.ttf"),
            "DejaVuSans-BoldOblique.ttf" => Some("/usr/share/fonts/truetype/dejavu/DejaVuSans-BoldOblique.ttf"),
            _ => None,
        };
        if let Some(path) = system_filename {
            if let Ok(bytes) = std::fs::read(path) {
                tracing::info!("Loaded system font from {path}");
                return Some(bytes);
            }
        }

        if filename == "DejaVuSans.ttf" {
            tracing::warn!(
                "No TTF font found — text rendering will use the built-in bitmap fallback. \
                 Place a TTF font at assets/fonts/DejaVuSans.ttf for real font rendering."
            );
        }
        None
    }

    /// Rasterize a glyph (or return it from cache) for a specific variant.
    fn rasterize(&mut self, ch: char, font_size: f32, variant: FontVariant) -> &CachedGlyph {
        let key = (variant, ch, (font_size * 10.0).round() as u32);
        // We need to get the font reference before the entry API borrows self.
        // Clone the font pointer data first.
        if !self.cache.contains_key(&key) {
            let font = match variant {
                FontVariant::Bold => self.bold.as_ref().unwrap_or(&self.regular),
                FontVariant::Italic => self.italic.as_ref().unwrap_or(&self.regular),
                FontVariant::BoldItalic => self.bold_italic.as_ref()
                    .or(self.bold.as_ref())
                    .unwrap_or(&self.regular),
                FontVariant::Regular => &self.regular,
            };
            let (metrics, bitmap) = font.rasterize(ch, font_size);
            self.cache.insert(key, CachedGlyph { bitmap, metrics });
        }
        self.cache.get(&key).unwrap()
    }
}

// ---------------------------------------------------------------------------
// Framebuffer
// ---------------------------------------------------------------------------

/// A 2D affine transform matrix stored as `[a, b, c, d, e, f]`.
///
/// Represents the matrix:
/// ```text
/// | a c e |
/// | b d f |
/// | 0 0 1 |
/// ```
#[derive(Debug, Clone, Copy)]
struct AffineTransform {
    m: [f32; 6],
}

impl AffineTransform {
    /// Identity transform.
    fn identity() -> Self {
        Self { m: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0] }
    }

    /// Multiply (compose) self * other.
    fn multiply(&self, other: &AffineTransform) -> AffineTransform {
        let a = &self.m;
        let b = &other.m;
        AffineTransform {
            m: [
                a[0] * b[0] + a[2] * b[1],
                a[1] * b[0] + a[3] * b[1],
                a[0] * b[2] + a[2] * b[3],
                a[1] * b[2] + a[3] * b[3],
                a[0] * b[4] + a[2] * b[5] + a[4],
                a[1] * b[4] + a[3] * b[5] + a[5],
            ],
        }
    }

    /// Transform a point (x, y).
    fn transform_point(&self, x: f32, y: f32) -> (f32, f32) {
        let m = &self.m;
        (m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5])
    }

    /// Whether this is the identity transform.
    fn is_identity(&self) -> bool {
        let m = &self.m;
        (m[0] - 1.0).abs() < 1e-6
            && m[1].abs() < 1e-6
            && m[2].abs() < 1e-6
            && (m[3] - 1.0).abs() < 1e-6
            && m[4].abs() < 1e-6
            && m[5].abs() < 1e-6
    }

    /// Create from a raw matrix array `[a, b, c, d, e, f]`.
    fn from_array(m: [f32; 6]) -> Self {
        Self { m }
    }
}

/// A simple software framebuffer.
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    /// RGBA pixel data, row-major, 4 bytes per pixel.
    pub pixels: Vec<u8>,
    /// Optional fontdue-based renderer (None when no font file is available).
    font_renderer: Option<FontRenderer>,
    /// Stack of clip rectangles. Rendering is restricted to the intersection of
    /// all active clip rects.
    clip_stack: Vec<ClipRect>,
    /// Extra y-offset applied by sticky positioning (reset each frame).
    translate_y_offset: f32,
    /// Stack of opacity values for nested PushOpacity/PopOpacity.
    opacity_stack: Vec<f32>,
    /// Stack of 2D affine transforms for Save/Transform/Restore.
    transform_stack: Vec<AffineTransform>,
    /// Current accumulated transform (product of all transforms in the stack).
    current_transform: AffineTransform,
    /// Nesting depth of fixed-position elements. When > 0, scroll offsets are
    /// ignored so fixed elements stay anchored to the viewport.
    fixed_depth: u32,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        let size = (width * height * 4) as usize;
        let pixels = vec![255u8; size]; // White background
        let font_renderer = FontRenderer::new();
        Self {
            width,
            height,
            pixels,
            font_renderer,
            clip_stack: Vec::new(),
            translate_y_offset: 0.0,
            opacity_stack: Vec::new(),
            transform_stack: Vec::new(),
            current_transform: AffineTransform::identity(),
            fixed_depth: 0,
        }
    }

    /// Reset the framebuffer to a new size, reusing the font renderer.
    /// Use this instead of `new()` when rebuilding frames (avoids reloading the font).
    pub fn reset(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        let size = (width * height * 4) as usize;
        self.pixels.resize(size, 255);
        self.pixels.fill(255);
    }

    /// Load custom `@font-face` fonts into the font renderer.
    ///
    /// Each entry is `(family_name, font_bytes)`. Fonts that have already been
    /// loaded (same family name) are skipped. Only TTF/OTF data is accepted;
    /// fontdue will reject anything else.
    pub fn load_custom_fonts(&mut self, fonts: &[(String, Vec<u8>)]) {
        if let Some(ref mut renderer) = self.font_renderer {
            for (family, data) in fonts {
                renderer.load_custom_font(family, data.clone());
                if let Some(ref mut ft) = renderer.ft_rasterizer {
                    ft.load_custom_font(family, data.clone());
                }
            }
        } else {
            tracing::warn!(
                "cannot load custom fonts: no font renderer available (no base font loaded)"
            );
        }
    }

    /// Measure the width of text at a given font size without rendering.
    ///
    /// When the rustybuzz [`TextShaper`] is available, uses shaped glyph
    /// advances for accurate measurement (including kerning and ligatures).
    /// Otherwise falls back to fontdue per-character advance widths.
    pub fn measure_text_width(&mut self, text: &str, font_size: f32) -> f32 {
        if let Some(ref mut renderer) = self.font_renderer {
            // Prefer the text shaper for more accurate measurement.
            if let Some(ref shaper) = renderer.text_shaper {
                return shaper.measure_width(text, font_size);
            }

            let mut width: f32 = 0.0;
            for ch in text.chars() {
                let glyph = renderer.rasterize(ch, font_size, FontVariant::Regular);
                width += glyph.metrics.advance_width as f32;
            }
            width
        } else {
            // Fallback: monospace estimate
            let scale = font_size / 16.0;
            text.len() as f32 * 8.0 * scale
        }
    }

    /// Compute the current effective opacity from the opacity stack.
    ///
    /// Returns the product of all active opacity values (1.0 if the stack is empty).
    fn effective_opacity(&self) -> f32 {
        self.opacity_stack.iter().copied().fold(1.0_f32, |acc, o| acc * o)
    }

    /// Apply the current opacity to a color by multiplying its alpha.
    fn apply_opacity(&self, color: Color) -> Color {
        let opacity = self.effective_opacity();
        if opacity >= 1.0 {
            color
        } else {
            Color { r: color.r, g: color.g, b: color.b, a: color.a * opacity }
        }
    }

    /// Compute the effective clip bounds from the clip stack.
    ///
    /// Returns `(x0, y0, x1, y1)` representing the intersection of all active
    /// clip rectangles, or the full framebuffer bounds if the stack is empty.
    fn effective_clip(&self) -> (i32, i32, i32, i32) {
        if self.clip_stack.is_empty() {
            (0, 0, self.width as i32, self.height as i32)
        } else {
            let mut cx0 = 0i32;
            let mut cy0 = 0i32;
            let mut cx1 = self.width as i32;
            let mut cy1 = self.height as i32;
            for clip in &self.clip_stack {
                cx0 = cx0.max(clip.x);
                cy0 = cy0.max(clip.y);
                cx1 = cx1.min(clip.x + clip.width);
                cy1 = cy1.min(clip.y + clip.height);
            }
            (cx0, cy0, cx1, cy1)
        }
    }

    /// Clear the framebuffer with a color.
    pub fn clear(&mut self, color: Color) {
        let r = (color.r * 255.0) as u8;
        let g = (color.g * 255.0) as u8;
        let b = (color.b * 255.0) as u8;
        let a = (color.a * 255.0) as u8;
        for chunk in self.pixels.chunks_exact_mut(4) {
            chunk[0] = r;
            chunk[1] = g;
            chunk[2] = b;
            chunk[3] = a;
        }
    }

    /// Set a pixel at (x, y) with blending.
    #[inline]
    fn set_pixel(&mut self, x: i32, y: i32, color: Color) {
        if x < 0 || y < 0 || x >= self.width as i32 || y >= self.height as i32 {
            return;
        }

        // Enforce the clip stack — skip pixels outside the effective clip region.
        if !self.clip_stack.is_empty() {
            let (cx0, cy0, cx1, cy1) = self.effective_clip();
            if x < cx0 || x >= cx1 || y < cy0 || y >= cy1 {
                return;
            }
        }

        let idx = ((y as u32 * self.width + x as u32) * 4) as usize;
        if idx + 3 >= self.pixels.len() {
            return;
        }

        let a = color.a;
        if a >= 1.0 {
            self.pixels[idx] = (color.r * 255.0) as u8;
            self.pixels[idx + 1] = (color.g * 255.0) as u8;
            self.pixels[idx + 2] = (color.b * 255.0) as u8;
            self.pixels[idx + 3] = 255;
        } else if a > 0.0 {
            // Alpha blend.
            let inv_a = 1.0 - a;
            let dst_r = self.pixels[idx] as f32 / 255.0;
            let dst_g = self.pixels[idx + 1] as f32 / 255.0;
            let dst_b = self.pixels[idx + 2] as f32 / 255.0;
            self.pixels[idx] = ((color.r * a + dst_r * inv_a) * 255.0) as u8;
            self.pixels[idx + 1] = ((color.g * a + dst_g * inv_a) * 255.0) as u8;
            self.pixels[idx + 2] = ((color.b * a + dst_b * inv_a) * 255.0) as u8;
            self.pixels[idx + 3] = 255;
        }
    }

    /// Blend a single coverage value from a glyph bitmap into the framebuffer.
    ///
    /// `coverage` is 0–255 (0 = fully transparent, 255 = fully opaque).
    /// The glyph colour is `color`, blended against the existing pixel.
    #[inline]
    fn blend_glyph_pixel(&mut self, x: i32, y: i32, coverage: u8, color: Color) {
        if coverage == 0 {
            return;
        }
        let alpha = (coverage as f32 / 255.0) * color.a;
        self.set_pixel(
            x,
            y,
            Color {
                r: color.r,
                g: color.g,
                b: color.b,
                a: alpha,
            },
        );
    }

    /// Fill a rectangle.
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        let x0 = x.round() as i32;
        let y0 = y.round() as i32;
        let x1 = (x + w).round() as i32;
        let y1 = (y + h).round() as i32;

        for py in y0..y1 {
            for px in x0..x1 {
                self.set_pixel(px, py, color);
            }
        }
    }

    /// Fill a rectangle with rounded corners using an SDF-based approach.
    ///
    /// `radius` is `[top-left, top-right, bottom-right, bottom-left]` in pixels.
    /// For each pixel inside the bounding rect we compute the distance from the
    /// nearest corner arc.  Pixels inside the rounded shape are filled; pixels
    /// outside the corner arcs are skipped.
    pub fn fill_rounded_rect(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        color: Color,
        radius: [f32; 4],
    ) {
        // Clamp each radius so overlapping corners don't exceed half the side.
        let max_r_h = w * 0.5;
        let max_r_v = h * 0.5;
        let r_tl = radius[0].min(max_r_h).min(max_r_v).max(0.0);
        let r_tr = radius[1].min(max_r_h).min(max_r_v).max(0.0);
        let r_br = radius[2].min(max_r_h).min(max_r_v).max(0.0);
        let r_bl = radius[3].min(max_r_h).min(max_r_v).max(0.0);

        let x0 = x.floor() as i32;
        let y0 = y.floor() as i32;
        let x1 = (x + w).ceil() as i32;
        let y1 = (y + h).ceil() as i32;

        for py in y0..y1 {
            // Pixel center within the rect (local coordinates).
            let fy = py as f32 + 0.5 - y;
            for px in x0..x1 {
                let fx = px as f32 + 0.5 - x;

                // Determine which corner quadrant this pixel falls into and
                // check whether it is inside the rounded corner arc.
                let inside = if fx < r_tl && fy < r_tl {
                    // Top-left corner.
                    let dx = r_tl - fx;
                    let dy = r_tl - fy;
                    dx * dx + dy * dy <= r_tl * r_tl
                } else if fx > w - r_tr && fy < r_tr {
                    // Top-right corner.
                    let dx = fx - (w - r_tr);
                    let dy = r_tr - fy;
                    dx * dx + dy * dy <= r_tr * r_tr
                } else if fx > w - r_br && fy > h - r_br {
                    // Bottom-right corner.
                    let dx = fx - (w - r_br);
                    let dy = fy - (h - r_br);
                    dx * dx + dy * dy <= r_br * r_br
                } else if fx < r_bl && fy > h - r_bl {
                    // Bottom-left corner.
                    let dx = r_bl - fx;
                    let dy = fy - (h - r_bl);
                    dx * dx + dy * dy <= r_bl * r_bl
                } else {
                    // Not in any corner arc → always inside the rounded rect.
                    true
                };

                if inside {
                    self.set_pixel(px, py, color);
                }
            }
        }
    }

    /// Draw a horizontal line.
    fn hline(&mut self, x0: i32, x1: i32, y: i32, color: Color) {
        for px in x0..x1 {
            self.set_pixel(px, y, color);
        }
    }

    /// Draw a rectangle border.
    pub fn stroke_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color, line_width: f32) {
        let lw = line_width.round() as i32;
        let x0 = x.round() as i32;
        let y0 = y.round() as i32;
        let x1 = (x + w).round() as i32;
        let y1 = (y + h).round() as i32;

        // Top and bottom.
        for dy in 0..lw {
            self.hline(x0, x1, y0 + dy, color);
            self.hline(x0, x1, y1 - 1 - dy, color);
        }
        // Left and right.
        for py in y0..y1 {
            for dx in 0..lw {
                self.set_pixel(x0 + dx, py, color);
                self.set_pixel(x1 - 1 - dx, py, color);
            }
        }
    }

    /// Draw text using the fontdue renderer with glyph caching, anti-aliasing,
    /// and automatic line wrapping. Supports bold/italic via font variants.
    ///
    /// When `font_family` is `Some` and a matching custom `@font-face` font has
    /// been loaded, that font is used instead of the default DejaVu Sans.
    ///
    /// Falls back to the built-in bitmap font if no TTF font was loaded.
    pub fn draw_text(
        &mut self,
        x: f32,
        y: f32,
        text: &str,
        font_size: f32,
        color: Color,
        font_weight: Option<u16>,
        font_style: Option<&str>,
        font_family: Option<&str>,
        letter_spacing: Option<f32>,
    ) {
        if self.font_renderer.is_none() {
            self.draw_text_bitmap(x, y, text, font_size, color);
            return;
        }

        let variant = FontVariant::from_css(font_weight, font_style);

        // Check if we should use a custom @font-face font, or resolve
        // the font-family via fontconfig if not already loaded.
        let custom_family_key = font_family.and_then(|family_str| {
            // Parse CSS font-family (comma-separated list) and try each.
            for candidate in family_str.split(',') {
                let candidate = candidate.trim().trim_matches('"').trim_matches('\'');
                if candidate.is_empty() { continue; }
                let key = candidate.to_lowercase();
                let has_it = self.font_renderer
                    .as_ref()
                    .map(|r| r.custom_fonts.contains_key(&key))
                    .unwrap_or(false);
                if has_it {
                    return Some(key);
                }
            }
            // None of the candidates were already loaded -- resolve the
            // font stack via fontconfig (with web-font name aliases) and
            // load the first match on demand.
            if let Some(path) = FontRenderer::resolve_font_stack(family_str) {
                // Determine which candidate name this path corresponds to.
                // Use the file stem as the family key for cache purposes.
                let family_name = path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("resolved-font");
                let key = family_name.to_lowercase();
                if let Ok(bytes) = std::fs::read(&path) {
                    if let Some(ref mut renderer) = self.font_renderer {
                        renderer.load_custom_font(family_name, bytes.clone());
                        if let Some(ref mut ft) = renderer.ft_rasterizer {
                            ft.load_custom_font(family_name, bytes);
                        }
                        if renderer.custom_fonts.contains_key(&key) {
                            return Some(key);
                        }
                    }
                }
            }
            None
        });

        // We need to temporarily take the font renderer out of `self` so we can
        // mutably borrow both `self` (for pixel writes) and the renderer (for
        // caching). We put it back at the end.
        let mut renderer = self.font_renderer.take().unwrap();

        let fb_width = self.width as i32;

        let mut cx = x.round() as i32;
        let mut cy = y.round() as i32;
        let line_height = (font_size * 1.2).round() as i32;

        // Try shaped rendering when the text shaper is available.
        // Word merging in the layout IFC ensures words + spaces are combined
        // into single DrawText calls, so kerning differences don't cause
        // inter-word gaps.
        let use_shaper = renderer.text_shaper.is_some() && !text.contains('\n');
        if use_shaper {
            let is_bold = font_weight.unwrap_or(400) >= 700;
            let is_italic = font_style
                .map(|s| s == "italic" || s == "oblique")
                .unwrap_or(false);

            let shaped_run = if let Some(ref family_key) = custom_family_key {
                renderer.text_shaper.as_ref().unwrap()
                    .shape_custom(text, font_size, &[], family_key)
            } else {
                renderer.text_shaper.as_ref().unwrap()
                    .shape_variant(text, font_size, &[], is_bold, is_italic)
            };

            // Map cluster indices back to characters for fontdue rasterization.
            let chars: Vec<char> = text.chars().collect();
            let mut cursor_x = x;

            for glyph in &shaped_run.glyphs {
                // Find the character for this cluster.
                let ch = chars
                    .get(glyph.cluster as usize)
                    .copied()
                    .unwrap_or('?');

                // Line wrapping check.
                let glyph_cx = (cursor_x + glyph.x_offset).round() as i32;
                if glyph_cx + (glyph.x_advance as i32) > fb_width && glyph_cx > x.round() as i32 {
                    cursor_x = x;
                    cy += line_height;
                }

                // Determine the FreeType variant key.
                let ft_variant_key = if let Some(ref fk) = custom_family_key {
                    fk.as_str()
                } else if is_bold {
                    "bold"
                } else if is_italic {
                    "italic"
                } else {
                    "regular"
                };

                // Rasterize with FreeType (high quality) or fontdue (fallback).
                let draw_x = (cursor_x + glyph.x_offset).round() as i32;
                let draw_y = cy;

                if let Some(ref mut ft) = renderer.ft_rasterizer {
                    let ft_glyph = ft.rasterize_glyph_id(glyph.glyph_id as u32, font_size, ft_variant_key);
                    let gx = draw_x + ft_glyph.bearing_x;
                    let gy = draw_y - ft_glyph.bearing_y + glyph.y_offset.round() as i32;
                    let bw = ft_glyph.width;
                    let bh = ft_glyph.height;
                    let bitmap = ft_glyph.bitmap.clone();

                    for row in 0..bh {
                        for col in 0..bw {
                            let coverage = bitmap[(row * bw + col) as usize];
                            self.blend_glyph_pixel(gx + col, gy + row, coverage, color);
                        }
                    }
                } else {
                    // Fallback: fontdue rasterization.
                    let cached = renderer.rasterize(ch, font_size, variant);
                    let metrics = cached.metrics;
                    let bitmap = cached.bitmap.clone();
                    let gx = draw_x + metrics.xmin;
                    let gy = draw_y - metrics.ymin + glyph.y_offset.round() as i32;
                    let bw = metrics.width;
                    let bh = metrics.height;
                    for row in 0..bh {
                        for col in 0..bw {
                            let coverage = bitmap[row * bw + col];
                            self.blend_glyph_pixel(gx + col as i32, gy + row as i32, coverage, color);
                        }
                    }
                }

                cursor_x += glyph.x_advance;
                if let Some(ls) = letter_spacing {
                    cursor_x += ls;
                }
            }

            // Put the renderer back.
            self.font_renderer = Some(renderer);
            return;
        }

        // Fallback: character-by-character rendering without shaping.
        // Determine FreeType variant key for the fallback path.
        let ft_fallback_key = if custom_family_key.is_some() {
            custom_family_key.as_deref().unwrap_or("regular")
        } else if font_weight.unwrap_or(400) >= 700 {
            "bold"
        } else if font_style.map(|s| s == "italic" || s == "oblique").unwrap_or(false) {
            "italic"
        } else {
            "regular"
        };

        for ch in text.chars() {
            if ch == '\n' {
                cx = x.round() as i32;
                cy += line_height;
                continue;
            }

            // Use FreeType if available, otherwise fontdue.
            if let Some(ref mut ft) = renderer.ft_rasterizer {
                let ft_glyph = ft.rasterize(ch, font_size, ft_fallback_key);
                let advance = ft_glyph.advance_x;

                if cx + advance as i32 > fb_width && cx > x.round() as i32 {
                    cx = x.round() as i32;
                    cy += line_height;
                }

                let gx = cx + ft_glyph.bearing_x;
                let gy = cy - ft_glyph.bearing_y;
                let bw = ft_glyph.width;
                let bh = ft_glyph.height;
                let bitmap = ft_glyph.bitmap.clone();

                for row in 0..bh {
                    for col in 0..bw {
                        let coverage = bitmap[(row * bw + col) as usize];
                        self.blend_glyph_pixel(gx + col, gy + row, coverage, color);
                    }
                }
                cx += advance.round() as i32;
                if let Some(ls) = letter_spacing {
                    cx += ls.round() as i32;
                }
            } else {
                // fontdue fallback.
                let (metrics, bitmap) = if let Some(ref family_key) = custom_family_key {
                    if let Some(glyph) = renderer.rasterize_custom(family_key, ch, font_size) {
                        (glyph.metrics, glyph.bitmap.clone())
                    } else {
                        let glyph = renderer.rasterize(ch, font_size, variant);
                        (glyph.metrics, glyph.bitmap.clone())
                    }
                } else {
                    let glyph = renderer.rasterize(ch, font_size, variant);
                    (glyph.metrics, glyph.bitmap.clone())
                };

                if cx + metrics.advance_width as i32 > fb_width && cx > x.round() as i32 {
                    cx = x.round() as i32;
                    cy += line_height;
                }

                let gx = cx + metrics.xmin;
                let gy = cy - metrics.ymin;
                let bw = metrics.width;
                let bh = metrics.height;

                for row in 0..bh {
                    for col in 0..bw {
                        let coverage = bitmap[row * bw + col];
                        self.blend_glyph_pixel(gx + col as i32, gy + row as i32, coverage, color);
                    }
                }
                cx += metrics.advance_width as i32;
                if let Some(ls) = letter_spacing {
                    cx += ls.round() as i32;
                }
            }
        }

        // Put the renderer back.
        self.font_renderer = Some(renderer);
    }

    /// Draw text using the built-in bitmap font (fallback).
    ///
    /// This is the original 8x16 monospace bitmap renderer, used when no TTF
    /// font is available.
    fn draw_text_bitmap(&mut self, x: f32, y: f32, text: &str, font_size: f32, color: Color) {
        let scale = (font_size / 16.0).max(0.5);
        let char_w = (8.0 * scale) as i32;
        let char_h = (16.0 * scale) as i32;
        let fb_width = self.width as i32;

        let mut cx = x.round() as i32;
        let mut cy = y.round() as i32;

        for ch in text.chars() {
            if ch == '\n' {
                cx = x.round() as i32;
                cy += char_h;
                continue;
            }

            // Line wrapping.
            if cx + char_w > fb_width && cx > x.round() as i32 {
                cx = x.round() as i32;
                cy += char_h;
            }

            if ch == ' ' {
                cx += char_w;
                continue;
            }

            let glyph = get_basic_glyph(ch);
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if bits & (1 << (7 - col)) != 0 {
                        let px = cx + (col as f32 * scale) as i32;
                        let py = cy + (row as f32 * scale) as i32;
                        self.set_pixel(px, py, color);
                        // If scaling up, fill the scaled pixel.
                        if scale > 1.0 {
                            let s = scale.ceil() as i32;
                            for dy in 0..s {
                                for dx in 0..s {
                                    self.set_pixel(px + dx, py + dy, color);
                                }
                            }
                        }
                    }
                }
            }

            cx += char_w;
        }
    }

    /// Render a full set of RenderCommands.
    pub fn render(&mut self, commands: &RenderCommands) {
        self.render_with_offset(commands, 0.0);
    }

    /// Render a full set of RenderCommands with a vertical offset applied to all operations.
    ///
    /// This is used to shift page content down to make room for the URL bar.
    pub fn render_with_offset(&mut self, commands: &RenderCommands, y_offset: f32) {
        self.render_scrolled(commands, y_offset, 0.0, 0.0, 0.0);
    }

    /// Render commands with both a static y-offset (e.g. URL bar) and a scroll offset.
    ///
    /// `y_offset` shifts content down (for chrome elements above the page).
    /// `scroll_y` shifts content up (the user has scrolled down by this many pixels).
    /// `content_height` is the total height of the rendered content (for the scrollbar).
    /// If `content_height` is 0, no scrollbar is drawn.
    pub fn render_scrolled(
        &mut self,
        commands: &RenderCommands,
        y_offset: f32,
        scroll_x: f32,
        scroll_y: f32,
        content_height: f32,
    ) {
        self.clear(Color::WHITE);
        self.clip_stack.clear();
        self.opacity_stack.clear();
        self.translate_y_offset = 0.0;
        self.transform_stack.clear();
        self.current_transform = AffineTransform::identity();
        self.fixed_depth = 0;

        // Load any custom @font-face fonts that haven't been loaded yet.
        if !commands.fonts.is_empty() {
            self.load_custom_fonts(&commands.fonts);
        }

        let sx = scroll_x;

        for op in &commands.ops {
            let sy_extra = self.translate_y_offset;
            // When inside a fixed-position element, ignore scroll offsets
            // so the element stays anchored to the viewport.
            let effective_scroll_x = if self.fixed_depth > 0 { 0.0 } else { sx };
            let effective_scroll_y = if self.fixed_depth > 0 { 0.0 } else { scroll_y };

            // Helper: apply scroll/offset then current transform to a point.
            let apply_pos = |ct: &AffineTransform, x: f32, y: f32| -> (f32, f32) {
                let base_x = x - effective_scroll_x;
                let base_y = y + y_offset - effective_scroll_y + sy_extra;
                if ct.is_identity() {
                    (base_x, base_y)
                } else {
                    ct.transform_point(base_x, base_y)
                }
            };

            match op {
                RenderOp::FillRect { x, y, width, height, color } => {
                    let c = self.apply_opacity(*color);
                    let (tx, ty) = apply_pos(&self.current_transform, *x, *y);
                    self.fill_rect(tx, ty, *width, *height, c);
                }
                RenderOp::DrawText { x, y, text, font_size, color, font_weight, font_style, font_family, letter_spacing } => {
                    let c = self.apply_opacity(*color);
                    let (tx, ty) = apply_pos(&self.current_transform, *x, *y);
                    self.draw_text(
                        tx, ty, text, *font_size, c,
                        *font_weight, font_style.as_deref(), font_family.as_deref(),
                        *letter_spacing,
                    );
                }
                RenderOp::StrokeRect { x, y, width, height, color, width_px } => {
                    let c = self.apply_opacity(*color);
                    let (tx, ty) = apply_pos(&self.current_transform, *x, *y);
                    self.stroke_rect(tx, ty, *width, *height, c, *width_px);
                }
                RenderOp::DrawImage {
                    x, y, width, height,
                    img_width, img_height, pixels,
                } => {
                    let (tx, ty) = apply_pos(&self.current_transform, *x, *y);
                    self.draw_image(
                        tx, ty, *width, *height,
                        *img_width, *img_height, pixels,
                    );
                }
                RenderOp::PushClip { x, y, width, height } => {
                    let (tx, ty) = apply_pos(&self.current_transform, *x, *y);
                    self.clip_stack.push(ClipRect {
                        x: tx.round() as i32,
                        y: ty.round() as i32,
                        width: width.round() as i32,
                        height: height.round() as i32,
                    });
                }
                RenderOp::PushRoundedClip { x, y, width, height, .. } => {
                    // For the software renderer, rounded clips are approximated
                    // as rectangular clips. Full rounded clipping would require
                    // per-pixel SDF masking which is expensive on CPU.
                    let (tx, ty) = apply_pos(&self.current_transform, *x, *y);
                    self.clip_stack.push(ClipRect {
                        x: tx.round() as i32,
                        y: ty.round() as i32,
                        width: width.round() as i32,
                        height: height.round() as i32,
                    });
                }
                RenderOp::PopClip => {
                    self.clip_stack.pop();
                }
                RenderOp::FillRoundedRect { x, y, width, height, color, radius } => {
                    let c = self.apply_opacity(*color);
                    let (tx, ty) = apply_pos(&self.current_transform, *x, *y);
                    self.fill_rounded_rect(
                        tx, ty, *width, *height, c, *radius,
                    );
                }
                RenderOp::BoxShadow {
                    x, y, width, height, color, offset_x, offset_y, blur: _,
                } => {
                    let c = self.apply_opacity(*color);
                    let (tx, ty) = apply_pos(&self.current_transform, *x + *offset_x, *y + *offset_y);
                    self.fill_rect(
                        tx,
                        ty,
                        *width,
                        *height,
                        c,
                    );
                }
                RenderOp::PushOpacity { opacity } => {
                    self.opacity_stack.push(*opacity);
                }
                RenderOp::PopOpacity => {
                    self.opacity_stack.pop();
                }
                // Sticky positioning: adjust the y-offset for subsequent ops
                // so the element sticks to the viewport during scroll.
                RenderOp::StickyStart { original_y, sticky_top } => {
                    // If the element would scroll above sticky_top in the viewport,
                    // push a translation to keep it at sticky_top.
                    let element_viewport_y = *original_y + y_offset - scroll_y;
                    if element_viewport_y < y_offset + *sticky_top {
                        let offset = (y_offset + *sticky_top) - element_viewport_y;
                        self.translate_y_offset += offset;
                    }
                }
                RenderOp::StickyEnd => {
                    self.translate_y_offset = 0.0;
                }
                // Save/Restore/Transform — manage the transform stack.
                RenderOp::Save => {
                    self.transform_stack.push(self.current_transform);
                }
                RenderOp::Restore => {
                    if let Some(prev) = self.transform_stack.pop() {
                        self.current_transform = prev;
                    }
                }
                RenderOp::Transform { matrix } => {
                    let t = AffineTransform::from_array(*matrix);
                    self.current_transform = self.current_transform.multiply(&t);
                }
                RenderOp::Translate { x: tx, y: ty } => {
                    let t = AffineTransform::from_array([1.0, 0.0, 0.0, 1.0, *tx, *ty]);
                    self.current_transform = self.current_transform.multiply(&t);
                }
                // Fixed positioning: ignore scroll offsets for enclosed ops.
                RenderOp::FixedStart => {
                    self.fixed_depth += 1;
                }
                RenderOp::FixedEnd => {
                    self.fixed_depth = self.fixed_depth.saturating_sub(1);
                }
                // Link ops are metadata-only; they don't draw anything.
                RenderOp::Link { .. } => {}
                // Other ops will be implemented as needed.
                _ => {}
            }
        }

        // Draw vertical scrollbar if content is taller than the page area.
        let page_area_height = (self.height as f32 - y_offset).max(0.0);
        let page_area_width = self.width as f32;
        if content_height > page_area_height && content_height > 0.0 {
            self.draw_scrollbar(scroll_y, content_height, page_area_height, y_offset);
        }

        // Draw horizontal scrollbar if content is wider than the viewport.
        if page_area_width > 0.0 {
            let content_w = self.compute_content_width_from_ops(commands);
            if content_w > page_area_width {
                self.draw_horizontal_scrollbar(scroll_x, content_w, page_area_width, y_offset + page_area_height);
            }
        }
    }

    /// Compute content width from render ops (max x + width).
    fn compute_content_width_from_ops(&self, commands: &RenderCommands) -> f32 {
        let mut max_x: f32 = 0.0;
        for op in &commands.ops {
            let right = match op {
                RenderOp::FillRect { x, width, .. } => x + width,
                RenderOp::DrawText { x, text, font_size, .. } => x + text.len() as f32 * font_size * 0.6,
                RenderOp::StrokeRect { x, width, .. } => x + width,
                RenderOp::DrawImage { x, width, .. } => x + width,
                _ => 0.0,
            };
            if right > max_x { max_x = right; }
        }
        max_x
    }

    /// Draw a thin horizontal scrollbar at the bottom of the page area.
    fn draw_horizontal_scrollbar(
        &mut self,
        scroll_x: f32,
        content_width: f32,
        page_area_width: f32,
        bar_y: f32,
    ) {
        let bar_height: f32 = 8.0;
        let bar_y = bar_y - bar_height; // Draw just above the bottom edge.

        // Track.
        let track_color = Color::rgba(0.85, 0.85, 0.85, 0.6);
        self.fill_rect(0.0, bar_y, page_area_width, bar_height, track_color);

        // Thumb.
        let thumb_width = (page_area_width / content_width * page_area_width).max(20.0);
        let max_scroll = (content_width - page_area_width).max(0.0);
        let scroll_ratio = if max_scroll > 0.0 { scroll_x / max_scroll } else { 0.0 };
        let thumb_x = scroll_ratio * (page_area_width - thumb_width);

        let thumb_color = Color::rgba(0.5, 0.5, 0.5, 0.7);
        self.fill_rect(thumb_x, bar_y, thumb_width, bar_height, thumb_color);
    }

    /// Draw a thin scrollbar on the right edge of the page area.
    ///
    /// Renders a semi-transparent gray track with a proportional thumb whose
    /// position reflects the current scroll offset. The scrollbar starts below
    /// `y_offset` (e.g. the URL bar) so it does not overlap browser chrome.
    fn draw_scrollbar(
        &mut self,
        scroll_y: f32,
        content_height: f32,
        page_area_height: f32,
        y_offset: f32,
    ) {
        let bar_width: f32 = 8.0;
        let bar_x = self.width as f32 - bar_width;

        // Track: light gray, semi-transparent. Starts below the URL bar.
        let track_color = Color::rgba(0.85, 0.85, 0.85, 0.6);
        self.fill_rect(bar_x, y_offset, bar_width, page_area_height, track_color);

        // Thumb: proportional size and position.
        let thumb_height = (page_area_height / content_height * page_area_height).max(20.0);
        let max_scroll = (content_height - page_area_height).max(0.0);
        let scroll_ratio = if max_scroll > 0.0 {
            scroll_y / max_scroll
        } else {
            0.0
        };
        let thumb_y = y_offset + scroll_ratio * (page_area_height - thumb_height);

        let thumb_color = Color::rgba(0.5, 0.5, 0.5, 0.7);
        self.fill_rect(bar_x, thumb_y, bar_width, thumb_height, thumb_color);
    }

    /// Blit a decoded RGBA image into the framebuffer, scaling with
    /// nearest-neighbour sampling from the source (`img_width` x `img_height`)
    /// to the destination rectangle (`dst_w` x `dst_h` at `dst_x, dst_y`).
    ///
    /// Respects the current clip stack — pixels outside the effective clip
    /// bounds are skipped.
    pub fn draw_image(
        &mut self,
        dst_x: f32,
        dst_y: f32,
        dst_w: f32,
        dst_h: f32,
        img_width: u32,
        img_height: u32,
        pixels: &[u8],
    ) {
        let expected_len = (img_width as usize) * (img_height as usize) * 4;
        if pixels.len() < expected_len || img_width == 0 || img_height == 0 {
            return;
        }

        let (clip_x0, clip_y0, clip_x1, clip_y1) = self.effective_clip();

        let dx0 = dst_x.round() as i32;
        let dy0 = dst_y.round() as i32;
        let dx1 = (dst_x + dst_w).round() as i32;
        let dy1 = (dst_y + dst_h).round() as i32;

        let dest_w = (dx1 - dx0).max(1) as f32;
        let dest_h = (dy1 - dy0).max(1) as f32;

        for py in dy0..dy1 {
            if py < 0 || py >= self.height as i32 {
                continue;
            }
            if py < clip_y0 || py >= clip_y1 {
                continue;
            }
            // Map destination y to source y (nearest-neighbour).
            let sy = (((py - dy0) as f32 / dest_h) * img_height as f32) as u32;
            let sy = sy.min(img_height - 1);

            for px in dx0..dx1 {
                if px < 0 || px >= self.width as i32 {
                    continue;
                }
                if px < clip_x0 || px >= clip_x1 {
                    continue;
                }
                // Map destination x to source x.
                let sx = (((px - dx0) as f32 / dest_w) * img_width as f32) as u32;
                let sx = sx.min(img_width - 1);

                let src_idx = ((sy * img_width + sx) * 4) as usize;
                let r = pixels[src_idx] as f32 / 255.0;
                let g = pixels[src_idx + 1] as f32 / 255.0;
                let b = pixels[src_idx + 2] as f32 / 255.0;
                let a = pixels[src_idx + 3] as f32 / 255.0;

                self.set_pixel(px, py, Color { r, g, b, a });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Built-in bitmap font (fallback)
// ---------------------------------------------------------------------------

/// Get a basic 8x16 bitmap glyph for a character.
/// This is a tiny built-in font covering ASCII printable range.
fn get_basic_glyph(ch: char) -> [u8; 16] {
    // Very basic bitmap font — just enough to see text on screen.
    // Each byte is a row of 8 pixels (MSB = leftmost).
    match ch {
        'A' => [0x00,0x18,0x3C,0x66,0x66,0x7E,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00],
        'B' => [0x00,0x7C,0x66,0x66,0x7C,0x66,0x66,0x66,0x7C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'C' => [0x00,0x3C,0x66,0x60,0x60,0x60,0x60,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'D' => [0x00,0x78,0x6C,0x66,0x66,0x66,0x66,0x6C,0x78,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'E' => [0x00,0x7E,0x60,0x60,0x7C,0x60,0x60,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'F' => [0x00,0x7E,0x60,0x60,0x7C,0x60,0x60,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'G' => [0x00,0x3C,0x66,0x60,0x60,0x6E,0x66,0x66,0x3E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'H' => [0x00,0x66,0x66,0x66,0x7E,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'I' => [0x00,0x3C,0x18,0x18,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'J' => [0x00,0x1E,0x0C,0x0C,0x0C,0x0C,0x0C,0x6C,0x38,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'K' => [0x00,0x66,0x6C,0x78,0x70,0x78,0x6C,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'L' => [0x00,0x60,0x60,0x60,0x60,0x60,0x60,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'M' => [0x00,0x63,0x77,0x7F,0x6B,0x63,0x63,0x63,0x63,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'N' => [0x00,0x66,0x76,0x7E,0x7E,0x6E,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'O' => [0x00,0x3C,0x66,0x66,0x66,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'P' => [0x00,0x7C,0x66,0x66,0x7C,0x60,0x60,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'Q' => [0x00,0x3C,0x66,0x66,0x66,0x66,0x6E,0x3C,0x0E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'R' => [0x00,0x7C,0x66,0x66,0x7C,0x78,0x6C,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'S' => [0x00,0x3C,0x66,0x60,0x3C,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'T' => [0x00,0x7E,0x18,0x18,0x18,0x18,0x18,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'U' => [0x00,0x66,0x66,0x66,0x66,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'V' => [0x00,0x66,0x66,0x66,0x66,0x66,0x3C,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'W' => [0x00,0x63,0x63,0x63,0x6B,0x7F,0x77,0x63,0x63,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'X' => [0x00,0x66,0x66,0x3C,0x18,0x3C,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'Y' => [0x00,0x66,0x66,0x66,0x3C,0x18,0x18,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'Z' => [0x00,0x7E,0x06,0x0C,0x18,0x30,0x60,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'a' => [0x00,0x00,0x00,0x3C,0x06,0x3E,0x66,0x66,0x3E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'b' => [0x00,0x60,0x60,0x7C,0x66,0x66,0x66,0x66,0x7C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'c' => [0x00,0x00,0x00,0x3C,0x66,0x60,0x60,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'd' => [0x00,0x06,0x06,0x3E,0x66,0x66,0x66,0x66,0x3E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'e' => [0x00,0x00,0x00,0x3C,0x66,0x7E,0x60,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'f' => [0x00,0x1C,0x36,0x30,0x7C,0x30,0x30,0x30,0x30,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'g' => [0x00,0x00,0x00,0x3E,0x66,0x66,0x66,0x3E,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00],
        'h' => [0x00,0x60,0x60,0x7C,0x66,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'i' => [0x00,0x18,0x00,0x38,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'j' => [0x00,0x06,0x00,0x06,0x06,0x06,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00],
        'k' => [0x00,0x60,0x60,0x66,0x6C,0x78,0x6C,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'l' => [0x00,0x38,0x18,0x18,0x18,0x18,0x18,0x18,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'm' => [0x00,0x00,0x00,0x66,0x7F,0x7F,0x6B,0x63,0x63,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'n' => [0x00,0x00,0x00,0x7C,0x66,0x66,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'o' => [0x00,0x00,0x00,0x3C,0x66,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'p' => [0x00,0x00,0x00,0x7C,0x66,0x66,0x66,0x7C,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00],
        'q' => [0x00,0x00,0x00,0x3E,0x66,0x66,0x66,0x3E,0x06,0x06,0x00,0x00,0x00,0x00,0x00,0x00],
        'r' => [0x00,0x00,0x00,0x7C,0x66,0x60,0x60,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        's' => [0x00,0x00,0x00,0x3E,0x60,0x3C,0x06,0x06,0x7C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        't' => [0x00,0x30,0x30,0x7C,0x30,0x30,0x30,0x36,0x1C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'u' => [0x00,0x00,0x00,0x66,0x66,0x66,0x66,0x66,0x3E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'v' => [0x00,0x00,0x00,0x66,0x66,0x66,0x3C,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'w' => [0x00,0x00,0x00,0x63,0x6B,0x7F,0x7F,0x36,0x36,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'x' => [0x00,0x00,0x00,0x66,0x3C,0x18,0x3C,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        'y' => [0x00,0x00,0x00,0x66,0x66,0x66,0x3E,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00],
        'z' => [0x00,0x00,0x00,0x7E,0x0C,0x18,0x30,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '0' => [0x00,0x3C,0x66,0x6E,0x76,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '1' => [0x00,0x18,0x38,0x18,0x18,0x18,0x18,0x18,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '2' => [0x00,0x3C,0x66,0x06,0x0C,0x18,0x30,0x60,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '3' => [0x00,0x3C,0x66,0x06,0x1C,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '4' => [0x00,0x0C,0x1C,0x3C,0x6C,0x7E,0x0C,0x0C,0x0C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '5' => [0x00,0x7E,0x60,0x7C,0x06,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '6' => [0x00,0x3C,0x66,0x60,0x7C,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '7' => [0x00,0x7E,0x06,0x0C,0x18,0x18,0x18,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '8' => [0x00,0x3C,0x66,0x66,0x3C,0x66,0x66,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '9' => [0x00,0x3C,0x66,0x66,0x3E,0x06,0x06,0x66,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '.' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        ',' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x18,0x18,0x30,0x00,0x00,0x00,0x00,0x00,0x00],
        ':' => [0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        ';' => [0x00,0x00,0x00,0x18,0x18,0x00,0x00,0x18,0x18,0x30,0x00,0x00,0x00,0x00,0x00,0x00],
        '!' => [0x00,0x18,0x18,0x18,0x18,0x18,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '?' => [0x00,0x3C,0x66,0x06,0x0C,0x18,0x00,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '/' => [0x00,0x06,0x06,0x0C,0x18,0x30,0x60,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '-' => [0x00,0x00,0x00,0x00,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '_' => [0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '(' => [0x00,0x0C,0x18,0x30,0x30,0x30,0x30,0x18,0x0C,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        ')' => [0x00,0x30,0x18,0x0C,0x0C,0x0C,0x0C,0x18,0x30,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '<' => [0x00,0x06,0x0C,0x18,0x30,0x18,0x0C,0x06,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '>' => [0x00,0x60,0x30,0x18,0x0C,0x18,0x30,0x60,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '=' => [0x00,0x00,0x00,0x7E,0x00,0x7E,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '+' => [0x00,0x00,0x18,0x18,0x7E,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '"' => [0x00,0x66,0x66,0x66,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        '\'' => [0x00,0x18,0x18,0x18,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
        // Default: small filled rectangle for unknown chars.
        _ => [0x00,0x00,0x3C,0x3C,0x3C,0x3C,0x3C,0x3C,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00],
    }
}
