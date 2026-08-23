//! # SphereKit
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
//!     -> spherekit-ui         elements, events, focus, widgets
//!     -> spherekit-layout     retained nodes, style, dirty flags, hit testing
//!     -> spherekit-render     canvas, scene, culling, batching
//!     -> spherekit-wgpu       the only crate that knows wgpu exists
//!     -> D3D12 / Vulkan / Metal / WebGPU
//! ```
//!
//! An application written in TypeScript enters one layer higher up, and the
//! rest of the stack cannot tell the difference:
//!
//! ```text
//! React (TypeScript)
//!     -> spherekit-bridge     JSON Lines protocol, native methods, events out
//!     -> spherekit-react      commit validation, CSS cascade, lowering
//!     -> spherekit-ui         ...and down the same path as above
//! ```
//!
//! The `v8` feature adds `spherekit::js`, an in-process V8 host that drives
//! that protocol without a browser. It is off by default: a WebView or a child
//! process drives the same [`bridge`] and never compiles a JavaScript engine.
//!
//! ## The short path
//!
//! [`SphereKitSurface`] wires one window's renderer, UI tree, text system and
//! image cache together:
//!
//! ```ignore
//! let mut surface = SphereKitSurface::new(window, size, scale, Default::default()).await?;
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
//! use spherekit::render::{Canvas, Scene};
//! use spherekit::core::{Color, ScaleFactor, px, rect, size};
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

pub use surface::{InitTiming, SphereKitSurface, SurfaceOptions, SurfaceStats};

/// Realtime audio visualisation and the lock-free audio-thread boundary.
pub use spherekit_audio_ui as audio;
/// The JSON Lines protocol that connects a JavaScript runtime to the host.
pub use spherekit_bridge as bridge;
/// Units, geometry, transforms, colour, paths, paint, identity and errors.
pub use spherekit_core as core;
/// CSS parsing, selector cascade and native style application.
pub use spherekit_css as css;
/// Image decoding and the texture cache.
pub use spherekit_image as image;
/// The embedded V8 engine, behind the `v8` feature.
///
/// Gated because the V8 prebuilt is Windows x86_64 only and adds about 100 MB
/// to a checkout; nothing else in the facade depends on it, and the bridge
/// protocol is the seam a non-V8 host uses instead.
#[cfg(feature = "v8")]
pub use spherekit_jsengine as js;
/// The retained layout tree, style and dirty propagation.
pub use spherekit_layout as layout;
/// Windows, input, monitors and frame scheduling.
pub use spherekit_platform as platform;
/// The React commit host: validation, retention and lowering to elements.
pub use spherekit_react as react;
/// Canvas, scene, culling, batching and the backend seam.
pub use spherekit_render as render;
/// SVG asset loading.
pub use spherekit_svg as svg;
/// Font discovery, shaping, layout and MTSDF glyph rendering.
pub use spherekit_text as text;
/// Elements, events, focus, theming and widgets.
pub use spherekit_ui as ui;
/// The wgpu rendering backend.
pub use spherekit_wgpu as wgpu_backend;

/// Everything a typical application needs, in one import.
pub mod prelude {
    pub use crate::{SphereKitSurface, SurfaceOptions};
    pub use spherekit_audio_ui::prelude::*;
    #[cfg(feature = "v8")]
    pub use spherekit_bridge::JsBridge;
    pub use spherekit_bridge::{ApiBridge, BridgeError, BridgeMessage, PROTOCOL_VERSION};
    pub use spherekit_core::prelude::*;
    pub use spherekit_css::{Node as CssNode, ResolvedStyle, StyleContext, Stylesheet};
    pub use spherekit_layout::prelude::*;
    pub use spherekit_react::{EventQueue, HostEvent, NativeNode, NativeTree, ReactHost};
    pub use spherekit_render::prelude::*;
    pub use spherekit_ui::prelude::*;
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_facade_re_exports_every_layer() {
        // A compile-time check that the layering is reachable from one import.
        let _ = crate::core::Color::BLACK;
        let _ = crate::render::Scene::default();
        let _ = crate::layout::Style::DEFAULT;
        let _ = crate::css::Stylesheet::default();
        let _: crate::ui::Theme = crate::ui::Theme::dark();
        let _ = crate::audio::MeterScale::default();
        let _ = crate::text::TextStyle::default();
        let _ = crate::react::ReactHost::new();
        let _ = crate::bridge::ApiBridge::new();
        assert_eq!(crate::bridge::PROTOCOL_VERSION, 1);
    }

    #[test]
    fn a_react_commit_reaches_the_ui_layer_through_the_facade_alone() {
        // The point of the facade is that the TypeScript entry path needs no
        // more imports than the native one. If a lowering type ever stops being
        // reachable from `spherekit::` this fails to compile rather than
        // sending an application off to depend on the sub-crates directly.
        use crate::prelude::*;

        let mut bridge = ApiBridge::new();
        bridge
            .host_mut()
            .set_stylesheet("view { background: #101418; }")
            .expect("the stylesheet parses");
        bridge
            .commit_json(r#"{"revision":1,"children":[{"id":1,"type":"view"}]}"#)
            .expect("the commit is accepted");

        let host: &ReactHost = bridge.host();
        assert!(host.contains(1));
        assert_eq!(host.node_count(), 1);

        let queue = EventQueue::new();
        queue.push(HostEvent::new("press", 1));
        bridge.pump_events(&queue);
        assert!(matches!(
            bridge.take_outbound().as_slice(),
            [BridgeMessage::Event { node_id: Some(1), .. }]
        ));
    }

    #[cfg(feature = "v8")]
    #[test]
    fn the_v8_feature_reaches_the_engine_and_the_js_bridge() {
        // Asserted on both sides of the `cfg` rather than only where V8 exists:
        // the whole point of the stub engine is that a host crate compiles and
        // *runs* on a target without the prebuilt, refusing at the call instead
        // of failing to build. A test that only ran on Windows would never
        // notice that promise breaking.
        assert!(size_of::<crate::bridge::JsBridge>() > 0, "the JS host is reachable by name");
        if cfg!(windows) {
            assert!(!crate::js::Engine::version().is_empty());
            assert!(!crate::bridge::v8_version().is_empty());
        } else {
            assert_eq!(crate::js::Engine::new().err(), Some(crate::js::Error::UnsupportedPlatform));
        }
    }
}
