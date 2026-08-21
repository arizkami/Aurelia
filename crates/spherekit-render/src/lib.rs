//! # spherekit-render
//!
//! The rendering core: a canvas API, a flat display list, a batch compiler, and
//! the backend seam. No GPU dependency lives here — `spherekit-wgpu` implements
//! [`RendererBackend`], and everything above this crate is backend-agnostic.
//!
//! ## The frame
//!
//! ```text
//! Canvas calls
//!    -> Scene           flat, resolved display list; clips and transforms
//!                       already resolved to indices
//!    -> cull            device-space rejection before an instance is written
//!    -> batch           run-length merge of adjacent compatible commands
//!    -> CompiledFrame   instance buffers, a transform table, a clip table,
//!                       gradients, batches, passes
//!    -> RendererBackend
//! ```
//!
//! Nothing reorders across incompatible commands. UI is painted back to front,
//! so a global sort would batch better and draw wrong.
//!
//! ## Using it without the UI layer
//!
//! The graphics engine stands alone. Nothing here knows about nodes, layout or
//! widgets:
//!
//! ```
//! use spherekit_render::{BatchCompiler, Canvas, GlyphPlacement, GlyphProvider, GlyphRequest,
//!                     Scene, TextureProvider};
//! use spherekit_core::{Color, ImageId, ScaleFactor, TextureId, px, rect, size};
//!
//! # struct NoGlyphs;
//! # impl GlyphProvider for NoGlyphs {
//! #     fn place_glyph(&mut self, _: GlyphRequest) -> Option<GlyphPlacement> { None }
//! # }
//! # struct NoTextures;
//! # impl TextureProvider for NoTextures {
//! #     fn texture_for(&mut self, _: ImageId) -> Option<TextureId> { None }
//! # }
//! let mut scene = Scene::new(size(px(400.0), px(300.0)), ScaleFactor::IDENTITY);
//! {
//!     let mut canvas = Canvas::new(&mut scene);
//!     canvas.fill_rect(rect(px(8.0), px(8.0), px(120.0), px(32.0)), Color::hex(0x1E88E5));
//!     canvas.stroke_rect(rect(px(8.0), px(8.0), px(120.0), px(32.0)), Color::WHITE, px(1.0));
//! }
//!
//! let mut compiler = BatchCompiler::new();
//! let frame = compiler.compile(&scene, &mut NoGlyphs, &mut NoTextures);
//! assert_eq!(frame.quads.len(), 2);
//! assert_eq!(frame.batches.len(), 1); // both quads in one draw call
//! ```

#![deny(missing_docs)]

pub mod backend;
pub mod batch;
pub mod canvas;
pub mod primitives;
pub mod scene;
pub mod tessellate;

pub use backend::{
    BackendCapabilities, DEFAULT_MSAA_SAMPLES, FrameHandle, FrameStats, PresentPreference,
    RendererBackend, SceneCompiler, SurfaceConfig, VsyncMode,
};
pub use batch::{
    Batch, BatchCompiler, BatchKind, CompiledFrame, Composite, GlyphPlacement, GlyphProvider,
    GlyphRequest, Pass, RenderTarget, TextureProvider,
};
pub use canvas::Canvas;
pub use primitives::{
    FrameUniforms, GlyphInstance, GpuClip, GpuGradient, GpuTransform, MAX_GRADIENT_STOPS,
    QuadInstance, glyph_flags, gradient_kind, quad_flags,
};
pub use scene::{
    Clip, ClipKind, DrawCommand, FULL_COVERAGE_CONTRAST, Filter, GlyphRun, Layer, MAX_EDGE_BIAS_PX,
    MIN_EDGE_RAMP_PX, Mesh, MeshVertex, PositionedGlyph, QuadCommand, SMALL_TEXT_MAX_DEVICE_PX,
    Scene, SceneIndex, SceneStats, TextRasterMode, alpha_from_coverage, coverage_contrast_for,
    mtsdf_edge_ramp,
};
pub use tessellate::{TessellationError, TessellationOptions, Tessellator};

/// Everything a typical consumer needs, in one import.
pub mod prelude {
    pub use crate::backend::{PresentPreference, RendererBackend, SurfaceConfig, VsyncMode};
    pub use crate::batch::{BatchCompiler, CompiledFrame, GlyphProvider, TextureProvider};
    pub use crate::canvas::Canvas;
    pub use crate::scene::{Mesh, MeshVertex, Scene, TextRasterMode};
    pub use spherekit_core::prelude::*;
}
