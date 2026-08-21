//! # SphereGraphicEngine
//!
//! A GPU-first graphics and UI engine written in Rust for realtime creative
//! applications: digital audio workstations, audio plug-ins, creative tools and
//! visualisation.
//!
//! This crate is a facade. Everything it exposes lives in a focused crate that
//! can be used on its own, and the layering is deliberate — the graphics engine
//! has no idea the UI layer exists, and the UI layer has no idea which backend
//! is drawing it.
//!
//! ```text
//! Application
//!     -> sphere-ui         elements, events, focus, widgets
//!     -> sphere-layout     retained nodes, style, dirty flags, hit testing
//!     -> sphere-render     canvas, scene, culling, batching
//!     -> sphere-wgpu       the only crate that knows wgpu exists
//!     -> D3D12 / Vulkan / Metal / WebGPU
//! ```
//!
//! ## The short path
//!
//! [`SphereSurface`] wires one window's renderer, UI tree, text system and
//! image cache together:
//!
//! ```ignore
//! let mut surface = SphereSurface::new(window, size, scale, Default::default()).await?;
//!
//! // ...per frame:
//! surface.render(
//!     div().flex_col().p(px(16.0)).child(label("Threshold")).into_element(),
//!     Color::hex(0x14161A),
//! )?;
//! ```
//!
//! ## The long path
//!
//! Nothing above is required. The canvas stands alone with no nodes, no layout
//! and no widgets:
//!
//! ```
//! use sphere::render::{Canvas, Scene};
//! use sphere::core::{Color, ScaleFactor, px, rect, size};
//!
//! let mut scene = Scene::new(size(px(400.0), px(300.0)), ScaleFactor::IDENTITY);
//! {
//!     let mut canvas = Canvas::new(&mut scene);
//!     canvas.fill_rect(rect(px(8.0), px(8.0), px(120.0), px(32.0)), Color::hex(0x1E88E5));
//! }
//! assert_eq!(scene.len(), 1);
//! ```
//!
//! ## What this engine is for
//!
//! The design decisions that distinguish it are all downstream of one workload:
//! software that must stay responsive while something else is already using the
//! machine hard.
//!
//! * **Paint-only updates cost no layout.** A meter repainting sixty times a
//!   second marks `PAINT`; nothing relayouts and no text reshapes.
//! * **Text is a distance field.** Glyphs are rasterised once and sampled at any
//!   size, so zooming a panel re-rasterises nothing.
//! * **Realtime audio data never renders from the audio thread.** The boundary
//!   is a lock-free snapshot or ring; see [`audio`].
//! * **Colour is linear.** Blending and gradient interpolation happen in linear
//!   light, so translucent overlaps and filtered glyph edges come out clean.

#![deny(missing_docs)]

pub mod surface;

pub use surface::{SphereSurface, SurfaceOptions, SurfaceStats};

/// Realtime audio visualisation and the lock-free audio-thread boundary.
pub use sphere_audio_ui as audio;
/// Units, geometry, transforms, colour, paths, paint, identity and errors.
pub use sphere_core as core;
/// Image decoding and the texture cache.
pub use sphere_image as image;
/// The retained layout tree, style and dirty propagation.
pub use sphere_layout as layout;
/// Windows, input, monitors and frame scheduling.
pub use sphere_platform as platform;
/// Canvas, scene, culling, batching and the backend seam.
pub use sphere_render as render;
/// SVG asset loading.
pub use sphere_svg as svg;
/// Font discovery, shaping, layout and MTSDF glyph rendering.
pub use sphere_text as text;
/// Elements, events, focus, theming and widgets.
pub use sphere_ui as ui;
/// The wgpu rendering backend.
pub use sphere_wgpu as wgpu_backend;

/// Everything a typical application needs, in one import.
pub mod prelude {
    pub use crate::{SphereSurface, SurfaceOptions};
    pub use sphere_audio_ui::prelude::*;
    pub use sphere_core::prelude::*;
    pub use sphere_layout::prelude::*;
    pub use sphere_render::prelude::*;
    pub use sphere_ui::prelude::*;
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_facade_re_exports_every_layer() {
        // A compile-time check that the layering is reachable from one import.
        let _ = crate::core::Color::BLACK;
        let _ = crate::render::Scene::default();
        let _ = crate::layout::Style::DEFAULT;
        let _: crate::ui::Theme = crate::ui::Theme::dark();
        let _ = crate::audio::MeterScale::default();
        let _ = crate::text::TextStyle::default();
    }
}
