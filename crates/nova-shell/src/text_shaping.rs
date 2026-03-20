//! Text shaping using rustybuzz (pure-Rust HarfBuzz port).
//!
//! Provides proper text shaping with ligatures, kerning, BiDi support, and
//! complex script handling. The [`TextShaper`] struct wraps a `rustybuzz::Face`
//! and exposes methods for shaping text runs, measuring widths, and parsing
//! CSS `font-feature-settings`.
//!
//! The shaping pipeline:
//! 1. **rustybuzz** shapes the text and produces glyph IDs + positions.
//! 2. **fontdue** rasterizes each glyph (by character fallback).
//! 3. Glyphs are placed at the shaped positions for correct kerning/ligatures.

use std::sync::Arc;

use rustybuzz::{script, Direction, Feature, GlyphBuffer, Script, UnicodeBuffer};

// ---------------------------------------------------------------------------
// ShapedGlyph / ShapedRun
// ---------------------------------------------------------------------------

/// A single glyph after text shaping.
///
/// Contains the glyph identifier and its positioning offsets as determined by
/// the rustybuzz shaping engine. All position values are in pixels (already
/// scaled from font units).
#[derive(Debug, Clone)]
pub struct ShapedGlyph {
    /// The glyph ID in the font (guaranteed <= `u16::MAX` by rustybuzz).
    pub glyph_id: u16,
    /// Horizontal advance — how far the cursor moves after this glyph.
    pub x_advance: f32,
    /// Vertical advance (usually 0 for horizontal text).
    pub y_advance: f32,
    /// Horizontal offset — shift before drawing (does not affect advance).
    pub x_offset: f32,
    /// Vertical offset — shift before drawing (does not affect advance).
    pub y_offset: f32,
    /// Index into the original character cluster this glyph belongs to.
    pub cluster: u32,
}

/// The result of shaping a text run.
///
/// Contains the sequence of shaped glyphs and the total advance width of the
/// run in pixels.
#[derive(Debug, Clone)]
pub struct ShapedRun {
    /// The shaped glyphs in visual order.
    pub glyphs: Vec<ShapedGlyph>,
    /// Total advance width of the entire run, in pixels.
    pub total_width: f32,
}

// ---------------------------------------------------------------------------
// TextShaper
// ---------------------------------------------------------------------------

/// Text shaper backed by rustybuzz.
///
/// Holds font data (as owned bytes behind an `Arc`) and the parsed
/// `rustybuzz::Face`. Because `Face` borrows from the byte slice, the bytes
/// are stored in an `Arc<Vec<u8>>` that outlives the face via a `'static`
/// trick (the face is re-created from a stable pointer each time shaping is
/// requested, or we use the owned-data pattern).
///
/// For simplicity and safety, we store the font bytes and re-parse on
/// construction only. The `Face` is stored alongside the data using a
/// self-referential-safe approach: we leak the `Arc` to get a `'static`
/// reference, and prevent the data from being freed while the shaper lives.
pub struct TextShaper {
    /// Owned font data for the regular face.
    _font_data: Arc<Vec<u8>>,
    /// Raw pointer to the font bytes, valid for as long as `_font_data` lives.
    font_data_ptr: &'static [u8],
    /// Optional bold face data.
    _bold_data: Option<Arc<Vec<u8>>>,
    bold_ptr: Option<&'static [u8]>,
    /// Optional italic face data.
    _italic_data: Option<Arc<Vec<u8>>>,
    italic_ptr: Option<&'static [u8]>,
    /// Custom @font-face data, keyed by lowercased family name.
    custom_data: Vec<(String, Arc<Vec<u8>>, &'static [u8])>,
}

/// Leak an `Arc<Vec<u8>>` to obtain a `'static` reference to its contents.
///
/// # Safety
/// The caller must ensure the `Arc` is kept alive for at least as long as the
/// returned reference is used. We enforce this by storing the `Arc` in the
/// `TextShaper` struct.
fn leak_arc(data: &Arc<Vec<u8>>) -> &'static [u8] {
    let ptr = data.as_slice() as *const [u8];
    // SAFETY: The Arc is stored in the TextShaper struct and will not be
    // dropped while the TextShaper is alive. The pointer remains valid.
    unsafe { &*ptr }
}

