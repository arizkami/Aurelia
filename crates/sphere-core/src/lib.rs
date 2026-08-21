//! # sphere-core
//!
//! The shared vocabulary of `SphereGraphicEngine`: units, geometry, transforms,
//! color, paths, paint, identity and errors.
//!
//! This crate has no GPU, windowing or text dependencies. Every other Sphere
//! crate depends on it, and nothing here depends on them, which is what keeps
//! the renderer swappable and the geometry testable without a device.
//!
//! ## Units
//!
//! Sphere is strict about the difference between logical and physical pixels.
//! [`Px`] is what application code writes; [`DevicePx`] is what the surface
//! sees. Converting between them requires a [`ScaleFactor`], and there is no
//! `impl From<Px> for DevicePx` that could do it silently.
//!
//! ```
//! use sphere_core::{px, ScaleFactor, DevicePx};
//!
//! let scale = ScaleFactor::new(1.5);
//! assert_eq!(scale.to_device(px(10.0)), DevicePx(15));
//! ```
//!
//! ## Color
//!
//! [`Color`] is sRGB-encoded with straight alpha; [`LinearColor`] is
//! linear-light with premultiplied alpha and is what reaches the GPU. Mixing
//! happens in linear space, so a black-to-white midpoint is the perceptually
//! correct value rather than a naive `0.5`.
//!
//! ```
//! use sphere_core::Color;
//!
//! let mid = Color::BLACK.lerp(Color::WHITE, 0.5);
//! assert!((mid.r - 0.735).abs() < 0.01);
//! ```
//!
//! ## Geometry
//!
//! [`Rect`] uses half-open containment so that adjacent rectangles tile without
//! double-hits, and [`Rect::intersection`] never produces a negative extent, so
//! folding a clip stack is always safe.

#![deny(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod color;
pub mod error;
pub mod geometry;
pub mod id;
pub mod paint;
pub mod path;
pub mod transform;
pub mod unit;

pub use color::{BlendMode, Color, Hsla, LinearColor, hsla, linear_to_srgb, srgb_to_linear};
pub use error::{
    FontError, ImageError, InitError, LayoutError, PlatformError, RenderError, ShaderError,
    SurfaceError, SvgError,
};
pub use geometry::{Corners, Edges, Point, Rect, RoundedRect, Size, point, rect, size};
pub use id::{
    ElementId, FocusId, FontId, GenerationalKey, GenerationalStore, GlyphId, ImageId, NodeId,
    PipelineId, SvgId, TextureId, ViewId, WindowId,
};
pub use paint::{
    Brush, Gradient, GradientStop, ImageFit, LineCap, LineJoin, Paint, Shadow, Stroke,
};
pub use path::{FillRule, Path, PathBuilder, PathEvent, Verb};
pub use transform::Affine;
pub use unit::{
    Deg, DevicePx, Length, Pt, Px, Rad, Scalar, ScaleFactor, percent, pt, px, relative,
};

/// Everything a typical consumer needs, in one import.
pub mod prelude {
    pub use crate::color::{BlendMode, Color, Hsla, LinearColor, hsla};
    pub use crate::geometry::{Corners, Edges, Point, Rect, RoundedRect, Size, point, rect, size};
    pub use crate::id::ElementId;
    pub use crate::paint::{Brush, Gradient, Paint, Shadow, Stroke};
    pub use crate::path::{FillRule, Path, PathBuilder};
    pub use crate::transform::Affine;
    pub use crate::unit::{Deg, DevicePx, Length, Px, Rad, ScaleFactor, percent, px, relative};
}
