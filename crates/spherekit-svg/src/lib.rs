//! # spherekit-svg
//!
//! SVG asset loading for interface graphics.
//!
//! ## Scope
//!
//! This is **not** a browser-grade SVG renderer and does not try to be. It
//! targets the assets an interface actually uses — icons, logos, decorative
//! shapes — and covers:
//!
//! * paths, and the `rect` / `circle` / `ellipse` / `line` / `polyline` /
//!   `polygon` shapes, all of which `usvg` resolves to paths for us
//! * fills and strokes, including caps, joins, miter limit and dash patterns
//! * linear and radial gradients
//! * `viewBox`, nested groups and their transforms
//! * group and per-shape opacity
//! * `use`, `defs` and `symbol`, resolved during parsing
//!
//! Deliberately out of scope, each reported as
//! [`SvgError::Unsupported`](spherekit_core::SvgError::Unsupported) rather than
//! silently dropped: filters, masks, patterns, `<text>`, animation and
//! `foreignObject`. An icon set that needs any of those is better exported flat.
//!
//! ## Why the cache is the point
//!
//! An icon is drawn every frame, at a handful of sizes, in a UI that may have
//! two hundred of them on screen. Parsing is expensive and tessellation is
//! expensive; doing either per frame would dominate the frame budget.
//!
//! So [`SvgCache`] does both once. Parsing is keyed by content hash, so loading
//! the same bytes twice returns the same [`SvgId`]. Tessellation is keyed by
//! `(id, destination size, scale factor)`, because the flattening tolerance
//! depends on how large the icon actually ends up — a 16 px icon and a 64 px
//! icon genuinely need different geometry, and reusing one for the other is
//! either faceted or wasteful.
//!
//! ## Tinting
//!
//! Interface icons are usually monochrome and coloured by the theme, so
//! [`SvgCache::render`] takes an optional tint that replaces every paint in the
//! document. That makes recolouring a one-argument operation rather than a
//! reason to ship one file per colour.
//!
//! ```no_run
//! use spherekit_svg::SvgCache;
//! use spherekit_core::{Color, px, rect};
//!
//! # fn demo(canvas: &mut spherekit_render::Canvas<'_>) -> Result<(), spherekit_core::SvgError> {
//! let mut cache = SvgCache::new();
//! let icon = cache.load_str(r#"<svg viewBox="0 0 16 16"><path d="M2 8h12"/></svg>"#)?;
//! cache.render(icon, canvas, rect(px(0.0), px(0.0), px(16.0), px(16.0)), Some(Color::WHITE));
//! # Ok(())
//! # }
//! ```
//!
//! No `usvg` type appears in this crate's public API.

#![deny(missing_docs)]

pub mod cache;
pub mod document;

pub use cache::{SvgCache, SvgCacheStats};
pub use document::{SvgDocument, SvgShape};

pub use spherekit_core::{SvgError, SvgId};