impl TextShaper {
    /// Create a new `TextShaper` from raw font bytes (TTF/OTF).
    ///
    /// Returns `None` if the font data cannot be parsed by rustybuzz.
    pub fn new(font_bytes: &[u8]) -> Option<Self> {
        let data = Arc::new(font_bytes.to_vec());
        let ptr = leak_arc(&data);

        // Validate that rustybuzz can parse this font.
        let face = rustybuzz::Face::from_slice(ptr, 0)?;
        drop(face);

        tracing::debug!(
            "TextShaper initialized with {} bytes of font data",
            font_bytes.len()
        );

        Some(Self {
            _font_data: data,
            font_data_ptr: ptr,
            _bold_data: None,
            bold_ptr: None,
            _italic_data: None,
            italic_ptr: None,
            custom_data: Vec::new(),
        })
    }

    /// Load a bold font variant.
    pub fn set_bold(&mut self, font_bytes: &[u8]) {
        let data = Arc::new(font_bytes.to_vec());
        let ptr = leak_arc(&data);
        if rustybuzz::Face::from_slice(ptr, 0).is_some() {
            self._bold_data = Some(data);
            self.bold_ptr = Some(ptr);
            tracing::debug!("TextShaper: bold variant loaded");
        } else {
            tracing::warn!("TextShaper: failed to parse bold font data");
        }
    }

    /// Load an italic font variant.
    pub fn set_italic(&mut self, font_bytes: &[u8]) {
        let data = Arc::new(font_bytes.to_vec());
        let ptr = leak_arc(&data);
        if rustybuzz::Face::from_slice(ptr, 0).is_some() {
            self._italic_data = Some(data);
            self.italic_ptr = Some(ptr);
            tracing::debug!("TextShaper: italic variant loaded");
        } else {
            tracing::warn!("TextShaper: failed to parse italic font data");
        }
    }

    /// Load a custom `@font-face` font.
    ///
    /// The font is stored under `family` (lowercased). If the data fails to
    /// parse it is silently skipped.
    pub fn load_custom_font(&mut self, family: &str, font_bytes: &[u8]) {
        let key = family.to_lowercase();
        if self.custom_data.iter().any(|(k, _, _)| k == &key) {
            return; // already loaded
        }
        let data = Arc::new(font_bytes.to_vec());
        let ptr = leak_arc(&data);
        if rustybuzz::Face::from_slice(ptr, 0).is_some() {
            tracing::info!(family = %family, "TextShaper: custom font loaded");
            self.custom_data.push((key, data, ptr));
        } else {
            tracing::warn!(family = %family, "TextShaper: failed to parse custom font data");
        }
    }

