//! The batch compiler: [`Scene`] in, GPU-ready buffers out.
//!
//! This is where painter-order commands become draw calls. Three rules govern
//! it, and they are in tension, so the ordering matters:
//!
//! 1. **Order is never violated.** UI is painted back to front and overlapping
//!    translucent content depends on it. Commands are therefore merged only
//!    when *adjacent* and compatible — run-length batching, not a global sort.
//!    A global sort would batch better and draw wrong.
//! 2. **Invisible work is dropped before it costs anything.** Culling happens
//!    against the device-space scissor before an instance is written, not after.
//! 3. **State changes are counted.** A new batch means a bind or a pipeline
//!    switch, so the compiler reports them and the diagnostics overlay makes
//!    regressions visible.
//!
//! Compilation is GPU-free by construction, which is what makes it testable and
//! benchmarkable without a device.

use crate::primitives::{
    FrameUniforms, GlyphInstance, GpuClip, GpuGradient, GpuTransform, QuadInstance, glyph_flags,
    quad_flags,
};
use crate::scene::{
    ClipKind, DrawCommand, Filter, Layer, Mesh, MeshVertex, NO_INDEX, QuadCommand, Scene,
    SceneIndex, SceneStats, TextRasterMode,
};
use crate::tessellate::{TessellationOptions, Tessellator};
use core::ops::Range;
use rustc_hash::FxHashMap;
use spherekit_core::{
    Affine, BlendMode, Brush, Color, Corners, DevicePx, FontId, GlyphId, ImageId, LinearColor,
    Point, Px, Rect, Size, TextureId,
};

/// What a glyph rasteriser must answer for the compiler to emit a glyph.
///
/// Defined here rather than in `spherekit-text` so the render crate stays
/// independent of the text stack: `spherekit-text` implements this trait, and a
/// test can implement it with a stub in five lines.
pub trait GlyphProvider {
    /// Resolves a glyph to its atlas placement, rasterising on demand.
    ///
    /// Returning `None` means the glyph could not be produced — a missing face,
    /// a full atlas — and the compiler silently skips it rather than failing the
    /// whole frame. One missing glyph must not blank a window.
    fn place_glyph(&mut self, request: GlyphRequest) -> Option<GlyphPlacement>;
}

/// A request for one glyph at one size.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GlyphRequest {
    /// The face.
    pub font: FontId,
    /// The glyph index within that face.
    pub glyph: GlyphId,
    /// Font size in logical pixels.
    pub font_size: Px,
    /// The surface scale factor, so the provider can decide MTSDF vs bitmap
    /// from the *physical* size rather than the logical one.
    pub device_scale: f32,
    /// The caller's rasterisation preference.
    pub mode: TextRasterMode,
    /// Whether an RGB-stripe subpixel bitmap may be returned.
    ///
    /// This is only set for direct rendering to an opaque surface with a
    /// translation-only transform and a backend that supports dual-source
    /// blending. Providers must fall back to grayscale for bitmap glyphs when
    /// it is false.
    pub subpixel: bool,
    /// Quarter-pixel horizontal raster phase in `0..=3`.
    ///
    /// Bitmap providers include this in their cache key and offset the outline
    /// by `subpixel_phase / 4` before rasterisation. Distance fields ignore it.
    pub subpixel_phase: u8,
}

/// Where a glyph lives in the atlas and how to draw it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct GlyphPlacement {
    /// Which atlas page.
    pub page: u32,
    /// Normalised `[u0, v0, u1, v1]` in that page.
    pub uv: [f32; 4],
    /// Ink bounds in em units, pen-relative, y-down: `[x, y, w, h]`.
    pub bounds_em: [f32; 4],
    /// The distance field's range in em units. Zero for a bitmap.
    pub range_em: f32,
    /// True when the placement is a size-specific coverage bitmap rather than
    /// a distance field. This includes grayscale and RGB subpixel bitmaps.
    pub is_bitmap: bool,
    /// True when the bitmap stores independent RGB subpixel coverage.
    ///
    /// This implies [`GlyphPlacement::is_bitmap`] and selects the dual-source
    /// text pipeline.
    pub is_subpixel: bool,
    /// The glyph's size in atlas texels.
    ///
    /// A bitmap glyph is only crisp when its quad covers exactly this many
    /// device pixels, on an integer boundary. The atlas's UV convention is
    /// edge-aligned specifically so that a 1:1 quad samples texel centres, and
    /// that promise is only kept if the batcher snaps the quad — which is why
    /// the texel count has to travel with the placement.
    pub texel_size: [u32; 2],
}

/// What an image cache must answer for the compiler to emit a textured quad.
pub trait TextureProvider {
    /// Resolves an image handle to an uploaded texture.
    fn texture_for(&mut self, image: ImageId) -> Option<TextureId>;
}

/// Which pipeline a batch runs on.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BatchKind {
    /// Analytic rectangles, rounded rectangles, borders and shadows.
    Quad,
    /// Text, sampling one atlas page.
    Glyph {
        /// The atlas page this batch samples.
        page: u32,
        /// True when the page holds coverage bitmaps rather than distance
        /// fields, which the shader needs to know.
        bitmap: bool,
        /// True when this page stores RGB subpixel coverage and therefore needs
        /// the dual-source blend pipeline.
        subpixel: bool,
    },
    /// Textured quads.
    Image {
        /// The texture this batch samples.
        texture: TextureId,
    },
    /// Tessellated or generated triangle geometry.
    Mesh {
        /// Optional texture; `None` means vertex color only.
        texture: Option<TextureId>,
    },
}

/// A run of instances or indices that can be issued as one draw call.
#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    /// Which pipeline to bind.
    pub kind: BatchKind,
    /// Scissor rectangle in device pixels, relative to the batch's target.
    pub scissor: Rect<DevicePx>,
    /// Instance range for instanced kinds, index range for [`BatchKind::Mesh`].
    pub range: Range<u32>,
    /// First vertex, for mesh draws.
    pub base_vertex: u32,
    /// Index into [`CompiledFrame::targets`].
    pub target: u32,
}

/// A render target the frame writes into.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct RenderTarget {
    /// `true` for the swapchain image, `false` for an offscreen layer texture.
    pub is_surface: bool,
    /// Size in device pixels.
    pub size: Size<DevicePx>,
    /// The target's origin in scene-absolute logical pixels.
    ///
    /// Offscreen layers are allocated only as large as their content, so the
    /// vertex stage subtracts this to bring scene coordinates into target
    /// space. Allocating every layer at full window size would be simpler and
    /// far more expensive.
    pub origin: Point<Px>,
}

/// How a finished offscreen target composites back into its parent.
#[derive(Clone, Debug, PartialEq)]
pub struct Composite {
    /// The offscreen target to read.
    pub source: u32,
    /// The target to write into.
    pub destination: u32,
    /// Where in the destination, in scene-absolute logical pixels.
    pub bounds: Rect<Px>,
    /// Opacity applied while compositing.
    pub opacity: f32,
    /// Blend mode used while compositing.
    pub blend: BlendMode,
    /// Effect applied before compositing.
    pub filter: Option<Filter>,
}

/// One render pass: a contiguous run of batches sharing a target.
#[derive(Clone, Debug, PartialEq)]
pub struct Pass {
    /// Index into [`CompiledFrame::targets`].
    pub target: u32,
    /// Range into [`CompiledFrame::batches`].
    pub batches: Range<u32>,
    /// Whether this pass must clear its target first.
    ///
    /// Only the first pass on a given target clears; a target resumed after a
    /// nested layer must preserve what it already holds.
    pub clear: bool,
    /// The composite to run after this pass, if it ends a layer.
    pub composite: Option<Composite>,
}

/// Everything a backend needs to draw one frame.
#[derive(Clone, Debug, Default)]
pub struct CompiledFrame {
    /// Quad instances, indexed by [`Batch::range`].
    pub quads: Vec<QuadInstance>,
    /// Glyph instances.
    pub glyphs: Vec<GlyphInstance>,
    /// Mesh vertices.
    pub mesh_vertices: Vec<MeshVertex>,
    /// Mesh indices.
    pub mesh_indices: Vec<u32>,
    /// Transform table, indexed by instances.
    pub transforms: Vec<GpuTransform>,
    /// Clip table, indexed by instances.
    pub clips: Vec<GpuClip>,
    /// Gradient table, indexed by instances.
    pub gradients: Vec<GpuGradient>,
    /// Draw calls in submission order.
    pub batches: Vec<Batch>,
    /// Render passes in submission order.
    pub passes: Vec<Pass>,
    /// Render targets referenced by passes.
    pub targets: Vec<RenderTarget>,
    /// Per-frame uniforms.
    pub uniforms: FrameUniforms,
    /// Workload figures.
    pub stats: SceneStats,
}

impl CompiledFrame {
    /// Drops every buffer's contents while keeping the allocations.
    pub fn clear(&mut self) {
        self.quads.clear();
        self.glyphs.clear();
        self.mesh_vertices.clear();
        self.mesh_indices.clear();
        self.transforms.clear();
        self.clips.clear();
        self.gradients.clear();
        self.batches.clear();
        self.passes.clear();
        self.targets.clear();
        self.stats = SceneStats::default();
    }

    /// True when the frame would draw nothing.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.batches.is_empty()
    }

    /// Total bytes that must be uploaded for this frame.
    pub fn upload_bytes(&self) -> u64 {
        use core::mem::size_of;
        (self.quads.len() * size_of::<QuadInstance>()
            + self.glyphs.len() * size_of::<GlyphInstance>()
            + self.mesh_vertices.len() * size_of::<MeshVertex>()
            + self.mesh_indices.len() * 4
            + self.transforms.len() * size_of::<GpuTransform>()
            + self.clips.len() * size_of::<GpuClip>()
            + self.gradients.len() * size_of::<GpuGradient>()) as u64
    }
}

/// Turns scenes into [`CompiledFrame`]s, reusing its buffers across frames.
pub struct BatchCompiler {
    frame: CompiledFrame,
    tessellator: Tessellator,
    /// Absolute clip bounds and radii, resolved once per frame per clip index.
    resolved_clips: Vec<ResolvedClip>,
    /// Maps a scene paint index to a gradient table index, so a gradient shared
    /// by many primitives is uploaded once.
    gradient_map: FxHashMap<SceneIndex, u32>,
    /// Stack of open targets during the layer walk.
    target_stack: Vec<OpenTarget>,
    current_batch: Option<OpenBatch>,
    pass_start: u32,
    time: f32,
    /// Whether the final surface can accept RGB subpixel coverage.
    ///
    /// Defaults off so non-wgpu and test backends degrade safely. The surface
    /// integration enables it only for an opaque target backed by a device with
    /// dual-source blending.
    subpixel_text_enabled: bool,
}

