//! # sphere-text
//!
//! The text stack: font discovery, complex-script shaping, line layout, and
//! GPU glyph rasterisation via multi-channel signed distance fields.
//!
//! ## Why MTSDF
//!
//! A conventional glyph cache stores one bitmap per (glyph, size) pair. At the
//! sizes a DAW mixes — 10 px labels next to 24 px headings, all of it zoomable
//! and all of it on displays between 100 % and 200 % scaling — that cache
//! multiplies out fast and re-rasterises on every zoom step.
//!
//! A distance field is stored once and sampled at any size. The multi-channel
//! form additionally preserves sharp corners, which single-channel SDF rounds
//! off, and the extra true-distance channel (the "T" in MTSDF) gives outlines,
//! glows and shadows for free without a second rasterisation.
//!
//! ## The small-text exception
//!
//! Below roughly 12 device pixels a distance field runs out of resolution
//! before a glyph runs out of detail, and stems start to shimmer. Sphere keeps
//! a grayscale bitmap fallback for exactly that range, selected per glyph by
//! [`sphere_render::TextRasterMode::Auto`]. It is deliberately isolated: the
//! fallback exists to make 11 px labels crisp, not to become the main path.
//!
//! ## Pipeline
//!
//! ```text
//! text + style
//!    -> bidi resolution + script itemisation   (shape)
//!    -> per-run shaping with fallback          (shape, font)
//!    -> line breaking and alignment            (layout)
//!    -> glyph rasterisation, MTSDF or bitmap   (mtsdf, raster)
//!    -> paged atlas placement                  (atlas)
//!    -> instanced draw                         (sphere-wgpu)
//! ```

#![deny(missing_docs)]

pub mod atlas;
pub mod cache;
pub mod caret;
pub mod font;
pub mod layout;
pub mod mtsdf;
pub mod raster;
pub mod shape;
pub mod system;
pub mod system_ui;
pub mod types;

pub use system::{TextSystem, TextSystemStats};
pub use types::{
    AtlasPlacement, FontMetrics, FontRequest, FontStretch, FontStyle, FontWeight, GlyphFormat,
    GlyphImage, GlyphKey, GlyphMetrics, Overflow, ScaledFontMetrics, ShapedGlyph, ShapedRun,
    TextAlign, TextDirection, TextLayout, TextLine, TextStyle, VariationAxis, WrapMode,
};