    /// Get the font data pointer for a given family (lowercased), or the
    /// default regular font.
    fn font_ptr_for(&self, family: Option<&str>) -> &'static [u8] {
        if let Some(fam) = family {
            let key = fam.to_lowercase();
            for (k, _, ptr) in &self.custom_data {
                if k == &key {
                    return ptr;
                }
            }
        }
        self.font_data_ptr
    }

    /// Get the font data pointer for a given variant.
    fn font_ptr_for_variant(&self, is_bold: bool, is_italic: bool) -> &'static [u8] {
        if is_bold {
            if let Some(ptr) = self.bold_ptr {
                return ptr;
            }
        }
        if is_italic {
            if let Some(ptr) = self.italic_ptr {
                return ptr;
            }
        }
        self.font_data_ptr
    }

    /// Shape a text string and return positioned glyphs.
    ///
    /// `font_size` is in pixels. `features` is a list of OpenType feature tag
    /// strings (e.g. `["kern", "liga"]`). An empty slice uses the font's
    /// default features.
    pub fn shape(&self, text: &str, font_size: f32, features: &[&str]) -> ShapedRun {
        self.shape_with_font(text, font_size, features, self.font_data_ptr)
    }

    /// Shape text using a specific font data pointer.
    fn shape_with_font(
        &self,
        text: &str,
        font_size: f32,
        features: &[&str],
        font_ptr: &'static [u8],
    ) -> ShapedRun {
        let face = match rustybuzz::Face::from_slice(font_ptr, 0) {
            Some(f) => f,
            None => return ShapedRun { glyphs: Vec::new(), total_width: 0.0 },
        };

        let upem = face.units_per_em() as f32;
        let scale = if upem > 0.0 { font_size / upem } else { 1.0 };

        // Build the unicode buffer.
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(text);

        // Detect script and direction.
        let detected_script = detect_script(text);
        buffer.set_script(detected_script);

        if is_rtl_script(detected_script) {
            buffer.set_direction(Direction::RightToLeft);
        } else {
            buffer.set_direction(Direction::LeftToRight);
        }

        // Parse feature tags.
        let parsed_features: Vec<Feature> = features
            .iter()
            .filter_map(|f| f.parse::<Feature>().ok())
            .collect();

        // Shape!
        let output: GlyphBuffer = rustybuzz::shape(&face, &parsed_features, buffer);

        let infos = output.glyph_infos();
        let positions = output.glyph_positions();

        let mut glyphs = Vec::with_capacity(infos.len());
        let mut total_width: f32 = 0.0;

        for (info, pos) in infos.iter().zip(positions.iter()) {
            let glyph = ShapedGlyph {
                glyph_id: info.glyph_id as u16,
                x_advance: pos.x_advance as f32 * scale,
                y_advance: pos.y_advance as f32 * scale,
                x_offset: pos.x_offset as f32 * scale,
                y_offset: pos.y_offset as f32 * scale,
                cluster: info.cluster,
            };
            total_width += glyph.x_advance;
            glyphs.push(glyph);
        }

        // For RTL scripts, reverse glyph order for display.
        if is_rtl_script(detected_script) {
            glyphs.reverse();
        }

        ShapedRun { glyphs, total_width }
    }

    /// Shape text with a specific variant (bold/italic).
    pub fn shape_variant(
        &self,
        text: &str,
        font_size: f32,
        features: &[&str],
        is_bold: bool,
        is_italic: bool,
    ) -> ShapedRun {
        let ptr = self.font_ptr_for_variant(is_bold, is_italic);
        self.shape_with_font(text, font_size, features, ptr)
    }

    /// Shape text using a custom font family.
    pub fn shape_custom(
        &self,
        text: &str,
        font_size: f32,
        features: &[&str],
        family: &str,
    ) -> ShapedRun {
        let ptr = self.font_ptr_for(Some(family));
        self.shape_with_font(text, font_size, features, ptr)
    }

    /// Measure the width of text at a given font size using shaping.
    ///
    /// This is more accurate than fontdue's per-character `advance_width`
    /// because it accounts for kerning and ligatures.
    pub fn measure_width(&self, text: &str, font_size: f32) -> f32 {
        let run = self.shape(text, font_size, &[]);
        run.total_width
    }

    /// Measure width using a specific font variant.
    pub fn measure_width_variant(
        &self,
        text: &str,
        font_size: f32,
        is_bold: bool,
        is_italic: bool,
    ) -> f32 {
        let run = self.shape_variant(text, font_size, &[], is_bold, is_italic);
        run.total_width
    }

    /// Check if a custom font family has been loaded.
    pub fn has_custom_font(&self, family: &str) -> bool {
        let key = family.to_lowercase();
        self.custom_data.iter().any(|(k, _, _)| k == &key)
    }
}

// ---------------------------------------------------------------------------
// Script detection
// ---------------------------------------------------------------------------

/// Detect the dominant script of a text string.
///
/// Scans the text for the first character that belongs to a non-Common,
/// non-Inherited script and returns that script. Falls back to
/// `Script::LATIN` if no specific script is found.
pub fn detect_script(text: &str) -> Script {
    for ch in text.chars() {
        let s = char_to_script(ch);
        if s != script::COMMON && s != script::INHERITED && s != script::UNKNOWN {
            return s;
        }
    }
    script::LATIN
}