#[derive(Copy, Clone, Debug)]
struct ResolvedClip {
    /// Intersected bounds of the whole ancestor chain, in logical pixels.
    bounds: Rect<Px>,
    /// Radii of the nearest rounded clip, or zero.
    radii: Corners<Px>,
    /// Whether the chain contains a rounded or path clip.
    needs_shader_clip: bool,
    /// Index into the GPU clip table.
    gpu_index: u32,
}

#[derive(Copy, Clone, Debug)]
struct OpenTarget {
    target: u32,
    layer: SceneIndex,
    /// Where the parent's pass should resume from.
    parent_target: u32,
    /// The layer's bounds snapped to the device grid, in logical pixels.
    ///
    /// Both the target's origin and the composite's destination come from this
    /// rather than from `Layer::bounds`, so the offscreen texture's texel (0, 0)
    /// is a whole device pixel and the blit back is one texel to one pixel. See
    /// [`BatchCompiler::begin_layer`].
    snapped: Rect<Px>,
}

#[derive(Clone, Debug)]
struct OpenBatch {
    kind: BatchKind,
    scissor: Rect<DevicePx>,
    start: u32,
    base_vertex: u32,
    target: u32,
}

impl Default for BatchCompiler {
    fn default() -> Self {
        Self::new()
    }
}

impl BatchCompiler {
    /// A new compiler.
    pub fn new() -> Self {
        Self {
            frame: CompiledFrame::default(),
            tessellator: Tessellator::new(),
            resolved_clips: Vec::new(),
            gradient_map: FxHashMap::default(),
            target_stack: Vec::new(),
            current_batch: None,
            pass_start: 0,
            time: 0.0,
            subpixel_text_enabled: false,
        }
    }

    /// Sets the animation clock handed to shaders via [`FrameUniforms::time`].
    pub fn set_time(&mut self, seconds: f32) {
        self.time = seconds;
    }

    /// Enables RGB subpixel text on eligible direct-to-surface glyph runs.
    ///
    /// Eligibility is checked again per run: offscreen layers and transforms
    /// other than a translation always use grayscale coverage.
    pub fn set_subpixel_text_enabled(&mut self, enabled: bool) {
        self.subpixel_text_enabled = enabled;
    }

    /// The most recently compiled frame.
    #[inline]
    pub fn frame(&self) -> &CompiledFrame {
        &self.frame
    }

    /// Compiles a scene.
    pub fn compile(
        &mut self,
        scene: &Scene,
        glyphs: &mut dyn GlyphProvider,
        textures: &mut dyn TextureProvider,
    ) -> &CompiledFrame {
        self.frame.clear();
        self.gradient_map.clear();
        self.target_stack.clear();
        self.current_batch = None;
        self.pass_start = 0;

        let scale = scene.scale_factor;
        let (vw, vh) = scene.viewport.to_surface_extent(scale);
        self.frame.uniforms = FrameUniforms {
            viewport: [vw as f32, vh as f32],
            scale_factor: scale.get(),
            time: self.time,
        };

        // The surface is always target 0.
        self.frame.targets.push(RenderTarget {
            is_surface: true,
            size: Size::new(DevicePx(vw as i32), DevicePx(vh as i32)),
            origin: Point::ZERO,
        });

        for t in &scene.transforms {
            self.frame.transforms.push(GpuTransform::from_affine(*t, scale));
        }
        self.resolve_clips(scene);

        let viewport_px = Rect::new(Point::ZERO, scene.viewport);
        self.frame.stats.commands = scene.commands.len() as u32;

        for cmd in &scene.commands {
            match cmd {
                DrawCommand::BeginLayer { layer } => self.begin_layer(scene, *layer),
                DrawCommand::EndLayer => self.end_layer(scene),
                _ => self.emit(scene, cmd, viewport_px, glyphs, textures),
            }
        }

        // A scene that ends with layers still open would leave content in an
        // offscreen texture that never composites. Close them.
        while !self.target_stack.is_empty() {
            self.end_layer(scene);
        }
        self.flush_batch();
        self.close_pass(0, None);

        self.frame.stats.batches = self.frame.batches.len() as u32;
        self.frame.stats.quads = self.frame.quads.len() as u32;
        self.frame.stats.glyphs = self.frame.glyphs.len() as u32;
        self.frame.stats.triangles = (self.frame.mesh_indices.len() / 3) as u32;
        self.frame.stats.layers = self.frame.targets.len().saturating_sub(1) as u32;
        &self.frame
    }

    // -------------------------------------------------------------- clips

    fn resolve_clips(&mut self, scene: &Scene) {
        self.resolved_clips.clear();
        self.resolved_clips.reserve(scene.clips.len());

        for i in 0..scene.clips.len() {
            let clip = &scene.clips[i];
            // Parents always precede children: the canvas only ever appends a
            // clip whose parent already exists, so one forward pass suffices.
            let (parent_bounds, parent_radii, parent_shader) =
                if clip.parent == NO_INDEX || clip.parent as usize >= self.resolved_clips.len() {
                    (Rect::INFINITE, Corners::ZERO, false)
                } else {
                    let p = self.resolved_clips[clip.parent as usize];
                    (p.bounds, p.radii, p.needs_shader_clip)
                };

            let bounds = parent_bounds.intersection(clip.bounds);
            let (radii, shader) = match &clip.kind {
                ClipKind::Rect => (parent_radii, parent_shader),
                // Only the innermost rounded clip is evaluated analytically.
                // Nesting two rounded clips is vanishingly rare in UI, and the
                // outer one is still enforced by the intersected scissor, so
                // the result is conservative rather than wrong.
                ClipKind::Rounded(r) => (*r, true),
                // A path clip needs a mask the backend builds separately; the
                // scissor keeps it conservative until then.
                ClipKind::Path { .. } => (parent_radii, true),
            };

            let gpu_index = self.frame.clips.len() as u32;
            self.frame.clips.push(GpuClip {
                bounds: [
                    bounds.min_x().get(),
                    bounds.min_y().get(),
                    bounds.max_x().get(),
                    bounds.max_y().get(),
                ],
                radii: radii.clamp_for(bounds.size).to_array(),
            });
            self.resolved_clips.push(ResolvedClip {
                bounds,
                radii,
                needs_shader_clip: shader,
                gpu_index,
            });
        }

        if self.resolved_clips.is_empty() {
            self.frame.clips.push(GpuClip::INFINITE);
            self.resolved_clips.push(ResolvedClip {
                bounds: Rect::INFINITE,
                radii: Corners::ZERO,
                needs_shader_clip: false,
                gpu_index: 0,
            });
        }
    }

    #[inline]
    fn clip_of(&self, i: SceneIndex) -> ResolvedClip {
        self.resolved_clips.get(i as usize).copied().unwrap_or(self.resolved_clips[0])
    }

    // ------------------------------------------------------------- layers

    fn begin_layer(&mut self, scene: &Scene, layer_index: SceneIndex) {
        let Some(layer) = scene.layers.get(layer_index as usize) else { return };
        self.flush_batch();

        let scale = scene.scale_factor;
        let device = layer.bounds.round_out(scale);
        let (w, h) = device.size.to_extent();
        // Cap the offscreen allocation at the surface size. A layer whose
        // bounds are enormous (an unclipped shadow, a runaway transform) must
        // not turn into a gigabyte of render target.
        let (max_w, max_h) = (
            self.frame.targets[0].size.width.as_u32().max(1),
            self.frame.targets[0].size.height.as_u32().max(1),
        );
        let size = Size::new(
            DevicePx(w.min(max_w.saturating_mul(2)) as i32),
            DevicePx(h.min(max_h.saturating_mul(2)) as i32),
        );

        // The target's origin is what every vertex stage subtracts, so it has to
        // name the texture's texel (0, 0) — which `round_out` put on a whole
        // device pixel. Handing over `layer.bounds.origin` instead leaves a
        // fractional offset in the subtraction, and that offset lands on the one
        // thing in this compiler that depends on absolute device coordinates:
        // `snap_glyph_quad` rounds a glyph to the device grid knowing nothing
        // about which target it is bound for, so inside a layer every bitmap
        // glyph would sit `frac(origin * scale)` off its texel and go through the
        // atlas's linear sampler as a genuine bilinear blur. The size is derived
        // the same way for the same reason: the composite has to be a unit-scale
        // blit or the layer is resampled on the way out.
        //
        // From `size` and not `device.size`, because the clamp above can shrink
        // an oversized layer, and describing the destination with the unclamped
        // rect would stretch the texture across it.
        let inv = 1.0 / scale.get();
        let snapped = Rect::new(
            Point::new(Px(device.origin.x.0 as f32 * inv), Px(device.origin.y.0 as f32 * inv)),
            Size::new(Px(size.width.0 as f32 * inv), Px(size.height.0 as f32 * inv)),
        );

        let parent_target = self.target_stack.last().map(|t| t.target).unwrap_or(0);
        self.frame.targets.push(RenderTarget { is_surface: false, size, origin: snapped.origin });
        let target = (self.frame.targets.len() - 1) as u32;

        self.close_pass(parent_target, None);
        self.target_stack.push(OpenTarget { target, layer: layer_index, parent_target, snapped });
    }

    fn end_layer(&mut self, scene: &Scene) {
        let Some(open) = self.target_stack.pop() else { return };
        self.flush_batch();

        let layer = scene.layers.get(open.layer as usize).cloned().unwrap_or(Layer {
            bounds: Rect::ZERO,
            opacity: 1.0,
            blend: BlendMode::Normal,
            filter: None,
            end_command: NO_INDEX,
        });

        self.close_pass(
            open.target,
            Some(Composite {
                source: open.target,
                destination: open.parent_target,
                // The snapped rect, not `layer.bounds`: it is exactly as large
                // as the texture that was allocated, so the blit is one texel to
                // one pixel and nothing inside the layer is resampled.
                bounds: open.snapped,
                opacity: layer.opacity,
                blend: layer.blend,
                filter: layer.filter,
            }),
        );
    }

    fn close_pass(&mut self, target: u32, composite: Option<Composite>) {
        let end = self.frame.batches.len() as u32;
        if end == self.pass_start && composite.is_none() {
            return;
        }
        // A target is cleared by the first pass that writes it. Offscreen
        // layers always start blank; the surface is cleared once per frame.
        let clear = !self.frame.passes.iter().any(|p| p.target == target);
        self.frame.passes.push(Pass { target, batches: self.pass_start..end, clear, composite });
        self.pass_start = end;
    }

