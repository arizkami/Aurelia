//! # spherekit-wgpu
//!
//! The wgpu rendering backend for SphereKit.
//!
//! This is the only crate in the workspace that knows wgpu exists. It
//! implements [`spherekit_render::RendererBackend`], consuming the pure-data
//! [`spherekit_render::CompiledFrame`] the batch compiler produces.
//!
//! ## Backend mapping
//!
//! | Platform | API |
//! |----------|-----|
//! | Windows  | Direct3D 12 |
//! | Linux    | Vulkan |
//! | macOS    | Metal |
//! | Web      | WebGPU |
//!
//! wgpu picks among the enabled backends; [`WgpuRenderer::adapter_name`]
//! reports what was actually chosen, which is the first thing to check when a
//! frame looks wrong on one machine and right on another.
//!
//! ## Colour
//!
//! The surface is configured with an sRGB format wherever one is offered, so
//! the hardware performs the encode and blending happens in linear light.
//! Everything SphereKit hands the GPU is linear and premultiplied, which is what
//! makes translucent overlaps and filtered glyph edges come out clean rather
//! than dark-fringed.
//!
//! ## Shaders
//!
//! WGSL sources live in this crate's `shaders/` directory and are embedded at
//! compile time. A one-directive include system ([`shader`]) shares the SDF and
//! clip helpers between them without a preprocessor.

#![deny(missing_docs)]

pub mod buffer;
pub mod pipeline;
pub mod renderer;
pub mod shader;
pub mod texture;

pub use pipeline::{PipelineCache, PipelineKey, PipelineKind};
pub use renderer::WgpuRenderer;
pub use texture::TextureStore;

/// Re-exported so callers can name the concrete backend types when they need to
/// interoperate with other wgpu code, without taking their own wgpu dependency
/// and risking a version mismatch.
pub use wgpu;