/// Map a character to its Unicode script using code block heuristics.
///
/// This is a simplified mapping that covers the most common scripts without
/// requiring a full Unicode database dependency.
fn char_to_script(ch: char) -> Script {
    let cp = ch as u32;
    match cp {
        // Basic Latin + Latin Extended
        0x0041..=0x024F => script::LATIN,
        // Latin Extended Additional
        0x1E00..=0x1EFF => script::LATIN,
        // Combining Diacritical Marks
        0x0300..=0x036F => script::INHERITED,
        // Greek and Coptic
        0x0370..=0x03FF => script::GREEK,
        // Cyrillic
        0x0400..=0x04FF => script::CYRILLIC,
        0x0500..=0x052F => script::CYRILLIC,
        // Armenian
        0x0530..=0x058F => script::ARMENIAN,
        // Hebrew
        0x0590..=0x05FF => script::HEBREW,
        // Arabic
        0x0600..=0x06FF => script::ARABIC,
        0x0750..=0x077F => script::ARABIC,
        0x08A0..=0x08FF => script::ARABIC,
        0xFB50..=0xFDFF => script::ARABIC,
        0xFE70..=0xFEFF => script::ARABIC,
        // Devanagari
        0x0900..=0x097F => script::DEVANAGARI,
        // Bengali
        0x0980..=0x09FF => script::BENGALI,
        // Gurmukhi
        0x0A00..=0x0A7F => script::GURMUKHI,
        // Gujarati
        0x0A80..=0x0AFF => script::GUJARATI,
        // Tamil
        0x0B80..=0x0BFF => script::TAMIL,
        // Telugu
        0x0C00..=0x0C7F => script::TELUGU,
        // Kannada
        0x0C80..=0x0CFF => script::KANNADA,
        // Malayalam
        0x0D00..=0x0D7F => script::MALAYALAM,
        // Thai
        0x0E00..=0x0E7F => script::THAI,
        // Lao
        0x0E80..=0x0EFF => script::LAO,
        // Tibetan
        0x0F00..=0x0FFF => script::TIBETAN,
        // Georgian
        0x10A0..=0x10FF => script::GEORGIAN,
        // Hangul Jamo
        0x1100..=0x11FF => script::HANGUL,
        // Hangul Syllables
        0xAC00..=0xD7AF => script::HANGUL,
        // Hangul Jamo Extended
        0xA960..=0xA97F => script::HANGUL,
        0xD7B0..=0xD7FF => script::HANGUL,
        // CJK Unified Ideographs
        0x4E00..=0x9FFF => script::HAN,
        0x3400..=0x4DBF => script::HAN,
        0x20000..=0x2A6DF => script::HAN,
        0x2A700..=0x2B73F => script::HAN,
        // CJK Compatibility Ideographs
        0xF900..=0xFAFF => script::HAN,
        // Hiragana
        0x3040..=0x309F => script::HIRAGANA,
        // Katakana
        0x30A0..=0x30FF => script::KATAKANA,
        0x31F0..=0x31FF => script::KATAKANA,
        // Myanmar
        0x1000..=0x109F => script::MYANMAR,
        // Ethiopic
        0x1200..=0x137F => script::ETHIOPIC,
        // Khmer
        0x1780..=0x17FF => script::KHMER,
        // Sinhala
        0x0D80..=0x0DFF => script::SINHALA,
        // Common: digits, punctuation, symbols
        0x0000..=0x0040 => script::COMMON,
        0x2000..=0x206F => script::COMMON, // General Punctuation
        0x20A0..=0x20CF => script::COMMON, // Currency Symbols
        0x2100..=0x214F => script::COMMON, // Letterlike Symbols
        0x2190..=0x21FF => script::COMMON, // Arrows
        0x2200..=0x22FF => script::COMMON, // Math Operators
        0x2300..=0x23FF => script::COMMON, // Misc Technical
        0x25A0..=0x25FF => script::COMMON, // Geometric Shapes
        0x2600..=0x26FF => script::COMMON, // Misc Symbols
        0x3000..=0x303F => script::COMMON, // CJK Symbols and Punctuation
        0xFF00..=0xFFEF => script::COMMON, // Halfwidth and Fullwidth Forms
        _ => script::COMMON,
    }
}

/// Check whether a script uses right-to-left text direction.
pub fn is_rtl_script(s: Script) -> bool {
    s == script::ARABIC
        || s == script::HEBREW
        || s == script::SYRIAC
        || s == script::THAANA
        || s == script::NKO
        || s == script::MANDAIC
        || s == script::SAMARITAN
}

// ---------------------------------------------------------------------------
// CSS font-feature-settings parsing
// ---------------------------------------------------------------------------