    #[inline]
    fn current_target(&self) -> u32 {
        self.target_stack.last().map(|t| t.target).unwrap_or(0)
    }

    // --------------------------------------------------------- batching

    fn push_instance(&mut self, kind: BatchKind, scissor: Rect<DevicePx>, count: u32) {
        let target = self.current_target();
        let compatible = self
            .current_batch
            .as_ref()
            .is_some_and(|b| b.kind == kind && b.scissor == scissor && b.target == target);
        if !compatible {
            self.flush_batch();
            let start = match kind {
                BatchKind::Quad => self.frame.quads.len() as u32,
                BatchKind::Glyph { .. } => self.frame.glyphs.len() as u32,
                BatchKind::Image { .. } => self.frame.quads.len() as u32,
                BatchKind::Mesh { .. } => self.frame.mesh_indices.len() as u32,
            };
            self.current_batch = Some(OpenBatch { kind, scissor, start, base_vertex: 0, target });
        }
        let _ = count;
    }

    fn flush_batch(&mut self) {
        let Some(b) = self.current_batch.take() else { return };
        let end = match b.kind {
            BatchKind::Quad | BatchKind::Image { .. } => self.frame.quads.len() as u32,
            BatchKind::Glyph { .. } => self.frame.glyphs.len() as u32,
            BatchKind::Mesh { .. } => self.frame.mesh_indices.len() as u32,
        };
        if end > b.start {
            self.frame.batches.push(Batch {
                kind: b.kind,
                scissor: b.scissor,
                range: b.start..end,
                base_vertex: b.base_vertex,
                target: b.target,
            });
        }
    }

    // --------------------------------------------------------- emission

    fn emit(
        &mut self,
        scene: &Scene,
        cmd: &DrawCommand,
        viewport: Rect<Px>,
        glyph_provider: &mut dyn GlyphProvider,
        textures: &mut dyn TextureProvider,
    ) {
        match cmd {
            DrawCommand::Quad(q) => self.emit_quad(scene, q, viewport),
            DrawCommand::Shadow { shape, blur_radius, color, inset, transform, clip } => self
                .emit_shadow(
                    scene,
                    *shape,
                    *blur_radius,
                    *color,
                    *inset,
                    *transform,
                    *clip,
                    viewport,
                ),
            DrawCommand::Text { run, paint, transform, clip } => {
                self.emit_text(scene, *run, *paint, *transform, *clip, viewport, glyph_provider)
            }
            DrawCommand::Image { dest, source, image, tint, radii, transform, clip } => self
                .emit_image(
                    scene, *dest, *source, *image, *tint, *radii, *transform, *clip, viewport,
                    textures,
                ),
            DrawCommand::FillPath { path, paint, fill_rule, transform, clip } => {
                self.emit_fill_path(scene, *path, *paint, *fill_rule, *transform, *clip, viewport)
            }
            DrawCommand::StrokePath { path, paint, stroke, transform, clip } => {
                self.emit_stroke_path(scene, *path, *paint, *stroke, *transform, *clip, viewport)
            }
            DrawCommand::Mesh { mesh, texture, transform, clip } => {
                self.emit_mesh(scene, *mesh, *texture, *transform, *clip, viewport, textures)
            }
            DrawCommand::BeginLayer { .. } | DrawCommand::EndLayer => {}
        }
    }

    /// Computes the device-space scissor and rejects the command when it cannot
    /// contribute anything visible.
    fn visible(
        &mut self,
        scene: &Scene,
        local_bounds: Rect<Px>,
        transform: SceneIndex,
        clip: SceneIndex,
        viewport: Rect<Px>,
    ) -> Option<(ResolvedClip, Rect<DevicePx>)> {
        let rc = self.clip_of(clip);
        let t = scene.transform(transform);
        let world = t.transform_rect_bounds(local_bounds);

        let visible_region = rc.bounds.intersection(viewport);
        if visible_region.is_empty() || !world.intersects(visible_region) {
            self.frame.stats.culled += 1;
            return None;
        }
        Some((rc, visible_region.round_out(scene.scale_factor)))
    }

