//! The renderer backend seam.
//!
//! Everything above this line — canvas, scene, batching, layout, widgets —
//! is backend-agnostic. `sphere-wgpu` is the only implementation today, but the
//! seam exists so a native D3D12, Metal or software backend can be added
//! without redesigning anything above it.
//!
//! The trait is deliberately coarse. Dispatch happens once per frame and once
//! per pass, never per primitive, so `dyn` here costs nothing measurable while
//! keeping the hot loop monomorphic inside the backend.

use crate::batch::CompiledFrame;
use crate::scene::Scene;
use sphere_core::{Color, DevicePx, RenderError, ScaleFactor, Size, SurfaceError, TextureId};

/// How the presentation engine should trade latency against throughput.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum PresentPreference {
    /// Minimise the delay between a frame being drawn and being seen.
    ///
    /// The right default for plug-in editors and performance surfaces, where a
    /// knob that lags the mouse is immediately noticeable.
    LowLatency,
    /// Let the CPU and GPU pipeline a full refresh each.
    #[default]
    Balanced,
    /// Favour throughput and power over responsiveness.
    PowerSaving,
}

impl PresentPreference {
    /// The number of frames the presentation engine may keep in flight.
    #[inline]
    pub fn max_frame_latency(self) -> u32 {
        match self {
            PresentPreference::LowLatency => 1,
            PresentPreference::Balanced => 2,
            PresentPreference::PowerSaving => 3,
        }
    }
}

/// How a surface synchronises with the display.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum VsyncMode {
    /// Wait for vertical blank. Never tears.
    #[default]
    On,
    /// Present immediately. Lowest latency, may tear.
    Off,
    /// Replace the queued frame when a newer one is ready.
    Mailbox,
}

/// Configuration for one rendering surface.
#[derive(Copy, Clone, Debug)]
pub struct SurfaceConfig {
    /// Surface size in device pixels.
    pub size: Size<DevicePx>,
    /// The surface's scale factor.
    pub scale_factor: ScaleFactor,
    /// Latency preference.
    pub present: PresentPreference,
    /// Vsync behaviour.
    pub vsync: VsyncMode,
    /// Whether the surface should composite with what is behind the window.
    pub transparent: bool,
}

impl Default for SurfaceConfig {
    fn default() -> Self {
        Self {
            size: Size::new(DevicePx(1), DevicePx(1)),
            scale_factor: ScaleFactor::IDENTITY,
            present: PresentPreference::default(),
            vsync: VsyncMode::default(),
            transparent: false,
        }
    }
}

/// Timing and workload figures for one submitted frame.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct FrameStats {
    /// Wall time spent building and compiling the scene, in milliseconds.
    pub cpu_ms: f32,
    /// GPU time for the frame, in milliseconds. `None` when the adapter has no
    /// timestamp query support.
    pub gpu_ms: Option<f32>,
    /// Draw calls issued.
    pub draw_calls: u32,
    /// Pipeline state changes.
    pub pipeline_switches: u32,
    /// Quad instances submitted.
    pub quads: u32,
    /// Glyph instances submitted.
    pub glyphs: u32,
    /// Mesh triangles submitted.
    pub triangles: u32,
    /// Bytes written to GPU buffers this frame.
    pub bytes_uploaded: u64,
    /// Offscreen render targets used.
    pub layers: u32,
}

/// A handle to a frame in progress.
///
/// Held between [`RendererBackend::begin_frame`] and
/// [`RendererBackend::end_frame`]. The backend owns whatever it needs behind
/// this; callers only pass it along.
#[derive(Debug)]
pub struct FrameHandle {
    /// Monotonically increasing frame counter.
    pub index: u64,
    /// The target's size in device pixels.
    pub size: Size<DevicePx>,
    /// Backend-private slot, so a backend can pipeline several frames without
    /// allocating a box per frame.
    pub slot: u32,
}

/// What a backend can do.
///
/// Reported once at startup so higher layers can degrade cleanly rather than
/// failing, which matters most on older integrated GPUs and under software
/// rasterisation.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct BackendCapabilities {
    /// Largest supported 2D texture dimension.
    pub max_texture_size: u32,
    /// Whether GPU timestamp queries are available.
    pub timestamp_queries: bool,
    /// Whether the backend can render to offscreen targets, which layers,
    /// blur and non-fixed-function blend modes all need.
    pub offscreen_targets: bool,
    /// Whether compute shaders are available, used by the faster blur path.
    pub compute: bool,
    /// Maximum number of instances in a single draw call.
    pub max_instances_per_draw: u32,
}

impl Default for BackendCapabilities {
    fn default() -> Self {
        Self {
            max_texture_size: 2048,
            timestamp_queries: false,
            offscreen_targets: true,
            compute: false,
            max_instances_per_draw: u32::MAX,
        }
    }
}

/// A rendering backend.
pub trait RendererBackend {
    /// Human-readable adapter description, for diagnostics and bug reports.
    fn adapter_name(&self) -> &str;

    /// What this backend supports.
    fn capabilities(&self) -> BackendCapabilities;

    /// Reconfigures the surface after a resize or DPI change.
    ///
    /// A zero-area configuration must be accepted and turned into a no-op
    /// rather than an error: minimising a window is not a failure.
    fn configure_surface(&mut self, config: SurfaceConfig) -> Result<(), RenderError>;

    /// Acquires the next frame.
    fn begin_frame(&mut self) -> Result<FrameHandle, SurfaceError>;

    /// Renders a compiled frame into the acquired target.
    fn render(
        &mut self,
        frame: &mut FrameHandle,
        compiled: &CompiledFrame,
        clear: Color,
    ) -> Result<(), RenderError>;

    /// Submits and presents the frame.
    fn end_frame(&mut self, frame: FrameHandle) -> Result<FrameStats, RenderError>;

    /// Uploads RGBA8 pixels into a texture, creating it if `id` is `None`.
    fn upload_texture(
        &mut self,
        id: Option<TextureId>,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<TextureId, RenderError>;

    /// Uploads a sub-rectangle of an existing texture, used by the glyph atlas
    /// as it fills incrementally.
    fn upload_texture_region(
        &mut self,
        id: TextureId,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        rgba: &[u8],
    ) -> Result<(), RenderError>;

    /// Releases a texture.
    fn destroy_texture(&mut self, id: TextureId);

    /// Bytes of GPU memory currently held by textures and buffers.
    fn memory_usage(&self) -> u64;
}

/// Turns a scene into something a backend can render.
///
/// Kept separate from [`RendererBackend`] so the compilation step can be tested
/// and benchmarked without a GPU, which is most of what a batch compiler needs.
pub trait SceneCompiler {
    /// Compiles a scene into GPU-ready buffers.
    fn compile(&mut self, scene: &Scene) -> Result<&CompiledFrame, RenderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_preference_orders_frames_in_flight() {
        assert!(
            PresentPreference::LowLatency.max_frame_latency()
                < PresentPreference::Balanced.max_frame_latency()
        );
        assert!(
            PresentPreference::Balanced.max_frame_latency()
                < PresentPreference::PowerSaving.max_frame_latency()
        );
        assert_eq!(PresentPreference::LowLatency.max_frame_latency(), 1);
    }

    #[test]
    fn default_surface_config_is_never_zero_area() {
        let c = SurfaceConfig::default();
        assert!(c.size.width.get() >= 1 && c.size.height.get() >= 1);
    }
}