/// Parse a CSS `font-feature-settings` value into rustybuzz `Feature` values.
///
/// Accepts the standard CSS syntax:
/// ```text
/// "liga" on, "kern" 1, "smcp" off
/// ```
///
/// Each quoted tag may be followed by `on`/`off` or a numeric value.
/// Tags without a value default to `on` (value 1).
pub fn parse_font_features(css: &str) -> Vec<Feature> {
    if css.trim().eq_ignore_ascii_case("normal") || css.trim().eq_ignore_ascii_case("initial") {
        return Vec::new();
    }

    let mut features = Vec::new();

    for part in css.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }

        // Extract the tag (between quotes).
        let tag_str = if let Some(start) = part.find('"') {
            let rest = &part[start + 1..];
            if let Some(end) = rest.find('"') {
                &rest[..end]
            } else {
                continue;
            }
        } else if let Some(start) = part.find('\'') {
            let rest = &part[start + 1..];
            if let Some(end) = rest.find('\'') {
                &rest[..end]
            } else {
                continue;
            }
        } else {
            // No quotes — try parsing the whole thing as a rustybuzz feature string.
            if let Ok(f) = part.parse::<Feature>() {
                features.push(f);
            }
            continue;
        };

        if tag_str.len() != 4 {
            tracing::warn!(tag = %tag_str, "font feature tag must be exactly 4 characters");
            continue;
        }

        // Parse the value after the closing quote.
        let after_tag = part
            .rsplit('"')
            .next()
            .or_else(|| part.rsplit('\'').next())
            .unwrap_or("")
            .trim();

        let value: u32 = if after_tag.is_empty() || after_tag.eq_ignore_ascii_case("on") {
            1
        } else if after_tag.eq_ignore_ascii_case("off") {
            0
        } else {
            after_tag.parse().unwrap_or(1)
        };

        let tag_bytes: [u8; 4] = [
            tag_str.as_bytes().get(0).copied().unwrap_or(b' '),
            tag_str.as_bytes().get(1).copied().unwrap_or(b' '),
            tag_str.as_bytes().get(2).copied().unwrap_or(b' '),
            tag_str.as_bytes().get(3).copied().unwrap_or(b' '),
        ];
        let tag = rustybuzz::ttf_parser::Tag::from_bytes(&tag_bytes);
        features.push(Feature::new(tag, value, ..));
    }

    features
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: try to load the regular DejaVu Sans font for testing.
    ///
    /// Tries system paths first (guaranteed real TTF), then the workspace
    /// assets folder.  Each candidate is validated with
    /// `rustybuzz::Face::from_slice` to avoid picking up placeholder files.
    fn test_font_bytes() -> Option<Vec<u8>> {
        let paths = &[
            // Common system paths (real TTF files).
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
            "/usr/share/fonts/noto/NotoSans-Regular.ttf",
            "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
            // Workspace assets (may be LFS placeholders).
            concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/fonts/DejaVuSans.ttf"),
        ];
        for path in paths {
            if let Ok(bytes) = std::fs::read(path) {
                // Validate that this is a real font file.
                if rustybuzz::Face::from_slice(&bytes, 0).is_some() {
                    return Some(bytes);
                }
            }
        }
        None
    }

    #[test]
    fn test_basic_shaping() {
        let bytes = match test_font_bytes() {
            Some(b) => b,
            None => {
                eprintln!("skipping test_basic_shaping: no font available");
                return;
            }
        };
        let shaper = TextShaper::new(&bytes).expect("should parse font");
        let run = shaper.shape("Hello", 16.0, &[]);
        assert!(!run.glyphs.is_empty(), "should produce glyphs");
        assert!(run.total_width > 0.0, "total width should be positive");
        // "Hello" has 5 characters, should produce at least 5 glyphs (unless ligatures).
        assert!(run.glyphs.len() >= 4, "expected at least 4 glyphs for 'Hello'");
    }

    #[test]
    fn test_width_measurement() {
        let bytes = match test_font_bytes() {
            Some(b) => b,
            None => {
                eprintln!("skipping test_width_measurement: no font available");
                return;
            }
        };
        let shaper = TextShaper::new(&bytes).expect("should parse font");
        let w1 = shaper.measure_width("Hello", 16.0);
        let w2 = shaper.measure_width("Hello World", 16.0);
        assert!(w1 > 0.0);
        assert!(w2 > w1, "longer text should be wider");
    }

    #[test]
    fn test_empty_text() {
        let bytes = match test_font_bytes() {
            Some(b) => b,
            None => {
                eprintln!("skipping test_empty_text: no font available");
                return;
            }
        };
        let shaper = TextShaper::new(&bytes).expect("should parse font");
        let run = shaper.shape("", 16.0, &[]);
        assert!(run.glyphs.is_empty());
        assert_eq!(run.total_width, 0.0);
    }

    #[test]
    fn test_script_detection_latin() {
        let s = detect_script("Hello world");
        assert_eq!(s, script::LATIN);
    }

    #[test]
    fn test_script_detection_arabic() {
        let s = detect_script("\u{0627}\u{0644}\u{0639}\u{0631}\u{0628}\u{064A}\u{0629}");
        assert_eq!(s, script::ARABIC);
    }

    #[test]
    fn test_script_detection_hebrew() {
        let s = detect_script("\u{05E9}\u{05DC}\u{05D5}\u{05DD}");
        assert_eq!(s, script::HEBREW);
    }

    #[test]
    fn test_script_detection_devanagari() {
        let s = detect_script("\u{0928}\u{092E}\u{0938}\u{094D}\u{0924}\u{0947}");
        assert_eq!(s, script::DEVANAGARI);
    }

    #[test]
    fn test_script_detection_cjk() {
        let s = detect_script("\u{4F60}\u{597D}"); // Chinese "ni hao"
        assert_eq!(s, script::HAN);
    }

    #[test]
    fn test_script_detection_cyrillic() {
        let s = detect_script("\u{041F}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}");
        assert_eq!(s, script::CYRILLIC);
    }

    #[test]
    fn test_script_detection_common_fallback() {
        // Digits and punctuation only → should fall back to Latin.
        let s = detect_script("123 !!!");
        assert_eq!(s, script::LATIN);
    }

    #[test]
    fn test_rtl_detection() {
        assert!(is_rtl_script(script::ARABIC));
        assert!(is_rtl_script(script::HEBREW));
        assert!(!is_rtl_script(script::LATIN));
        assert!(!is_rtl_script(script::HAN));
        assert!(!is_rtl_script(script::CYRILLIC));
    }

    #[test]
    fn test_font_feature_parsing_basic() {
        let features = parse_font_features(r#""liga" on, "kern" 1"#);
        assert_eq!(features.len(), 2);
        assert_eq!(features[0].value, 1);
        assert_eq!(features[1].value, 1);
    }

    #[test]
    fn test_font_feature_parsing_off() {
        let features = parse_font_features(r#""liga" off"#);
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].value, 0);
    }

    #[test]
    fn test_font_feature_parsing_normal() {
        let features = parse_font_features("normal");
        assert!(features.is_empty());
    }

    #[test]
    fn test_font_feature_parsing_numeric() {
        let features = parse_font_features(r#""smcp" 2"#);
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].value, 2);
    }

    #[test]
    fn test_rtl_shaping() {
        let bytes = match test_font_bytes() {
            Some(b) => b,
            None => {
                eprintln!("skipping test_rtl_shaping: no font available");
                return;
            }
        };
        let shaper = TextShaper::new(&bytes).expect("should parse font");
        // Arabic text — should produce glyphs and have positive width.
        let run = shaper.shape("\u{0645}\u{0631}\u{062D}\u{0628}\u{0627}", 16.0, &[]);
        assert!(run.total_width > 0.0, "RTL text should have positive width");
    }

    #[test]
    fn test_shaping_with_features() {
        let bytes = match test_font_bytes() {
            Some(b) => b,
            None => {
                eprintln!("skipping test_shaping_with_features: no font available");
                return;
            }
        };
        let shaper = TextShaper::new(&bytes).expect("should parse font");
        // Shape with explicit kern feature.
        let run = shaper.shape("AVATAR", 16.0, &["kern"]);
        assert!(!run.glyphs.is_empty());
        assert!(run.total_width > 0.0);
    }

    #[test]
    fn test_invalid_font_data() {
        let result = TextShaper::new(b"not a font");
        assert!(result.is_none(), "garbage data should fail gracefully");
    }

    #[test]
    fn test_font_size_scaling() {
        let bytes = match test_font_bytes() {
            Some(b) => b,
            None => {
                eprintln!("skipping test_font_size_scaling: no font available");
                return;
            }
        };
        let shaper = TextShaper::new(&bytes).expect("should parse font");
        let w16 = shaper.measure_width("Test", 16.0);
        let w32 = shaper.measure_width("Test", 32.0);
        // Width at 32px should be roughly 2x the width at 16px.
        let ratio = w32 / w16;
        assert!(
            (1.8..=2.2).contains(&ratio),
            "expected ~2x scaling, got ratio={ratio}"
        );
    }

    #[test]
    fn test_custom_font_loading() {
        let bytes = match test_font_bytes() {
            Some(b) => b,
            None => {
                eprintln!("skipping test_custom_font_loading: no font available");
                return;
            }
        };
        let mut shaper = TextShaper::new(&bytes).expect("should parse font");
        // Load the same font as a "custom" family.
        shaper.load_custom_font("TestFamily", &bytes);
        assert!(shaper.has_custom_font("TestFamily"));
        assert!(shaper.has_custom_font("testfamily")); // case-insensitive

        let run = shaper.shape_custom("Hello", 16.0, &[], "TestFamily");
        assert!(!run.glyphs.is_empty());
        assert!(run.total_width > 0.0);
    }
}