    fn emit_quad(&mut self, scene: &Scene, q: &QuadCommand, viewport: Rect<Px>) {
        let Some((rc, scissor)) = self.visible(scene, q.bounds, q.transform, q.clip, viewport)
        else {
            return;
        };

        let mut flags = 0u32;
        if rc.needs_shader_clip {
            flags |= quad_flags::CLIP_ROUNDED;
        }
        let mut color = LinearColor::TRANSPARENT;
        let mut gradient = u32::MAX;

        if q.fill != NO_INDEX
            && let Some(paint) = scene.paints.get(q.fill as usize)
        {
            match &paint.brush {
                Brush::Solid(c) => {
                    flags |= quad_flags::FILL_SOLID;
                    color = c.scale_alpha(paint.opacity).to_linear();
                }
                Brush::Gradient(g) => {
                    flags |= quad_flags::FILL_GRADIENT;
                    gradient = self.intern_gradient(q.fill, g);
                    // The shader still needs a base color for the degenerate
                    // single-stop case and for premultiplied alpha maths.
                    color = g.sample(0.0).scale_alpha(paint.opacity).to_linear();
                }
                Brush::Image { tint, .. } => {
                    flags |= quad_flags::FILL_SOLID;
                    color = tint.scale_alpha(paint.opacity).to_linear();
                }
            }
        }
        let border = q.border_width > Px::ZERO && !q.border_color.is_transparent();
        if border {
            flags |= quad_flags::BORDER;
        }

        self.push_instance(BatchKind::Quad, scissor, 1);
        self.frame.quads.push(QuadInstance {
            bounds: [
                q.bounds.min_x().get(),
                q.bounds.min_y().get(),
                q.bounds.width().get(),
                q.bounds.height().get(),
            ],
            radii: q.radii.clamp_for(q.bounds.size).to_array(),
            color: color.to_array(),
            border_color: q.border_color.to_linear().to_array(),
            border_width: if border { q.border_width.get() } else { 0.0 },
            blur_sigma: 0.0,
            flags,
            gradient,
            transform_index: q.transform,
            clip_index: rc.gpu_index,
            _pad: [0; 2],
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_shadow(
        &mut self,
        scene: &Scene,
        shape: spherekit_core::RoundedRect,
        blur_radius: Px,
        color: Color,
        inset: bool,
        transform: SceneIndex,
        clip: SceneIndex,
        viewport: Rect<Px>,
    ) {
        // The blurred footprint reaches beyond the shape, so cull against the
        // grown bounds or an offscreen shape's visible shadow disappears.
        let grow = Px(blur_radius.get() * 1.5 + 1.0);
        let grown = shape.rect.outset(spherekit_core::Edges::all(grow));
        let Some((rc, scissor)) = self.visible(scene, grown, transform, clip, viewport) else {
            return;
        };

        let mut flags = quad_flags::SHADOW | quad_flags::FILL_SOLID;
        if inset {
            flags |= quad_flags::SHADOW_INSET;
        }
        if rc.needs_shader_clip {
            flags |= quad_flags::CLIP_ROUNDED;
        }

        self.push_instance(BatchKind::Quad, scissor, 1);
        self.frame.quads.push(QuadInstance {
            bounds: [
                shape.rect.min_x().get(),
                shape.rect.min_y().get(),
                shape.rect.width().get(),
                shape.rect.height().get(),
            ],
            radii: shape.clamped_radii().to_array(),
            color: color.to_linear().to_array(),
            border_color: [0.0; 4],
            border_width: 0.0,
            // The analytic box-shadow approximation treats `blur_radius` as
            // twice sigma, matching the CSS definition.
            blur_sigma: (blur_radius.get() * 0.5).max(1e-3),
            flags,
            gradient: u32::MAX,
            transform_index: transform,
            clip_index: rc.gpu_index,
            _pad: [0; 2],
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_text(
        &mut self,
        scene: &Scene,
        run_index: SceneIndex,
        paint_index: SceneIndex,
        transform: SceneIndex,
        clip: SceneIndex,
        viewport: Rect<Px>,
        provider: &mut dyn GlyphProvider,
    ) {
        let Some(run) = scene.runs.get(run_index as usize) else { return };
        let color = match scene.paints.get(paint_index as usize).map(|p| (&p.brush, p.opacity)) {
            Some((Brush::Solid(c), o)) => c.scale_alpha(o).to_linear(),
            Some((Brush::Gradient(g), o)) => g.sample(0.5).scale_alpha(o).to_linear(),
            Some((Brush::Image { tint, .. }, o)) => tint.scale_alpha(o).to_linear(),
            None => return,
        };

        let size = run.font_size.get();
        let scale = scene.scale_factor.get();
        let outline = run.outline_width > Px::ZERO;
        let outline_color = run.outline_color.to_linear();

        // Snapping is only meaningful when the transform is a translation: under
        // rotation or non-uniform scale there is no pixel grid to snap to, and
        // forcing one would make text crawl as the transform animates.
        let affine = scene.transform(transform);
        let snappable = affine.is_translation_only();
        let translation = affine.translation();
        // RGB coverage is tied to the physical stripes of the final opaque
        // surface. An intermediate texture would collapse those three coverage
        // values into ordinary RGBA before its opacity/filter/transform is
        // known, producing coloured fringes when it is composited later.
        let allow_subpixel = self.subpixel_text_enabled
            && self.target_stack.is_empty()
            && snappable
            // The RGB coverage bitmap has no signed-distance information from
            // which the text shader could reconstruct a synthetic outline.
            && !outline;

        for g in &run.glyphs {
            // Split the device-space pen into an integral placement plus one of
            // four cached raster phases. The outline is shifted by the phase in
            // the provider while the resulting bitmap is drawn at the integral
            // pen, so the fractional position is applied exactly once.
            let (bitmap_pen_x, subpixel_phase) = if snappable && scale > 0.0 {
                let device_pen = (g.position.x.get() + translation.width.get()) * scale;
                let (integral, phase) = quarter_pixel_bin(device_pen);
                (Px(integral as f32 / scale - translation.width.get()), phase)
            } else {
                (g.position.x, 0)
            };
            let Some(p) = provider.place_glyph(GlyphRequest {
                font: run.font,
                glyph: g.glyph,
                font_size: run.font_size,
                device_scale: scale,
                mode: run.raster,
                subpixel: allow_subpixel,
                subpixel_phase,
            }) else {
                continue;
            };
            if p.uv[2] <= p.uv[0] || p.uv[3] <= p.uv[1] {
                // A zero-area placement is a space or a control character.
                continue;
            }

            // The atlas image covers the glyph's ink box outset by the field's
            // range on every side, and `generate_mtsdf` is explicit that the
            // quad has to match. Drawing the ink box alone squeezes the whole
            // padded image into it, which shrinks every glyph by
            // `ink / (ink + 2 * range)` -- around 75 % for a typical lowercase
            // letter -- while its advance stays correct, so a line comes out
            // small and tracked out. `range_em` is zero for a bitmap, so one
            // unconditional outset is right for both paths.
            debug_assert!(!p.is_subpixel || p.is_bitmap);
            let pad = p.range_em * size;
            let pen_x = if p.is_bitmap { bitmap_pen_x } else { g.position.x };
            let mut bounds = Rect::new(
                Point::new(
                    Px(pen_x.get() + p.bounds_em[0] * size - pad),
                    Px(g.position.y.get() + p.bounds_em[1] * size - pad),
                ),
                Size::new(
                    Px(p.bounds_em[2] * size + 2.0 * pad),
                    Px(p.bounds_em[3] * size + 2.0 * pad),
                ),
            );
            if snappable {
                bounds = snap_glyph_quad(bounds, g.position, &p, translation, scale);
            }

            let Some((rc, scissor)) = self.visible(scene, bounds, transform, clip, viewport) else {
                continue;
            };

            let mut flags = 0u32;
            if p.is_bitmap {
                flags |= glyph_flags::BITMAP;
            }
            if outline {
                flags |= glyph_flags::OUTLINE;
            }
            if rc.needs_shader_clip {
                flags |= glyph_flags::CLIP_ROUNDED;
            }
            if p.is_subpixel {
                flags |= glyph_flags::SUBPIXEL;
            }

            self.push_instance(
                BatchKind::Glyph { page: p.page, bitmap: p.is_bitmap, subpixel: p.is_subpixel },
                scissor,
                1,
            );
            self.frame.glyphs.push(GlyphInstance {
                bounds: [
                    bounds.min_x().get(),
                    bounds.min_y().get(),
                    bounds.width().get(),
                    bounds.height().get(),
                ],
                uv: p.uv,
                color: color.to_array(),
                outline_color: outline_color.to_array(),
                // The field's em range becomes a destination-pixel range once
                // multiplied by the font size and the surface scale. This is
                // what lets one batch mix sizes and still antialias correctly.
                px_range: (p.range_em * size * scale).max(1e-3),
                outline_width: run.outline_width.get(),
                // A non-finite exponent would make `pow` in the shader produce
                // NaN coverage, which shows as a black block. The sign is
                // meaningful here — it picks which end of the ramp bends — so
                // only the magnitude is clamped, and a magnitude below one is
                // the shader's own no-op.
                coverage_contrast: if run.coverage_contrast.is_finite() {
                    run.coverage_contrast.clamp(-8.0, 8.0)
                } else {
                    1.0
                },
                // The size the small-text compensation keys off. The vertex
                // stage multiplies it by the transform's scale, exactly as it
                // does `px_range`, so a zoomed panel is compensated for the
                // size it actually appears at rather than the size it was
                // recorded at.
                em_px: size * scale,
                flags,
                atlas_page: p.page,
                transform_index: transform,
                clip_index: rc.gpu_index,
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_image(
        &mut self,
        scene: &Scene,
        dest: Rect<Px>,
        source: Rect<Px>,
        image: ImageId,
        tint: Color,
        radii: Corners<Px>,
        transform: SceneIndex,
        clip: SceneIndex,
        viewport: Rect<Px>,
        textures: &mut dyn TextureProvider,
    ) {
        let Some(texture) = textures.texture_for(image) else {
            self.frame.stats.culled += 1;
            return;
        };
        let Some((rc, scissor)) = self.visible(scene, dest, transform, clip, viewport) else {
            return;
        };

        let mut flags = quad_flags::FILL_TEXTURE;
        if rc.needs_shader_clip {
            flags |= quad_flags::CLIP_ROUNDED;
        }

        self.push_instance(BatchKind::Image { texture }, scissor, 1);
        self.frame.quads.push(QuadInstance {
            bounds: [
                dest.min_x().get(),
                dest.min_y().get(),
                dest.width().get(),
                dest.height().get(),
            ],
            radii: radii.clamp_for(dest.size).to_array(),
            color: tint.to_linear().to_array(),
            // The source rectangle rides in the border-color slot, which is
            // unused for textured quads. Reusing it keeps the instance at 96
            // bytes instead of growing the struct for every quad in the frame.
            border_color: [
                source.min_x().get(),
                source.min_y().get(),
                source.width().get(),
                source.height().get(),
            ],
            border_width: 0.0,
            blur_sigma: 0.0,
            flags,
            gradient: u32::MAX,
            transform_index: transform,
            clip_index: rc.gpu_index,
            _pad: [0; 2],
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_fill_path(
        &mut self,
        scene: &Scene,
        path_index: SceneIndex,
        paint_index: SceneIndex,
        fill_rule: spherekit_core::FillRule,
        transform: SceneIndex,
        clip: SceneIndex,
        viewport: Rect<Px>,
    ) {
        let Some(path) = scene.paths.get(path_index as usize) else { return };
        let Some((_, scissor)) =
            self.visible(scene, path.control_bounds(), transform, clip, viewport)
        else {
            return;
        };
        let color = match scene.paints.get(paint_index as usize).map(|p| (&p.brush, p.opacity)) {
            Some((Brush::Solid(c), o)) => c.scale_alpha(o).to_linear(),
            Some((Brush::Gradient(g), o)) => g.sample(0.5).scale_alpha(o).to_linear(),
            Some((Brush::Image { tint, .. }, o)) => tint.scale_alpha(o).to_linear(),
            None => return,
        };
        let opts = TessellationOptions {
            tolerance: 0.25,
            scale: scene.transform(transform).approx_scale() * scene.scale_factor.get(),
        };
        let Ok(mesh) = self.tessellator.fill_path(path, fill_rule, color, opts) else { return };
        self.append_mesh(&mesh, None, scissor, scene.transform(transform));
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_stroke_path(
        &mut self,
        scene: &Scene,
        path_index: SceneIndex,
        paint_index: SceneIndex,
        stroke_index: SceneIndex,
        transform: SceneIndex,
        clip: SceneIndex,
        viewport: Rect<Px>,
    ) {
        let Some(path) = scene.paths.get(path_index as usize) else { return };
        let Some(stroke) = scene.strokes.get(stroke_index as usize) else { return };
        let grow = spherekit_core::Edges::all(Px(stroke.width.get() * 0.5 + 1.0));
        let Some((_, scissor)) =
            self.visible(scene, path.control_bounds().outset(grow), transform, clip, viewport)
        else {
            return;
        };
        let color = match scene.paints.get(paint_index as usize).map(|p| (&p.brush, p.opacity)) {
            Some((Brush::Solid(c), o)) => c.scale_alpha(o).to_linear(),
            Some((Brush::Gradient(g), o)) => g.sample(0.5).scale_alpha(o).to_linear(),
            Some((Brush::Image { tint, .. }, o)) => tint.scale_alpha(o).to_linear(),
            None => return,
        };
        let opts = TessellationOptions {
            tolerance: 0.25,
            scale: scene.transform(transform).approx_scale() * scene.scale_factor.get(),
        };
        let dashed;
        let target = if stroke.dash.is_empty() {
            path
        } else {
            dashed = crate::tessellate::apply_dash(
                path,
                &stroke.dash,
                stroke.dash_offset,
                Px(opts.local_tolerance()),
            );
            &dashed
        };
        let Ok(mesh) = self.tessellator.stroke_path(target, stroke, color, opts) else { return };
        self.append_mesh(&mesh, None, scissor, scene.transform(transform));
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_mesh(
        &mut self,
        scene: &Scene,
        mesh_index: SceneIndex,
        texture: Option<TextureId>,
        transform: SceneIndex,
        clip: SceneIndex,
        viewport: Rect<Px>,
        textures: &mut dyn TextureProvider,
    ) {
        let Some(mesh) = scene.meshes.get(mesh_index as usize) else { return };
        let _ = textures;
        let Some((_, scissor)) = self.visible(scene, mesh.bounds(), transform, clip, viewport)
        else {
            return;
        };
        self.append_mesh(mesh, texture, scissor, scene.transform(transform));
    }

    /// Appends mesh geometry, baking the transform into the vertices.
    ///
    /// Mesh vertices carry no transform index — an indexed transform would need
    /// a per-vertex lookup, and meshes are already CPU-side geometry, so baking
    /// is both simpler and cheaper.
    fn append_mesh(
        &mut self,
        mesh: &Mesh,
        texture: Option<TextureId>,
        scissor: Rect<DevicePx>,
        transform: Affine,
    ) {
        if mesh.is_empty() {
            return;
        }
        let base = self.frame.mesh_vertices.len() as u32;
        // A single mesh cannot exceed the u32 index space, and neither can the
        // accumulated frame; bail rather than wrap around into garbage indices.
        if base as usize + mesh.vertices.len() > u32::MAX as usize {
            return;
        }

        let kind = BatchKind::Mesh { texture };
        let target = self.current_target();
        let compatible = self.current_batch.as_ref().is_some_and(|b| {
            b.kind == kind && b.scissor == scissor && b.target == target && b.base_vertex == 0
        });
        if !compatible {
            self.flush_batch();
            self.current_batch = Some(OpenBatch {
                kind,
                scissor,
                start: self.frame.mesh_indices.len() as u32,
                // Indices are rebased on append, so every mesh batch draws from
                // vertex zero. That lets consecutive meshes merge into one call.
                base_vertex: 0,
                target,
            });
        }

        if transform == Affine::IDENTITY {
            self.frame.mesh_vertices.extend_from_slice(&mesh.vertices);
        } else {
            self.frame.mesh_vertices.extend(mesh.vertices.iter().map(|v| {
                let p = transform.apply(Point::new(Px(v.position[0]), Px(v.position[1])));
                MeshVertex { position: [p.x.get(), p.y.get()], uv: v.uv, color: v.color }
            }));
        }
        self.frame.mesh_indices.extend(mesh.indices.iter().map(|i| i + base));
    }

    fn intern_gradient(&mut self, paint_index: SceneIndex, g: &spherekit_core::Gradient) -> u32 {
        if let Some(i) = self.gradient_map.get(&paint_index) {
            return *i;
        }
        let i = self.frame.gradients.len() as u32;
        self.frame.gradients.push(GpuGradient::from_gradient(g));
        self.gradient_map.insert(paint_index, i);
        i
    }
}

/// Aligns a glyph quad to the device pixel grid.
///
/// The correction has to be *uniform across a run*, which is the part that is
/// easy to get wrong. Every glyph has a different ink top — an `x`, an `l` and
/// a `g` all begin at different heights — so rounding each quad's own top edge
/// hands every glyph in the line a different sub-pixel shift and pulls the
/// shared baseline apart. The shift is derived from the pen instead, which
/// every glyph in the run shares, and applied as one common delta.
///
/// Two separate problems, with two different answers:
///
/// * **Vertical.** A baseline at a fractional device y softens every glyph in
///   the run, distance field or not, and the softening is identical for all of
///   them, so nothing is gained by leaving it fractional. Always snapped.
/// * **Horizontal.** A distance field reconstructs correctly at any subpixel x,
///   and snapping it would visibly quantise letter spacing at small sizes — so
///   MTSDF glyphs keep their exact x. A *bitmap* glyph has no such property:
///   sampled at a fractional offset it is a blurred copy of itself, so its
///   origin **and** its extent are snapped, the extent to the exact texel count
///   the atlas allocated.
///
/// This is what makes the atlas's edge-aligned UV convention actually hold: it
/// promises that a 1:1, pixel-aligned quad samples texel centres, and nothing
/// else in the pipeline was arranging for the quad to be either.
fn snap_glyph_quad(
    bounds: Rect<Px>,
    pen: Point<Px>,
    placement: &GlyphPlacement,
    translation: Size<Px>,
    scale: f32,
) -> Rect<Px> {
    if scale <= 0.0 {
        return bounds;
    }
    // Work in the device space the vertex stage will land in, which is the only
    // space where "the pixel grid" means anything.
    let device_x = (bounds.min_x().get() + translation.width.get()) * scale;
    let device_y = (bounds.min_y().get() + translation.height.get()) * scale;

    // A bitmap's ink box is already a whole number of device pixels from the pen
    // — `rasterize_shape` snaps it outwards on purpose — so rounding the quad
    // directly is both grid-exact and consistent across the run, and this is the
    // only case where the extent may be overridden.
    if placement.is_bitmap {
        return Rect::new(
            Point::new(
                Px(device_x.round() / scale - translation.width.get()),
                Px(device_y.round() / scale - translation.height.get()),
            ),
            Size::new(
                Px(placement.texel_size[0] as f32 / scale),
                Px(placement.texel_size[1] as f32 / scale),
            ),
        );
    }

    // A field's padded box sits a fractional, glyph-dependent distance above the
    // baseline, so rounding each quad's own top edge would give every glyph in
    // the run a different sub-pixel shift. The pen is what they share, so that
    // is what gets rounded, and the whole quad moves by the same delta. The size
    // is left exactly as the field requires.
    let baseline = (pen.y.get() + translation.height.get()) * scale;
    let dy = (baseline.round() - baseline) / scale;
    Rect::new(Point::new(bounds.min_x(), Px(bounds.min_y().get() + dy)), bounds.size)
}

/// Quantises a horizontal device-space pen to the nearest quarter pixel.
///
/// Returning the integral part separately is important for negative positions:
/// `-0.25` is represented as `(-1, 3)`, not `(0, -1)`. Exact eighth-pixel ties
/// round away from zero, matching Rust's `f32::round` and the established egui
/// binning convention.
fn quarter_pixel_bin(position: f32) -> (i32, u8) {
    if !position.is_finite() {
        return (0, 0);
    }
    let quarters = (position * 4.0).round() as i32;
    (quarters.div_euclid(4), quarters.rem_euclid(4) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canvas::Canvas;
    use spherekit_core::{Gradient, ScaleFactor, px, rect, size as size2};

    /// A glyph provider that hands back a fixed placement, so batching and
    /// culling can be tested without a font stack.
    struct StubGlyphs {
        page: u32,
        calls: usize,
    }
    impl GlyphProvider for StubGlyphs {
        fn place_glyph(&mut self, r: GlyphRequest) -> Option<GlyphPlacement> {
            self.calls += 1;
            Some(GlyphPlacement {
                // Alternate pages so page-switch batching is exercised.
                page: if self.page == 0 { 0 } else { (r.glyph.0 as u32) % self.page },
                uv: [0.0, 0.0, 0.1, 0.1],
                bounds_em: [0.0, -0.8, 0.6, 0.8],
                range_em: 0.1,
                is_bitmap: false,
                is_subpixel: false,
                texel_size: [24, 32],
            })
        }
    }

    struct StubTextures(bool);
    impl TextureProvider for StubTextures {
        fn texture_for(&mut self, _: ImageId) -> Option<TextureId> {
            self.0.then(|| TextureId::new(0, 1))
        }
    }

    fn scene() -> Scene {
        Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::IDENTITY)
    }

    fn compile(s: &Scene) -> CompiledFrame {
        let mut c = BatchCompiler::new();
        c.compile(s, &mut StubGlyphs { page: 0, calls: 0 }, &mut StubTextures(true)).clone()
    }

    #[test]
    fn a_run_of_plain_rects_becomes_a_single_batch() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            for i in 0..500 {
                c.fill_rect(rect(px(i as f32), px(0.0), px(1.0), px(10.0)), Color::RED);
            }
        }
        let f = compile(&s);
        assert_eq!(f.quads.len(), 500);
        assert_eq!(f.batches.len(), 1, "500 compatible quads must be one draw call");
        assert_eq!(f.batches[0].range, 0..500);
    }

    #[test]
    fn changing_the_clip_starts_a_new_batch() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.save();
            c.clip_rect(rect(px(0.0), px(0.0), px(50.0), px(50.0)));
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::BLUE);
            c.restore();
        }
        let f = compile(&s);
        assert_eq!(f.batches.len(), 2, "a scissor change is a state change");
    }

    #[test]
    fn painters_order_is_never_reordered_across_kinds() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.draw_image(
                ImageId::new(0, 1),
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                spherekit_core::ImageFit::Fill,
            );
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::BLUE);
        }
        let f = compile(&s);
        // Merging the two quads across the image would draw the image on top.
        assert_eq!(f.batches.len(), 3);
        assert!(matches!(f.batches[0].kind, BatchKind::Quad));
        assert!(matches!(f.batches[1].kind, BatchKind::Image { .. }));
        assert!(matches!(f.batches[2].kind, BatchKind::Quad));
    }

    #[test]
    fn offscreen_primitives_are_culled_before_they_cost_an_instance() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.fill_rect(rect(px(-5000.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.fill_rect(rect(px(5000.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.fill_rect(rect(px(0.0), px(-5000.0), px(10.0), px(10.0)), Color::RED);
        }
        let f = compile(&s);
        assert_eq!(f.quads.len(), 1);
        assert_eq!(f.stats.culled, 3);
    }

    #[test]
    fn content_clipped_entirely_away_is_culled() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.clip_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)));
            c.fill_rect(rect(px(500.0), px(500.0), px(10.0), px(10.0)), Color::RED);
        }
        let f = compile(&s);
        assert_eq!(f.quads.len(), 0);
        assert_eq!(f.stats.culled, 1);
    }

    #[test]
    fn a_shadow_is_not_culled_when_only_its_blur_reaches_the_viewport() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            // The shape sits just off the left edge, but a 40 px blur spills in.
            c.draw_shadow(
                spherekit_core::RoundedRect::uniform(
                    rect(px(-20.0), px(100.0), px(15.0), px(50.0)),
                    px(4.0),
                ),
                &spherekit_core::Shadow {
                    offset: size2(px(0.0), px(0.0)),
                    blur_radius: px(40.0),
                    spread: Px::ZERO,
                    color: Color::BLACK,
                    inset: false,
                },
            );
        }
        let f = compile(&s);
        assert_eq!(f.quads.len(), 1, "the visible blur tail was wrongly culled");
        assert!(f.quads[0].flags & quad_flags::SHADOW != 0);
        assert!(f.quads[0].blur_sigma > 0.0);
    }

    #[test]
    fn clip_chains_resolve_to_the_intersected_scissor() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.clip_rect(rect(px(0.0), px(0.0), px(100.0), px(100.0)));
            c.clip_rect(rect(px(50.0), px(50.0), px(200.0), px(200.0)));
            c.fill_rect(rect(px(50.0), px(50.0), px(10.0), px(10.0)), Color::RED);
        }
        let f = compile(&s);
        let sc = f.batches[0].scissor;
        assert_eq!(sc.min_x(), DevicePx(50));
        assert_eq!(sc.min_y(), DevicePx(50));
        assert_eq!(sc.width(), DevicePx(50));
        assert_eq!(sc.height(), DevicePx(50));
    }

    #[test]
    fn a_rounded_clip_marks_instances_for_shader_evaluation() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.clip_rounded_rect(spherekit_core::RoundedRect::uniform(
                rect(px(0.0), px(0.0), px(100.0), px(100.0)),
                px(8.0),
            ));
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
        }
        let f = compile(&s);
        assert!(f.quads[0].flags & quad_flags::CLIP_ROUNDED != 0);
        assert!(f.clips[f.quads[0].clip_index as usize].radii[0] > 0.0);
    }

