//! GPU-ready primitive layouts.
//!
//! These structs are the contract between the batch compiler and every render
//! backend. They are `repr(C)` and `Pod`, so a `Vec<QuadInstance>` maps onto a
//! vertex buffer with one `cast_slice` and no per-element work.
//!
//! Two decisions shape the layout:
//!
//! * **Transforms and clips are indices, not inline matrices.** Sibling
//!   elements share both, so inlining a 6-float matrix and a clip rectangle in
//!   every instance would roughly double the bytes uploaded for a typical
//!   panel-heavy frame.
//! * **Colors are linear premultiplied.** Conversion happens once on the CPU
//!   while building the instance, not per fragment.

use spherekit_core::{Affine, Corners, LinearColor, Px, Rect, ScaleFactor};

/// Flag bits carried in [`QuadInstance::flags`].
pub mod quad_flags {
    /// The fill is a solid color in `color`.
    pub const FILL_SOLID: u32 = 1 << 0;
    /// The fill comes from the gradient table at `gradient`.
    pub const FILL_GRADIENT: u32 = 1 << 1;
    /// The instance samples a texture rather than filling.
    pub const FILL_TEXTURE: u32 = 1 << 2;
    /// The instance is a blurred shadow rather than a solid shape.
    pub const SHADOW: u32 = 1 << 3;
    /// The shadow is drawn inside the shape.
    pub const SHADOW_INSET: u32 = 1 << 4;
    /// The active clip is a rounded rectangle and must be evaluated per fragment.
    pub const CLIP_ROUNDED: u32 = 1 << 5;
    /// The instance has a visible border.
    pub const BORDER: u32 = 1 << 6;
}

/// Flag bits carried in [`GlyphInstance::flags`].
pub mod glyph_flags {
    /// The glyph samples a grayscale bitmap rather than a distance field.
    pub const BITMAP: u32 = 1 << 0;
    /// The glyph has a synthetic outline.
    pub const OUTLINE: u32 = 1 << 1;
    /// The active clip is a rounded rectangle.
    pub const CLIP_ROUNDED: u32 = 1 << 2;
    /// The glyph carries independent red, green and blue LCD coverage.
    ///
    /// Such an instance must run through the dual-source text pipeline; the
    /// ordinary premultiplied-alpha pipeline cannot preserve three independent
    /// destination blend factors.
    pub const SUBPIXEL: u32 = 1 << 3;
}

/// One instanced rectangle-family primitive.
///
/// Solid fills, gradients, borders, rounded corners and box shadows all share
/// this instance so they share one pipeline and one batch.
#[derive(Copy, Clone, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct QuadInstance {
    /// `[x, y, width, height]` in local logical pixels, before the transform.
    pub bounds: [f32; 4],
    /// Corner radii `[tl, tr, br, bl]`, already clamped against `bounds`.
    pub radii: [f32; 4],
    /// Linear premultiplied fill color, or the shadow color.
    pub color: [f32; 4],
    /// Linear premultiplied border color.
    pub border_color: [f32; 4],
    /// Border width in logical pixels, inset from `bounds`.
    pub border_width: f32,
    /// Gaussian sigma for shadows, zero otherwise.
    pub blur_sigma: f32,
    /// Bit set from [`quad_flags`].
    pub flags: u32,
    /// Index into the frame's gradient table, or `u32::MAX`.
    pub gradient: u32,
    /// Index into the frame's transform table.
    pub transform_index: u32,
    /// Index into the frame's clip table.
    pub clip_index: u32,
    /// Reserved, keeps the struct at a 16-byte multiple.
    pub _pad: [u32; 2],
}

const _: () = assert!(core::mem::size_of::<QuadInstance>() == 96);

impl QuadInstance {
    /// A fully transparent instance, used as a placeholder.
    pub const EMPTY: Self = Self {
        bounds: [0.0; 4],
        radii: [0.0; 4],
        color: [0.0; 4],
        border_color: [0.0; 4],
        border_width: 0.0,
        blur_sigma: 0.0,
        flags: 0,
        gradient: u32::MAX,
        transform_index: 0,
        clip_index: 0,
        _pad: [0; 2],
    };

    /// Builds a solid-filled instance.
    pub fn solid(
        bounds: Rect<Px>,
        radii: Corners<Px>,
        color: LinearColor,
        transform_index: u32,
        clip_index: u32,
    ) -> Self {
        Self {
            bounds: [
                bounds.min_x().get(),
                bounds.min_y().get(),
                bounds.width().get(),
                bounds.height().get(),
            ],
            radii: radii.clamp_for(bounds.size).to_array(),
            color: color.to_array(),
            flags: quad_flags::FILL_SOLID,
            transform_index,
            clip_index,
            ..Self::EMPTY
        }
    }
}

/// One instanced glyph quad.
#[derive(Copy, Clone, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct GlyphInstance {
    /// `[x, y, width, height]` of the glyph quad in local logical pixels.
    pub bounds: [f32; 4],
    /// `[u0, v0, u1, v1]` in normalised atlas coordinates.
    pub uv: [f32; 4],
    /// Linear premultiplied fill color.
    pub color: [f32; 4],
    /// Linear premultiplied outline color.
    pub outline_color: [f32; 4],
    /// The distance field's range, expressed in destination pixels.
    ///
    /// The shader needs this to convert a sampled distance into a screen-space
    /// coverage value. Passing it per instance rather than per pipeline is what
    /// lets one batch mix font sizes and still antialias every glyph correctly.
    pub px_range: f32,
    /// Outline half-width in distance-field units.
    pub outline_width: f32,
    /// Exponent applied to coverage; see [`crate::GlyphRun::coverage_contrast`].
    pub coverage_contrast: f32,
    /// Reserved float, keeping the four `u32`s below contiguous.
    ///
    /// The vertex layout reads the floats as one `vec4` and the indices as one
    /// `vec4<u32>`; interleaving them would mean a `u32` arriving in the shader
    /// reinterpreted as a float.
    pub _pad_f: f32,
    /// Bit set from [`glyph_flags`].
    pub flags: u32,
    /// Which atlas page to sample.
    pub atlas_page: u32,
    /// Index into the frame's transform table.
    pub transform_index: u32,
    /// Index into the frame's clip table.
    pub clip_index: u32,
}

const _: () = assert!(core::mem::size_of::<GlyphInstance>() == 96);

/// A clip region as the GPU sees it.
#[derive(Copy, Clone, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct GpuClip {
    /// `[min_x, min_y, max_x, max_y]` in device pixels.
    pub bounds: [f32; 4],
    /// Corner radii `[tl, tr, br, bl]`, zero for a plain rectangle.
    pub radii: [f32; 4],
}

const _: () = assert!(core::mem::size_of::<GpuClip>() == 32);

impl GpuClip {
    /// A clip that excludes nothing.
    pub const INFINITE: Self = Self { bounds: [-1.0e9, -1.0e9, 1.0e9, 1.0e9], radii: [0.0; 4] };
}

/// A transform as the GPU sees it, padded to two `vec4`s.
#[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct GpuTransform {
    /// The linear part `[a, b, c, d]`.
    pub matrix: [f32; 4],
    /// The translation `[tx, ty]` plus padding.
    pub translation: [f32; 4],
}

const _: () = assert!(core::mem::size_of::<GpuTransform>() == 32);

impl Default for GpuTransform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl GpuTransform {
    /// The identity transform.
    pub const IDENTITY: Self =
        Self { matrix: [1.0, 0.0, 0.0, 1.0], translation: [0.0, 0.0, 0.0, 0.0] };

    /// Converts a logical-space transform into device space.
    ///
    /// The scale factor is folded into the matrix here so the vertex shader
    /// works entirely in device pixels and never needs to know about HiDPI.
    pub fn from_affine(t: Affine, scale: ScaleFactor) -> Self {
        let s = scale.get();
        Self { matrix: [t.a, t.b, t.c, t.d], translation: [t.tx * s, t.ty * s, 0.0, 0.0] }
    }
}

/// The maximum number of stops a GPU gradient carries.
///
/// Eight covers every gradient a UI realistically uses; anything richer is
/// better expressed as an image. Fixing the count keeps the table a flat array
/// instead of a second level of indirection in the fragment shader.
pub const MAX_GRADIENT_STOPS: usize = 8;