    #[test]
    fn a_plain_rect_clip_does_not_pay_for_shader_clipping() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.clip_rect(rect(px(0.0), px(0.0), px(100.0), px(100.0)));
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
        }
        let f = compile(&s);
        assert_eq!(f.quads[0].flags & quad_flags::CLIP_ROUNDED, 0);
    }

    #[test]
    fn a_layer_produces_an_offscreen_target_and_a_composite() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.push_opacity_layer(rect(px(0.0), px(0.0), px(100.0), px(100.0)), 0.5);
            c.fill_rect(rect(px(0.0), px(0.0), px(50.0), px(50.0)), Color::BLUE);
            c.end_layer();
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::GREEN);
        }
        let f = compile(&s);
        assert_eq!(f.targets.len(), 2, "one surface plus one offscreen layer");
        assert!(!f.targets[1].is_surface);
        let composite = f.passes.iter().find_map(|p| p.composite.as_ref()).expect("a composite");
        assert_eq!(composite.source, 1);
        assert_eq!(composite.destination, 0);
        assert!((composite.opacity - 0.5).abs() < 1e-6);
        // The surface must be cleared exactly once, by its first pass.
        assert_eq!(f.passes.iter().filter(|p| p.target == 0 && p.clear).count(), 1);
    }

    /// A layer's coordinate space has to sit on the device grid, because
    /// `snap_glyph_quad` rounds glyphs in absolute device coordinates and knows
    /// nothing about which target they will land in. A fractional target origin
    /// would shift every snapped glyph inside the layer off its texel and take
    /// the whole bitmap path through the atlas's linear sampler.
    #[test]
    fn a_layers_origin_and_extent_land_on_whole_device_pixels() {
        for (scale, origin, size) in
            [(1.0f32, 10.25f32, 40.5f32), (1.5, 10.25, 40.5), (2.0, 7.3, 33.1), (1.25, 0.6, 100.9)]
        {
            let mut s = Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::new(scale));
            {
                let mut c = Canvas::new(&mut s);
                c.push_opacity_layer(rect(px(origin), px(origin), px(size), px(size)), 0.5);
                c.fill_rect(rect(px(origin), px(origin), px(4.0), px(4.0)), Color::BLUE);
                c.end_layer();
            }
            let f = compile(&s);
            let target = f.targets[1];
            let composite = f.passes.iter().find_map(|p| p.composite.as_ref()).expect("composite");

            for (name, v) in
                [("origin.x", target.origin.x.get()), ("origin.y", target.origin.y.get())]
            {
                let device = v * scale;
                assert!(
                    (device - device.round()).abs() < 1e-3,
                    "scale {scale}: target {name} is {device} device px"
                );
            }

            // And the destination is exactly the texture, so the blit is 1:1
            // rather than a resample of a `round_out`-sized texture into a
            // fractional rect.
            let w = composite.bounds.width().get() * scale;
            let h = composite.bounds.height().get() * scale;
            assert!(
                (w - target.size.width.get() as f32).abs() < 1e-3,
                "scale {scale}: composite is {w} px wide for a {} px texture",
                target.size.width.get()
            );
            assert!((h - target.size.height.get() as f32).abs() < 1e-3, "scale {scale}");
            assert_eq!(composite.bounds.min_x(), target.origin.x);
            assert_eq!(composite.bounds.min_y(), target.origin.y);
        }
    }

    /// The clamp can shrink an oversized layer, and the composite has to follow
    /// it — describing the destination with the unclamped rect would stretch the
    /// texture across it.
    #[test]
    fn a_clamped_layer_still_composites_one_texel_to_one_pixel() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.push_opacity_layer(rect(px(0.0), px(0.0), px(500_000.0), px(500_000.0)), 0.5);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.end_layer();
        }
        let f = compile(&s);
        let target = f.targets[1];
        let composite = f.passes.iter().find_map(|p| p.composite.as_ref()).expect("composite");
        assert_eq!(composite.bounds.width().get(), target.size.width.get() as f32);
        assert_eq!(composite.bounds.height().get(), target.size.height.get() as f32);
    }

    #[test]
    fn an_offscreen_target_is_capped_so_a_runaway_layer_cannot_allocate_gigabytes() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.push_opacity_layer(rect(px(0.0), px(0.0), px(500_000.0), px(500_000.0)), 0.5);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.end_layer();
        }
        let f = compile(&s);
        let t = f.targets[1];
        assert!(t.size.width.get() <= 800 * 2 + 1, "{t:?}");
        assert!(t.size.height.get() <= 600 * 2 + 1, "{t:?}");
    }

    #[test]
    fn glyphs_batch_per_atlas_page() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            let glyphs = (0u16..6)
                .map(|i| crate::scene::PositionedGlyph {
                    glyph: GlyphId(i),
                    position: Point::new(px(i as f32 * 10.0), px(20.0)),
                })
                .collect();
            c.draw_glyph_run(
                crate::scene::GlyphRun {
                    font: FontId::new(0, 1),
                    font_size: px(16.0),
                    glyphs,
                    raster: TextRasterMode::Mtsdf,
                    outline_width: Px::ZERO,
                    outline_color: Color::TRANSPARENT,
                    coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                },
                Color::WHITE,
            );
        }
        let mut c = BatchCompiler::new();
        // Two pages, alternating by glyph index.
        let f = c.compile(&s, &mut StubGlyphs { page: 2, calls: 0 }, &mut StubTextures(true));
        assert_eq!(f.glyphs.len(), 6);
        assert!(f.batches.len() > 1, "a page switch must split the batch");
        assert!(f.batches.iter().all(|b| matches!(b.kind, BatchKind::Glyph { .. })));
    }

    /// A provider that reports a bitmap glyph of a known texel size.
    /// Two glyphs whose ink tops differ, which is the case that catches a
    /// snapping rule applied per glyph instead of per run. Both sit on the same
    /// baseline, as every glyph in a real run does.
    struct StubVariedGlyphs;
    impl GlyphProvider for StubVariedGlyphs {
        fn place_glyph(&mut self, r: GlyphRequest) -> Option<GlyphPlacement> {
            // Deliberately fractional: an `x`-height glyph and an ascender.
            let top = if r.glyph.0 == 1 { -0.517 } else { -0.733 };
            Some(GlyphPlacement {
                page: 0,
                uv: [0.0, 0.0, 0.1, 0.1],
                bounds_em: [0.0, top, 0.6, -top],
                range_em: 0.1,
                is_bitmap: false,
                is_subpixel: false,
                texel_size: [24, 32],
            })
        }
    }

    struct StubBitmapGlyphs;
    impl GlyphProvider for StubBitmapGlyphs {
        fn place_glyph(&mut self, _: GlyphRequest) -> Option<GlyphPlacement> {
            Some(GlyphPlacement {
                page: 0,
                uv: [0.0, 0.0, 0.1, 0.1],
                // Deliberately awkward: 0.37 em at 11 px is 4.07 device px, so
                // an unsnapped quad lands off the grid and off the texel count.
                bounds_em: [0.13, -0.61, 0.37, 0.72],
                range_em: 0.0,
                is_bitmap: true,
                is_subpixel: false,
                texel_size: [4, 8],
            })
        }
    }

    /// Mirrors the compiler's subpixel request into the placement so tests can
    /// observe both the request-side eligibility guard and the batch it selects.
    #[derive(Default)]
    struct EchoSubpixelGlyphs {
        requests: Vec<GlyphRequest>,
    }

    impl GlyphProvider for EchoSubpixelGlyphs {
        fn place_glyph(&mut self, request: GlyphRequest) -> Option<GlyphPlacement> {
            let is_subpixel = request.subpixel;
            self.requests.push(request);
            Some(GlyphPlacement {
                page: u32::from(is_subpixel),
                uv: [0.0, 0.0, 0.1, 0.1],
                bounds_em: [0.0, -0.75, 0.5, 0.75],
                range_em: 0.0,
                is_bitmap: true,
                is_subpixel,
                texel_size: [6, 9],
            })
        }
    }

    fn glyph_run_at(x: f32, y: f32, size: f32) -> Scene {
        let mut s = Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::IDENTITY);
        {
            let mut c = Canvas::new(&mut s);
            c.draw_glyph_run(
                crate::scene::GlyphRun {
                    font: FontId::new(0, 1),
                    font_size: px(size),
                    glyphs: smallvec::smallvec![crate::scene::PositionedGlyph {
                        glyph: GlyphId(1),
                        position: Point::new(px(x), px(y)),
                    }],
                    raster: TextRasterMode::Auto,
                    outline_width: Px::ZERO,
                    outline_color: Color::TRANSPARENT,
                    coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                },
                Color::WHITE,
            );
        }
        s
    }

    #[test]
    fn a_bitmap_glyph_is_snapped_to_the_device_grid_and_to_its_texel_count() {
        // The atlas promises edge-aligned UVs sample texel centres *if* the quad
        // is 1:1 and pixel-aligned. Without this snap that promise is broken and
        // every small glyph is a blurred copy of itself.
        let s = glyph_run_at(10.3, 20.7, 11.0);
        let mut c = BatchCompiler::new();
        let f = c.compile(&s, &mut StubBitmapGlyphs, &mut StubTextures(true));

        let g = &f.glyphs[0];
        assert_eq!(g.bounds[0].fract(), 0.0, "x was not snapped: {}", g.bounds[0]);
        assert_eq!(g.bounds[1].fract(), 0.0, "y was not snapped: {}", g.bounds[1]);
        assert_eq!(g.bounds[2], 4.0, "width must equal the texel count exactly");
        assert_eq!(g.bounds[3], 8.0, "height must equal the texel count exactly");
    }

    #[test]
    fn quarter_pixel_bins_round_consistently_on_both_sides_of_zero() {
        assert_eq!(quarter_pixel_bin(10.12), (10, 0));
        assert_eq!(quarter_pixel_bin(10.13), (10, 1));
        assert_eq!(quarter_pixel_bin(10.49), (10, 2));
        assert_eq!(quarter_pixel_bin(10.76), (10, 3));
        assert_eq!(quarter_pixel_bin(10.99), (11, 0));
        assert_eq!(quarter_pixel_bin(-0.12), (0, 0));
        assert_eq!(quarter_pixel_bin(-0.13), (-1, 3));
        assert_eq!(quarter_pixel_bin(-0.49), (-1, 2));
        assert_eq!(quarter_pixel_bin(-0.76), (-1, 1));
        assert_eq!(quarter_pixel_bin(-0.99), (-1, 0));
    }

    #[test]
    fn eligible_bitmap_text_requests_rgb_coverage_and_carries_its_phase() {
        let s = glyph_run_at(10.3, 20.0, 11.0);
        let mut compiler = BatchCompiler::new();
        compiler.set_subpixel_text_enabled(true);
        let mut glyphs = EchoSubpixelGlyphs::default();
        let frame = compiler.compile(&s, &mut glyphs, &mut StubTextures(true));

        assert_eq!(glyphs.requests.len(), 1);
        assert!(glyphs.requests[0].subpixel);
        assert_eq!(glyphs.requests[0].subpixel_phase, 1);
        assert_ne!(frame.glyphs[0].flags & glyph_flags::SUBPIXEL, 0);
        assert!(matches!(frame.batches[0].kind, BatchKind::Glyph { subpixel: true, .. }));
        // The phase is baked into the cached bitmap. Its quad remains aligned
        // 1:1 with atlas texels rather than being translated by the phase again.
        assert_eq!(frame.glyphs[0].bounds[0].fract(), 0.0);
    }

    #[test]
    fn rgb_coverage_is_disabled_inside_an_offscreen_layer() {
        let mut s = scene();
        {
            let mut canvas = Canvas::new(&mut s);
            canvas.push_opacity_layer(rect(px(0.0), px(0.0), px(100.0), px(100.0)), 0.5);
            canvas.draw_glyph_run(
                crate::scene::GlyphRun {
                    font: FontId::new(0, 1),
                    font_size: px(11.0),
                    glyphs: smallvec::smallvec![crate::scene::PositionedGlyph {
                        glyph: GlyphId(1),
                        position: Point::new(px(10.3), px(20.0)),
                    }],
                    raster: TextRasterMode::Bitmap,
                    outline_width: Px::ZERO,
                    outline_color: Color::TRANSPARENT,
                    coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                },
                Color::WHITE,
            );
            canvas.end_layer();
        }

        let mut compiler = BatchCompiler::new();
        compiler.set_subpixel_text_enabled(true);
        let mut glyphs = EchoSubpixelGlyphs::default();
        compiler.compile(&s, &mut glyphs, &mut StubTextures(true));

        assert_eq!(glyphs.requests.len(), 1);
        assert!(!glyphs.requests[0].subpixel);
        // Quarter positioning is safe for grayscale and remains enabled.
        assert_eq!(glyphs.requests[0].subpixel_phase, 1);
    }

    #[test]
    fn outlined_text_does_not_request_an_rgb_coverage_bitmap() {
        let mut s = glyph_run_at(10.3, 20.0, 11.0);
        s.runs[0].outline_width = px(1.0);
        s.runs[0].outline_color = Color::BLACK;

        let mut compiler = BatchCompiler::new();
        compiler.set_subpixel_text_enabled(true);
        let mut glyphs = EchoSubpixelGlyphs::default();
        compiler.compile(&s, &mut glyphs, &mut StubTextures(true));

        assert_eq!(glyphs.requests.len(), 1);
        assert!(!glyphs.requests[0].subpixel);
    }

    #[test]
    fn snapping_accounts_for_the_scale_factor() {
        // At 2x, a logical position of 10.25 is device 20.5 and must land on 20
        // or 21 — snapping in logical space would leave it at 20.5.
        let mut s = Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::new(2.0));
        {
            let mut c = Canvas::new(&mut s);
            c.draw_glyph_run(
                crate::scene::GlyphRun {
                    font: FontId::new(0, 1),
                    font_size: px(11.0),
                    glyphs: smallvec::smallvec![crate::scene::PositionedGlyph {
                        glyph: GlyphId(1),
                        position: Point::new(px(10.25), px(20.13)),
                    }],
                    raster: TextRasterMode::Auto,
                    outline_width: Px::ZERO,
                    outline_color: Color::TRANSPARENT,
                    coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                },
                Color::WHITE,
            );
        }
        let mut c = BatchCompiler::new();
        let f = c.compile(&s, &mut StubBitmapGlyphs, &mut StubTextures(true));
        let g = &f.glyphs[0];
        // Convert back to device space, which is where alignment matters.
        assert_eq!((g.bounds[0] * 2.0).fract(), 0.0, "device x = {}", g.bounds[0] * 2.0);
        assert_eq!((g.bounds[1] * 2.0).fract(), 0.0, "device y = {}", g.bounds[1] * 2.0);
    }

    #[test]
    fn a_distance_field_glyph_keeps_its_subpixel_x_but_snaps_its_baseline() {
        // A field reconstructs correctly at any subpixel x, and quantising x
        // would visibly quantise letter spacing at small sizes. The baseline
        // gains nothing from being fractional, so it is snapped -- but it is the
        // *baseline* that lands on the grid, not the quad's top edge, which sits
        // a fractional ink height above it.
        let s = glyph_run_at(10.3, 20.7, 24.0);
        let mut c = BatchCompiler::new();
        let f = c.compile(&s, &mut StubGlyphs { page: 0, calls: 0 }, &mut StubTextures(true));

        let g = &f.glyphs[0];
        assert_ne!(g.bounds[0].fract(), 0.0, "MTSDF x should keep its subpixel offset");

        // Recover the baseline the quad implies: top edge, plus the padding the
        // field adds, minus the ink top the stub reports.
        let (pad, ink_top) = (0.1 * 24.0, -0.8 * 24.0);
        let baseline = g.bounds[1] + pad - ink_top;
        assert!((baseline - baseline.round()).abs() < 1e-4, "baseline landed at {baseline}");
    }

    /// The two halves of the same rule, checked in *device* space at every
    /// scale factor a Windows display actually reports.
    ///
    /// Snapping the baseline in logical space would leave 20.5 device pixels
    /// alone at 2x and soften every glyph in the run identically; quantising x
    /// would clump letter spacing at exactly the sizes where the field is the
    /// only path available. The existing single-scale test covers 1x, where the
    /// two spaces coincide and the distinction is invisible.
    #[test]
    fn a_field_baseline_lands_on_a_physical_pixel_while_x_keeps_its_fraction() {
        for scale in [1.0f32, 1.25, 1.5, 2.0, 3.0] {
            let mut s = Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::new(scale));
            // Deliberately fractional in both axes, and fractional again once
            // multiplied by every scale above.
            let (pen_x, pen_y, size) = (10.3f32, 20.7f32, 24.0f32);
            {
                let mut c = Canvas::new(&mut s);
                c.draw_glyph_run(
                    crate::scene::GlyphRun {
                        font: FontId::new(0, 1),
                        font_size: px(size),
                        glyphs: smallvec::smallvec![crate::scene::PositionedGlyph {
                            glyph: GlyphId(1),
                            position: Point::new(px(pen_x), px(pen_y)),
                        }],
                        raster: TextRasterMode::Mtsdf,
                        outline_width: Px::ZERO,
                        outline_color: Color::TRANSPARENT,
                        coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                    },
                    Color::WHITE,
                );
            }
            let mut c = BatchCompiler::new();
            let f = c.compile(&s, &mut StubGlyphs { page: 0, calls: 0 }, &mut StubTextures(true));
            let g = &f.glyphs[0];

            // Recover the baseline the quad implies, in device pixels: the top
            // edge, plus the padding the field adds, minus the stub's ink top.
            let (pad, ink_top) = (0.1 * size, -0.8 * size);
            let baseline = (g.bounds[1] + pad - ink_top) * scale;
            assert!(
                (baseline - baseline.round()).abs() < 1e-3,
                "scale {scale}: baseline landed at {baseline} device px"
            );

            // And x is untouched, which is only meaningful because it was
            // fractional to begin with.
            let device_x = (g.bounds[0] + pad) * scale;
            assert!((g.bounds[0] + pad - pen_x).abs() < 1e-4, "scale {scale}: x moved");
            assert!(
                (device_x - device_x.round()).abs() > 1e-3,
                "scale {scale}: x was quantised to {device_x}"
            );
        }
    }

    /// The compensation keys off the size the glyph appears at, so the size has
    /// to travel with the instance. The vertex stage multiplies it by the
    /// transform's scale, exactly as it does `px_range`.
    #[test]
    fn a_field_glyph_carries_its_device_em_size() {
        for (scale, size) in [(1.0f32, 13.0f32), (2.0, 13.0), (1.5, 11.0), (1.0, 32.0)] {
            let mut s = Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::new(scale));
            {
                let mut c = Canvas::new(&mut s);
                c.draw_glyph_run(
                    crate::scene::GlyphRun {
                        font: FontId::new(0, 1),
                        font_size: px(size),
                        glyphs: smallvec::smallvec![crate::scene::PositionedGlyph {
                            glyph: GlyphId(1),
                            position: Point::new(px(10.0), px(30.0)),
                        }],
                        raster: TextRasterMode::Mtsdf,
                        outline_width: Px::ZERO,
                        outline_color: Color::TRANSPARENT,
                        coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                    },
                    Color::WHITE,
                );
            }
            let mut c = BatchCompiler::new();
            let f = c.compile(&s, &mut StubGlyphs { page: 0, calls: 0 }, &mut StubTextures(true));
            assert!(
                (f.glyphs[0].em_px - size * scale).abs() < 1e-4,
                "{size} px at {scale}x carried {}",
                f.glyphs[0].em_px
            );
        }
    }

    /// What "2x is unchanged" means, stated exactly rather than approximately.
    ///
    /// The ceiling is a *device* pixel threshold, so at 2x it falls at 12
    /// logical pixels. At or above that, forced `Mtsdf` renders bit-identically
    /// to what it rendered before the compensation existed. Below it — 10 and 11
    /// logical pixels, forced onto the field path against the engine's own
    /// advice — a small bias does apply, and it is bounded here so the size of
    /// the exception is on the record rather than assumed away.
    ///
    /// Under `TextRasterMode::Auto`, which is what every shipped widget uses,
    /// there is no exception at all: the strategy threshold is the same number
    /// as the ceiling, so no glyph the field path receives is ever below it.
    /// `spherekit_text::raster` holds that test, where both constants are
    /// visible at once.
    #[test]
    fn at_two_times_the_interface_type_scale_is_bit_identical() {
        let ramp = |logical: f32| {
            let em_px = logical * 2.0;
            let px_range = em_px * (4.0 / 48.0);
            (crate::scene::mtsdf_edge_ramp(em_px, px_range), px_range)
        };
        for logical in [12.0f32, 13.0, 14.0, 16.0, 20.0, 24.0] {
            let ([scale, offset], px_range) = ramp(logical);
            assert_eq!(offset, 0.0, "{logical} px at 2x was biased");
            assert!((scale - px_range.max(1.0)).abs() < 1e-6, "{logical} px at 2x: scale {scale}");
        }
        // The bounded exception, in device pixels of edge movement.
        for (logical, most) in [(10.0f32, 0.026f32), (11.0, 0.013)] {
            let ([scale, offset], px_range) = ramp(logical);
            let bias = offset / scale * px_range;
            assert!(bias <= most, "{logical} px at 2x moved the edge by {bias}");
        }
    }

    #[test]
    fn every_glyph_in_a_run_is_shifted_by_the_same_amount() {
        // The regression this exists for: rounding each quad's own top edge
        // gives an `x` and an `l` different sub-pixel shifts, because their ink
        // tops differ, and the shared baseline comes apart. The shift has to be
        // derived from the pen, which every glyph in the run has in common.
        let mut s = Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::IDENTITY);
        {
            let mut c = Canvas::new(&mut s);
            c.draw_glyph_run(
                crate::scene::GlyphRun {
                    font: FontId::new(0, 1),
                    font_size: px(24.0),
                    glyphs: smallvec::smallvec![
                        crate::scene::PositionedGlyph {
                            glyph: GlyphId(1),
                            position: Point::new(px(10.0), px(40.31)),
                        },
                        crate::scene::PositionedGlyph {
                            glyph: GlyphId(2),
                            position: Point::new(px(30.0), px(40.31)),
                        },
                    ],
                    raster: TextRasterMode::Auto,
                    outline_width: Px::ZERO,
                    outline_color: Color::TRANSPARENT,
                    coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                },
                Color::WHITE,
            );
        }
        let mut c = BatchCompiler::new();
        let f = c.compile(&s, &mut StubVariedGlyphs, &mut StubTextures(true));
        assert_eq!(f.glyphs.len(), 2);

        let pad = 0.1 * 24.0;
        let shift = |g: &GlyphInstance, ink_top: f32| g.bounds[1] - (40.31 + ink_top * 24.0 - pad);
        let a = shift(&f.glyphs[0], -0.517);
        let b = shift(&f.glyphs[1], -0.733);
        assert!((a - b).abs() < 1e-4, "glyphs shifted by {a} and {b}: the baseline is torn");
    }

    #[test]
    fn a_field_quad_covers_the_padded_image_not_the_ink_box() {
        // `generate_mtsdf` writes the ink box outset by the field's range on
        // every side, and says outright that the caller must draw that box. A
        // quad matching only the ink box squeezes the whole image into it, so
        // the glyph renders at roughly 75 % of its size while its advance stays
        // correct -- a line that is small and tracked out.
        let s = glyph_run_at(10.0, 40.0, 24.0);
        let mut c = BatchCompiler::new();
        let f = c.compile(&s, &mut StubGlyphs { page: 0, calls: 0 }, &mut StubTextures(true));

        // The stub reports a 0.6 x 0.8 em ink box with a 0.1 em range, at 24 px.
        let g = &f.glyphs[0];
        assert!((g.bounds[0] - (10.0 - 2.4)).abs() < 1e-4, "x {}", g.bounds[0]);
        assert!((g.bounds[2] - 0.8 * 24.0).abs() < 1e-4, "width {}", g.bounds[2]);
        assert!((g.bounds[3] - 1.0 * 24.0).abs() < 1e-4, "height {}", g.bounds[3]);
    }

    #[test]
    fn a_bitmap_quad_is_not_padded_because_coverage_has_no_range() {
        // The outset is unconditional, which is only correct because the bitmap
        // path reports `range_em: 0`. If that ever changes, a bitmap stops being
        // 1:1 with its texels and the snap above stops meaning anything.
        let s = glyph_run_at(10.0, 40.0, 11.0);
        let mut c = BatchCompiler::new();
        let f = c.compile(&s, &mut StubBitmapGlyphs, &mut StubTextures(true));

        let g = &f.glyphs[0];
        assert_eq!(g.bounds[2], 4.0, "width must still equal the texel count");
        assert_eq!(g.bounds[3], 8.0, "height must still equal the texel count");
        assert_eq!(g.px_range, 1e-3, "a bitmap carries no field range");
    }

    #[test]
    fn a_rotated_run_is_not_snapped_at_all() {
        // There is no pixel grid to snap to under rotation, and forcing one
        // would make text crawl as the transform animates.
        let mut s = Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::IDENTITY);
        {
            let mut c = Canvas::new(&mut s);
            c.rotate(spherekit_core::Deg(30.0).to_rad());
            c.draw_glyph_run(
                crate::scene::GlyphRun {
                    font: FontId::new(0, 1),
                    font_size: px(11.0),
                    glyphs: smallvec::smallvec![crate::scene::PositionedGlyph {
                        glyph: GlyphId(1),
                        position: Point::new(px(10.3), px(20.7)),
                    }],
                    raster: TextRasterMode::Auto,
                    outline_width: Px::ZERO,
                    outline_color: Color::TRANSPARENT,
                    coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                },
                Color::WHITE,
            );
        }
        let mut c = BatchCompiler::new();
        let f = c.compile(&s, &mut StubBitmapGlyphs, &mut StubTextures(true));
        let g = &f.glyphs[0];
        assert!(
            g.bounds[1].fract() != 0.0 || g.bounds[0].fract() != 0.0,
            "a rotated run must keep its exact geometry"
        );
    }

    #[test]
    fn glyph_px_range_scales_with_font_size_and_dpi() {
        let mut s = Scene::new(size2(px(800.0), px(600.0)), ScaleFactor::new(2.0));
        {
            let mut c = Canvas::new(&mut s);
            c.draw_glyph_run(
                crate::scene::GlyphRun {
                    font: FontId::new(0, 1),
                    font_size: px(20.0),
                    glyphs: smallvec::smallvec![crate::scene::PositionedGlyph {
                        glyph: GlyphId(1),
                        position: Point::new(px(10.0), px(30.0)),
                    }],
                    raster: TextRasterMode::Mtsdf,
                    outline_width: Px::ZERO,
                    outline_color: Color::TRANSPARENT,
                    coverage_contrast: crate::scene::FULL_COVERAGE_CONTRAST,
                },
                Color::WHITE,
            );
        }
        let f = compile(&s);
        // range_em 0.1 * size 20 * scale 2 = 4 destination pixels.
        assert!((f.glyphs[0].px_range - 4.0).abs() < 1e-4, "{}", f.glyphs[0].px_range);
    }

    #[test]
    fn an_unresolvable_image_is_skipped_not_fatal() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.draw_image(
                ImageId::new(7, 1),
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                spherekit_core::ImageFit::Fill,
            );
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
        }
        let mut c = BatchCompiler::new();
        let f = c.compile(&s, &mut StubGlyphs { page: 0, calls: 0 }, &mut StubTextures(false));
        assert_eq!(f.quads.len(), 1, "the rect must still draw");
        assert_eq!(f.stats.culled, 1);
    }

    #[test]
    fn filled_paths_tessellate_into_mesh_batches_with_valid_indices() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            let mut b = spherekit_core::PathBuilder::new();
            b.circle(Point::new(px(100.0), px(100.0)), px(40.0));
            c.fill_path(b.build(), Color::RED);
        }
        let f = compile(&s);
        assert!(!f.mesh_indices.is_empty());
        assert!(f.mesh_indices.iter().all(|i| (*i as usize) < f.mesh_vertices.len()));
        assert!(matches!(f.batches[0].kind, BatchKind::Mesh { texture: None }));
    }

    #[test]
    fn consecutive_meshes_merge_and_keep_their_indices_rebased() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            for i in 0..3 {
                let mut b = spherekit_core::PathBuilder::new();
                b.circle(Point::new(px(50.0 + i as f32 * 60.0), px(100.0)), px(20.0));
                c.fill_path(b.build(), Color::RED);
            }
        }
        let f = compile(&s);
        assert_eq!(f.batches.len(), 1, "three compatible meshes should be one draw");
        assert!(f.mesh_indices.iter().all(|i| (*i as usize) < f.mesh_vertices.len()));
        assert_eq!(f.batches[0].range, 0..f.mesh_indices.len() as u32);
    }

    #[test]
    fn transformed_meshes_are_baked_into_world_space() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.translate(size2(px(100.0), px(100.0)));
            let mut b = spherekit_core::PathBuilder::new();
            b.rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)));
            c.fill_path(b.build(), Color::RED);
        }
        let f = compile(&s);
        assert!(
            f.mesh_vertices.iter().all(|v| v.position[0] >= 99.0 && v.position[1] >= 99.0),
            "mesh vertices were not transformed"
        );
    }

    #[test]
    fn gradients_are_interned_once_per_paint() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            let g = Gradient::vertical(px(10.0), Color::RED, Color::BLUE);
            for i in 0..10 {
                c.fill_rect_with(
                    rect(px(i as f32 * 12.0), px(0.0), px(10.0), px(10.0)),
                    Brush::Gradient(g.clone()),
                );
            }
        }
        let f = compile(&s);
        // Each fill_rect_with records its own paint, so ten entries is correct;
        // what must NOT happen is a gradient uploaded twice for one paint.
        assert_eq!(f.gradients.len(), 10);
        let idx: Vec<u32> = f.quads.iter().map(|q| q.gradient).collect();
        assert!(idx.iter().all(|i| *i != u32::MAX));
        assert_eq!(idx.len(), idx.iter().collect::<std::collections::HashSet<_>>().len());
    }

    #[test]
    fn compiling_twice_reuses_buffers_and_yields_the_same_result() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            for i in 0..50 {
                c.fill_rect(rect(px(i as f32), px(0.0), px(1.0), px(10.0)), Color::RED);
            }
        }
        let mut c = BatchCompiler::new();
        let a =
            c.compile(&s, &mut StubGlyphs { page: 0, calls: 0 }, &mut StubTextures(true)).clone();
        let b =
            c.compile(&s, &mut StubGlyphs { page: 0, calls: 0 }, &mut StubTextures(true)).clone();
        assert_eq!(a.quads.len(), b.quads.len(), "second compile leaked state");
        assert_eq!(a.batches, b.batches);
    }

    #[test]
    fn an_empty_scene_compiles_to_nothing_without_panicking() {
        let s = scene();
        let f = compile(&s);
        assert!(f.is_empty());
        assert_eq!(f.upload_bytes(), (f.transforms.len() * 32 + f.clips.len() * 32) as u64);
    }

    #[test]
    fn a_zero_area_viewport_does_not_produce_a_zero_sized_target() {
        let s = Scene::new(size2(Px::ZERO, Px::ZERO), ScaleFactor::IDENTITY);
        let f = compile(&s);
        assert!(f.targets[0].size.width.get() >= 1);
        assert!(f.targets[0].size.height.get() >= 1);
    }

    #[test]
    fn an_unbalanced_layer_is_closed_by_the_compiler() {
        // The canvas normally guarantees balance, but a scene assembled by hand
        // must not leave content stranded in an offscreen texture.
        let mut s = scene();
        s.layers.push(Layer {
            bounds: rect(px(0.0), px(0.0), px(100.0), px(100.0)),
            opacity: 0.5,
            blend: BlendMode::Normal,
            filter: None,
            end_command: NO_INDEX,
        });
        s.commands.push(DrawCommand::BeginLayer { layer: 0 });
        s.commands.push(DrawCommand::Quad(QuadCommand {
            bounds: rect(px(0.0), px(0.0), px(10.0), px(10.0)),
            radii: Corners::ZERO,
            fill: NO_INDEX,
            border_color: Color::RED,
            border_width: px(1.0),
            transform: 0,
            clip: 0,
        }));
        let f = compile(&s);
        assert!(
            f.passes.iter().any(|p| p.composite.is_some()),
            "an unbalanced layer must still composite"
        );
    }

    #[test]
    fn stats_account_for_every_command() {
        let mut s = scene();
        {
            let mut c = Canvas::new(&mut s);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
            c.fill_rect(rect(px(-9999.0), px(0.0), px(10.0), px(10.0)), Color::RED);
        }
        let f = compile(&s);
        assert_eq!(f.stats.commands, 2);
        assert_eq!(f.stats.culled, 1);
        assert_eq!(f.stats.quads, 1);
        assert_eq!(f.stats.batches, 1);
    }
}