/// Gradient kinds as encoded for the shader.
pub mod gradient_kind {
    /// Interpolates along a line.
    pub const LINEAR: u32 = 0;
    /// Interpolates radially outward.
    pub const RADIAL: u32 = 1;
    /// Interpolates around a centre.
    pub const SWEEP: u32 = 2;
}

/// A gradient as the GPU sees it.
#[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct GpuGradient {
    /// Start point for linear, centre for radial and sweep, plus the end point
    /// for linear or the radii for radial.
    pub geometry: [f32; 4],
    /// Stop offsets, padded with the last real offset.
    pub offsets: [f32; MAX_GRADIENT_STOPS],
    /// Stop colors, linear premultiplied.
    pub colors: [[f32; 4]; MAX_GRADIENT_STOPS],
    /// One of [`gradient_kind`].
    pub kind: u32,
    /// How many entries of `offsets` and `colors` are real.
    pub stop_count: u32,
    /// Reserved.
    pub _pad: [u32; 2],
}

impl Default for GpuGradient {
    fn default() -> Self {
        Self {
            geometry: [0.0; 4],
            offsets: [0.0; MAX_GRADIENT_STOPS],
            colors: [[0.0; 4]; MAX_GRADIENT_STOPS],
            kind: gradient_kind::LINEAR,
            stop_count: 0,
            _pad: [0; 2],
        }
    }
}

impl GpuGradient {
    /// Converts an authoring gradient, resampling when it has too many stops.
    ///
    /// Rather than dropping stops past the limit, the ramp is resampled at
    /// uniform intervals. Truncating would silently delete the tail of a
    /// gradient; resampling keeps the overall shape.
    pub fn from_gradient(g: &spherekit_core::Gradient) -> Self {
        use spherekit_core::Gradient;
        let mut out = Self::default();
        let stops = g.stops();

        if stops.len() <= MAX_GRADIENT_STOPS {
            out.stop_count = stops.len() as u32;
            for (i, s) in stops.iter().enumerate() {
                out.offsets[i] = s.offset;
                out.colors[i] = s.color.to_linear().to_array();
            }
            // Pad the tail so the shader can read a fixed count without a branch.
            if let Some(last) = stops.last() {
                for i in stops.len()..MAX_GRADIENT_STOPS {
                    out.offsets[i] = last.offset;
                    out.colors[i] = last.color.to_linear().to_array();
                }
            }
        } else {
            out.stop_count = MAX_GRADIENT_STOPS as u32;
            for i in 0..MAX_GRADIENT_STOPS {
                let t = i as f32 / (MAX_GRADIENT_STOPS - 1) as f32;
                out.offsets[i] = t;
                out.colors[i] = g.sample(t).to_linear().to_array();
            }
        }

        match g {
            Gradient::Linear { start, end, .. } => {
                out.kind = gradient_kind::LINEAR;
                out.geometry = [start.x.get(), start.y.get(), end.x.get(), end.y.get()];
            }
            Gradient::Radial { center, radius, .. } => {
                out.kind = gradient_kind::RADIAL;
                out.geometry = [
                    center.x.get(),
                    center.y.get(),
                    radius.width.get().max(1e-4),
                    radius.height.get().max(1e-4),
                ];
            }
            Gradient::Sweep { center, start_angle, sweep, .. } => {
                out.kind = gradient_kind::SWEEP;
                out.geometry = [center.x.get(), center.y.get(), *start_angle, *sweep];
            }
        }
        out
    }
}

/// Per-frame uniforms shared by every pipeline.
#[derive(Copy, Clone, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct FrameUniforms {
    /// Render target size in device pixels.
    pub viewport: [f32; 2],
    /// The surface's scale factor.
    pub scale_factor: f32,
    /// Seconds since the engine started, for animated shaders.
    pub time: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::SmallVec;
    use spherekit_core::{Color, Gradient, GradientStop, Point, Size, px, rect};

    #[test]
    fn instance_layouts_stay_sixteen_byte_aligned() {
        // Storage buffers and vertex strides both want a 16-byte multiple; a
        // stray field that breaks this costs a silent per-instance padding gap.
        assert_eq!(core::mem::size_of::<QuadInstance>() % 16, 0);
        assert_eq!(core::mem::size_of::<GlyphInstance>() % 16, 0);
        assert_eq!(core::mem::size_of::<GpuClip>() % 16, 0);
        assert_eq!(core::mem::size_of::<GpuTransform>() % 16, 0);
        assert_eq!(core::mem::size_of::<GpuGradient>() % 16, 0);
    }

    #[test]
    fn instances_are_castable_without_a_copy() {
        let v = vec![QuadInstance::EMPTY; 4];
        let bytes: &[u8] = bytemuck::cast_slice(&v);
        assert_eq!(bytes.len(), 4 * 96);
    }

    #[test]
    fn solid_instance_clamps_its_radii() {
        let q = QuadInstance::solid(
            rect(px(0.0), px(0.0), px(50.0), px(50.0)),
            Corners::all(px(40.0)),
            Color::RED.to_linear(),
            0,
            0,
        );
        assert!(q.radii[0] <= 25.0 + 1e-4, "{:?}", q.radii);
        assert_eq!(q.flags, quad_flags::FILL_SOLID);
    }

    #[test]
    fn transform_folds_the_scale_factor_into_translation_only() {
        // The linear part is scale-invariant because local extents are scaled
        // separately; folding scale into it twice would double-apply HiDPI.
        let t = GpuTransform::from_affine(
            Affine::translate(Size::new(px(10.0), px(20.0))),
            ScaleFactor::new(2.0),
        );
        assert_eq!(t.translation[0], 20.0);
        assert_eq!(t.translation[1], 40.0);
        assert_eq!(t.matrix, [1.0, 0.0, 0.0, 1.0]);
    }

    #[test]
    fn gradient_conversion_preserves_few_stops_exactly() {
        let g = Gradient::vertical(px(100.0), Color::RED, Color::BLUE);
        let gg = GpuGradient::from_gradient(&g);
        assert_eq!(gg.stop_count, 2);
        assert_eq!(gg.kind, gradient_kind::LINEAR);
        assert_eq!(gg.geometry, [0.0, 0.0, 0.0, 100.0]);
        assert_eq!(gg.colors[0], Color::RED.to_linear().to_array());
        assert_eq!(gg.colors[1], Color::BLUE.to_linear().to_array());
    }

    #[test]
    fn unused_gradient_slots_repeat_the_last_stop() {
        let g = Gradient::vertical(px(10.0), Color::RED, Color::BLUE);
        let gg = GpuGradient::from_gradient(&g);
        // A shader reading a fixed 8 slots must not sample garbage.
        for i in 2..MAX_GRADIENT_STOPS {
            assert_eq!(gg.offsets[i], 1.0);
            assert_eq!(gg.colors[i], Color::BLUE.to_linear().to_array());
        }
    }

    #[test]
    fn too_many_stops_are_resampled_not_truncated() {
        let mut stops: SmallVec<[GradientStop; 8]> = SmallVec::new();
        for i in 0..20 {
            let t = i as f32 / 19.0;
            stops.push(GradientStop::new(t, if i == 19 { Color::BLUE } else { Color::RED }));
        }
        let g =
            Gradient::Linear { start: Point::ZERO, end: Point::new(px(100.0), Px::ZERO), stops };
        let gg = GpuGradient::from_gradient(&g);
        assert_eq!(gg.stop_count, MAX_GRADIENT_STOPS as u32);
        assert_eq!(gg.offsets[0], 0.0);
        assert_eq!(gg.offsets[MAX_GRADIENT_STOPS - 1], 1.0);
        // The final color must survive; truncation would have dropped it.
        let last = gg.colors[MAX_GRADIENT_STOPS - 1];
        assert!(last[2] > last[0], "expected the blue tail to survive, got {last:?}");
    }

    #[test]
    fn radial_gradient_radii_never_reach_zero() {
        let g = Gradient::Radial {
            center: Point::ZERO,
            radius: Size::new(Px::ZERO, Px::ZERO),
            stops: SmallVec::from_slice(&[
                GradientStop::new(0.0, Color::RED),
                GradientStop::new(1.0, Color::BLUE),
            ]),
        };
        let gg = GpuGradient::from_gradient(&g);
        // A zero radius would divide by zero in the shader.
        assert!(gg.geometry[2] > 0.0 && gg.geometry[3] > 0.0);
    }
}
