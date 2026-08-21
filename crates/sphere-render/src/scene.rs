//! The scene: a resolved, flat display list for exactly one frame.
//!
//! Canvas calls never touch the GPU. They append to a [`Scene`], which the
//! batch compiler later turns into draw calls. Two properties make this worth
//! the indirection:
//!
//! * **Order is preserved, work is not duplicated.** The clip and transform
//!   stacks are resolved *while recording*, so each command carries a resolved
//!   index. Nothing re-walks a stack afterwards.
//! * **Commands stay small.** A [`DrawCommand`] is a fixed-size tagged record
//!   that indexes into side tables. Paths, gradients and glyph runs live in
//!   their own arenas, so a scene of ten thousand rectangles is ten thousand
//!   compact records and zero heap allocations per rectangle.

use smallvec::SmallVec;
use sphere_core::{
    Affine, BlendMode, Color, Corners, FillRule, GlyphId, ImageId, LinearColor, Paint, Path, Point,
    Px, Rect, RoundedRect, Size, Stroke, TextureId, linear_to_srgb, srgb_to_linear,
};

/// Index into a scene side table.
pub type SceneIndex = u32;

/// Sentinel meaning "no entry", used where an index is optional but must stay
/// `Copy` and fixed-size.
pub const NO_INDEX: SceneIndex = u32::MAX;

/// A clip region resolved to scene-absolute coordinates.
#[derive(Clone, Debug, PartialEq)]
pub struct Clip {
    /// The axis-aligned bound of the clip, always valid for a scissor rect.
    ///
    /// Even for rounded and path clips this stays correct as a conservative
    /// outer bound, so the backend can always scissor first and only apply the
    /// expensive part when [`Clip::kind`] demands it.
    pub bounds: Rect<Px>,
    /// What shape actually clips.
    pub kind: ClipKind,
    /// Index of the enclosing clip, or [`NO_INDEX`] at the root.
    pub parent: SceneIndex,
}

/// The shape of a clip region.
#[derive(Clone, Debug, PartialEq)]
pub enum ClipKind {
    /// A plain rectangle. Maps directly onto a scissor rect: no mask, no
    /// offscreen pass, no per-fragment work.
    Rect,
    /// A rounded rectangle, evaluated analytically in the fragment shader.
    Rounded(Corners<Px>),
    /// An arbitrary path, which requires a mask texture.
    Path {
        /// Index into [`Scene::paths`].
        path: SceneIndex,
        /// How the path's interior is determined.
        fill_rule: FillRule,
    },
}

impl Clip {
    /// The whole scene, unclipped.
    pub fn infinite() -> Self {
        Self { bounds: Rect::INFINITE, kind: ClipKind::Rect, parent: NO_INDEX }
    }

    /// True when this clip needs nothing beyond a scissor rectangle.
    #[inline]
    pub fn is_scissor_only(&self) -> bool {
        matches!(self.kind, ClipKind::Rect)
    }
}

/// A run of positioned glyphs sharing one font, size and paint.
#[derive(Clone, Debug, PartialEq)]
pub struct GlyphRun {
    /// The face these glyph indices belong to.
    pub font: sphere_core::FontId,
    /// Font size in logical pixels.
    pub font_size: Px,
    /// Positioned glyphs.
    pub glyphs: SmallVec<[PositionedGlyph; 8]>,
    /// How the glyphs should be rasterised.
    pub raster: TextRasterMode,
    /// Synthetic outline width, in logical pixels. Zero means no outline.
    pub outline_width: Px,
    /// Outline color, ignored when `outline_width` is zero.
    pub outline_color: Color,
    /// Signed exponent that steepens glyph coverage before compositing.
    ///
    /// Antialiasing coverage is a geometric fraction of a pixel, and Sphere
    /// blends it in linear light, because that is physically right and is what
    /// every other primitive here needs. Text is the one primitive it is wrong
    /// for. Half coverage of white on black is linear 0.5, which the surface
    /// encodes as sRGB **0.735** — so the one grey pixel beside a stem comes out
    /// nearly three quarters as bright as the stem itself, and a one-pixel edge
    /// reads as a two-pixel glow. That glow is what "soft text" is, and it is
    /// why every traditional rasteriser composites glyph coverage in gamma
    /// space instead (egui goes as far as asking for a non-sRGB framebuffer so
    /// its whole pipeline blends that way; see `docs/text.md`).
    ///
    /// The correction is to bend the coverage ramp so that what lands on screen
    /// *after* the linear blend and the sRGB encode is the ramp the rasteriser
    /// meant. Which way it has to bend depends on which side of the background
    /// the text sits:
    ///
    /// ```text
    /// light text on dark:  alpha = coverage ^ k
    /// dark text on light:  alpha = 1 - (1 - coverage) ^ k
    /// ```
    ///
    /// Those are mirror images, not one formula with an exponent above and
    /// below 1.0 — the blend is only linear in the *destination*, so the end of
    /// the ramp that needs bending is the end nearest the background. A single
    /// exponent applied in both directions cancels the error at one end and
    /// doubles it at the other.
    ///
    /// This field carries both: `|value|` is `k` and its sign picks the branch,
    /// positive for light-on-dark. `1.0` (or `-1.0`) disables the correction.
    /// [`coverage_contrast_for`] works the value out from the two colours, which
    /// is what callers should use.
    pub coverage_contrast: f32,
}

/// The exponent that cancels the sRGB encode outright, for black against white.
///
/// The surface is sRGB, whose transfer function is close enough to a 2.2 power
/// that raising coverage to 2.2 before a linear blend reproduces a gamma-space
/// blend to within a percent across the whole ramp; solving for the exponent
/// exactly at the extreme pair gives 2.224.
///
/// It is documentation, not the interpolation endpoint: [`coverage_contrast_for`]
/// solves the exponent from the two colours it is actually given, because how far
/// a pair travels through the curved part of the encode depends on *where* on the
/// ramp it sits and not only on how far apart the two ends are. Muted grey text
/// on a dark ground needs nearly the full exponent even though its contrast is
/// half that of white on black.
pub const FULL_COVERAGE_CONTRAST: f32 = 2.2;

/// The largest exponent [`coverage_contrast_for`] will return.
///
/// The solved exponent is unbounded as the two luminances converge from opposite
/// sides of the encode's knee, and a runaway value there would turn a low
/// contrast label into hard-edged aliasing. Three is above every real colour
/// pair — the extreme black-on-white case solves to 2.224 — so the clamp only
/// ever catches degenerate input.
const MAX_COVERAGE_CONTRAST: f32 = 3.0;

/// The coverage correction for text of one colour drawn on another.
///
/// See [`GlyphRun::coverage_contrast`] for what the number means. Positive for
/// light-on-dark, negative for dark-on-light, `1.0` when there is no contrast to
/// correct.
///
/// The exponent is *solved*, not interpolated: it is the one that puts half
/// coverage where a gamma-space rasteriser would put it, for this exact pair of
/// colours. Find the sRGB midpoint of the two, decode it, and read off the alpha
/// that a linear blend needs in order to land there; the exponent follows.
///
/// A pair-independent ramp — `1 + |delta| * (k - 1)` — is the obvious thing to
/// write and is wrong in the case that matters most. Muted grey text on a dark
/// ground has roughly half the luminance contrast of white on black but sits
/// almost entirely inside the steep part of the encode, so it needs nearly the
/// same exponent; interpolating hands it half of one, and secondary labels stay
/// soft while the primary ones come good.
///
/// Solving per pair also means it is right for a dark label on a light card
/// inside a dark theme, not only for whole-theme changes.
pub fn coverage_contrast_for(text: Color, background: Color) -> f32 {
    let (ink, ground) = (text.luminance(), background.luminance());
    let delta = ink - ground;
    // Text the same brightness as its background has no ramp to correct, and
    // dividing by that delta is what the guard is really for.
    if delta.abs() < 1e-4 {
        return 1.0;
    }

    // Where a gamma-space rasteriser puts half coverage: halfway between the two
    // in sRGB, expressed back in the linear light the blend actually works in.
    let target = srgb_to_linear((linear_to_srgb(ink) + linear_to_srgb(ground)) * 0.5);
    // The alpha a premultiplied linear blend needs to land there.
    let half = ((target - ground) / delta).clamp(1e-4, 1.0 - 1e-4);

    // Invert whichever branch of `alpha_from_coverage` this direction takes, at
    // `coverage = 0.5`. Both reduce to a ratio of logarithms.
    let bent = if delta > 0.0 { half } else { 1.0 - half };
    let k = (bent.ln() / 0.5f32.ln()).clamp(1.0, MAX_COVERAGE_CONTRAST);
    if delta > 0.0 { k } else { -k }
}

/// Applies [`GlyphRun::coverage_contrast`] on the CPU.
///
/// `text.wgsl` does exactly this on the GPU; anything that needs to predict what
/// the shader will produce — the `glyph_quad_probe` example, tests — must call
/// this rather than write the formula out a second time.
pub fn alpha_from_coverage(coverage: f32, contrast: f32) -> f32 {
    let c = coverage.clamp(0.0, 1.0);
    let k = contrast.abs();
    if !k.is_finite() || k <= 1.0 {
        return c;
    }
    if contrast > 0.0 { c.powf(k) } else { 1.0 - (1.0 - c).powf(k) }
}

/// One glyph placed at a baseline-relative position.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct PositionedGlyph {
    /// Glyph index within the run's face.
    pub glyph: GlyphId,
    /// Pen position of this glyph, on the baseline.
    pub position: Point<Px>,
}

/// How a glyph run is rasterised.
///
/// MTSDF is scale-independent and is the primary path. At very small physical
/// sizes the distance field runs out of resolution before the glyph runs out of
/// detail, so a grayscale bitmap fallback exists for the 10-to-13 pixel range
/// that DAW interfaces live in.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum TextRasterMode {
    /// Pick per glyph based on the final physical size.
    #[default]
    Auto,
    /// Always use the multi-channel + true distance field.
    Mtsdf,
    /// Always use a size-specific grayscale bitmap.
    Bitmap,
}

/// One drawing operation.
///
/// Every variant carries a `transform` and `clip` index that were resolved when
/// the command was recorded. Keeping the enum's payload small matters: a busy
/// DAW frame records tens of thousands of these.
#[derive(Clone, Debug, PartialEq)]
pub enum DrawCommand {
    /// A filled, optionally rounded and optionally bordered rectangle.
    ///
    /// This one variant covers the overwhelming majority of UI drawing, which
    /// is why it is a dedicated command rather than a degenerate path.
    Quad(QuadCommand),
    /// A filled path.
    FillPath {
        /// Index into [`Scene::paths`].
        path: SceneIndex,
        /// Index into [`Scene::paints`].
        paint: SceneIndex,
        /// How the interior is determined.
        fill_rule: FillRule,
        /// Resolved transform index.
        transform: SceneIndex,
        /// Resolved clip index.
        clip: SceneIndex,
    },
    /// A stroked path.
    StrokePath {
        /// Index into [`Scene::paths`].
        path: SceneIndex,
        /// Index into [`Scene::paints`].
        paint: SceneIndex,
        /// Index into [`Scene::strokes`].
        stroke: SceneIndex,
        /// Resolved transform index.
        transform: SceneIndex,
        /// Resolved clip index.
        clip: SceneIndex,
    },
    /// A run of text.
    Text {
        /// Index into [`Scene::runs`].
        run: SceneIndex,
        /// Index into [`Scene::paints`].
        paint: SceneIndex,
        /// Resolved transform index.
        transform: SceneIndex,
        /// Resolved clip index.
        clip: SceneIndex,
    },
    /// A textured quad.
    Image {
        /// Destination rectangle in local space.
        dest: Rect<Px>,
        /// Source sub-rectangle in normalised texture coordinates.
        source: Rect<Px>,
        /// The image to sample.
        image: ImageId,
        /// Multiplied over the sampled texels.
        tint: Color,
        /// Corner radii applied to the destination.
        radii: Corners<Px>,
        /// Resolved transform index.
        transform: SceneIndex,
        /// Resolved clip index.
        clip: SceneIndex,
    },
    /// A pre-tessellated mesh, the escape hatch for realtime audio primitives
    /// that generate their own geometry.
    Mesh {
        /// Index into [`Scene::meshes`].
        mesh: SceneIndex,
        /// Optional texture to sample; [`None`] means vertex color only.
        texture: Option<TextureId>,
        /// Resolved transform index.
        transform: SceneIndex,
        /// Resolved clip index.
        clip: SceneIndex,
    },
    /// A blurred shadow cast by a rounded rectangle.
    ///
    /// Box shadows are common enough, and analytically solvable enough, that
    /// routing them through the generic blur machinery would be wasteful.
    Shadow {
        /// The shape casting the shadow, already offset and spread.
        shape: RoundedRect,
        /// Blur radius in logical pixels.
        blur_radius: Px,
        /// Shadow color.
        color: Color,
        /// When true, the shadow is drawn inside the shape.
        inset: bool,
        /// Resolved transform index.
        transform: SceneIndex,
        /// Resolved clip index.
        clip: SceneIndex,
    },
    /// Begins an offscreen layer.
    ///
    /// Layers are expensive: each one is a render target allocation and an extra
    /// pass. The canvas only emits them when opacity, a non-fixed-function blend
    /// mode, or a filter actually requires compositing.
    BeginLayer {
        /// Index into [`Scene::layers`].
        layer: SceneIndex,
    },
    /// Ends the most recent [`DrawCommand::BeginLayer`] and composites it.
    EndLayer,
}

/// A rectangle-family primitive.
///
/// Solid fills, rounded rectangles, borders and gradient fills all share this
/// shape so they can share one instanced pipeline.
#[derive(Clone, Debug, PartialEq)]
pub struct QuadCommand {
    /// The rectangle in local space.
    pub bounds: Rect<Px>,
    /// Corner radii, not yet clamped.
    pub radii: Corners<Px>,
    /// Index into [`Scene::paints`] for the fill, or [`NO_INDEX`] for no fill.
    pub fill: SceneIndex,
    /// Border color. Ignored when `border_width` is zero.
    pub border_color: Color,
    /// Border width in logical pixels, drawn inset from `bounds`.
    pub border_width: Px,
    /// Resolved transform index.
    pub transform: SceneIndex,
    /// Resolved clip index.
    pub clip: SceneIndex,
}

/// An offscreen compositing group.
#[derive(Clone, Debug, PartialEq)]
pub struct Layer {
    /// The region the layer needs to cover, in scene-absolute coordinates.
    pub bounds: Rect<Px>,
    /// Opacity applied when compositing back.
    pub opacity: f32,
    /// Blend mode used when compositing back.
    pub blend: BlendMode,
    /// Optional effect applied before compositing.
    pub filter: Option<Filter>,
    /// Index of the command that ends this layer, filled in when the layer is
    /// closed. Lets the batch compiler skip a whole subtree in one step.
    pub end_command: SceneIndex,
}

/// A post-process applied to a layer before it composites back.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum Filter {
    /// Separable Gaussian blur.
    Blur {
        /// Standard deviation in logical pixels.
        sigma: Px,
    },
    /// Blurs what is *behind* the layer rather than its contents, the frosted
    /// glass effect.
    BackdropBlur {
        /// Standard deviation in logical pixels.
        sigma: Px,
    },
    /// A 4x5 color matrix in row-major order, the last column being the offset.
    ColorMatrix([f32; 20]),
    /// Scales saturation, where `0.0` is greyscale and `1.0` is unchanged.
    Saturate(f32),
}

/// Triangle geometry produced by the tessellator or by a realtime primitive.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Mesh {
    /// Vertices in local space.
    pub vertices: Vec<MeshVertex>,
    /// Triangle indices into `vertices`.
    pub indices: Vec<u32>,
}

impl Mesh {
    /// True when the mesh would produce no triangles.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.indices.is_empty() || self.vertices.is_empty()
    }

    /// The axis-aligned bounds of the mesh's vertices.
    pub fn bounds(&self) -> Rect<Px> {
        let Some(first) = self.vertices.first() else { return Rect::ZERO };
        let mut min = Point::new(Px(first.position[0]), Px(first.position[1]));
        let mut max = min;
        for v in &self.vertices[1..] {
            let p = Point::new(Px(v.position[0]), Px(v.position[1]));
            min = min.min(p);
            max = max.max(p);
        }
        Rect::from_corners(min, max)
    }
}

/// One tessellated vertex.
///
/// Colors are stored linear-premultiplied so the vertex stage does no
/// conversion work, and the layout is `repr(C)` so it maps straight onto a
/// vertex buffer without a copy.
#[derive(Copy, Clone, Debug, PartialEq, Default, bytemuck::Pod, bytemuck::Zeroable)]
#[repr(C)]
pub struct MeshVertex {
    /// Position in local logical pixels.
    pub position: [f32; 2],
    /// Texture coordinate, ignored when the mesh has no texture.
    pub uv: [f32; 2],
    /// Linear premultiplied color.
    pub color: [f32; 4],
}

impl MeshVertex {
    /// A vertex with no texture coordinate.
    #[inline]
    pub fn new(position: Point<Px>, color: LinearColor) -> Self {
        Self {
            position: [position.x.get(), position.y.get()],
            uv: [0.0, 0.0],
            color: color.to_array(),
        }
    }

    /// A textured vertex.
    #[inline]
    pub fn textured(position: Point<Px>, uv: [f32; 2], color: LinearColor) -> Self {
        Self { position: [position.x.get(), position.y.get()], uv, color: color.to_array() }
    }
}

/// A frame's worth of drawing, recorded but not yet submitted.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    /// Commands in painter's order. Order is significant and never reordered
    /// across incompatible commands.
    pub commands: Vec<DrawCommand>,
    /// Deduplicated transforms. Index 0 is always the identity.
    pub transforms: Vec<Affine>,
    /// Clip regions, forming a tree via [`Clip::parent`]. Index 0 is the
    /// infinite root clip.
    pub clips: Vec<Clip>,
    /// Paints referenced by commands.
    pub paints: Vec<Paint>,
    /// Stroke styles referenced by stroke commands.
    pub strokes: Vec<Stroke>,
    /// Paths referenced by path and clip commands.
    pub paths: Vec<Path>,
    /// Glyph runs referenced by text commands.
    pub runs: Vec<GlyphRun>,
    /// Meshes referenced by mesh commands.
    pub meshes: Vec<Mesh>,
    /// Offscreen layers.
    pub layers: Vec<Layer>,
    /// The viewport this scene was recorded for, in logical pixels.
    pub viewport: Size<Px>,
    /// The scale factor this scene was recorded for.
    pub scale_factor: sphere_core::ScaleFactor,
}

impl Scene {
    /// A new, empty scene for a viewport.
    pub fn new(viewport: Size<Px>, scale_factor: sphere_core::ScaleFactor) -> Self {
        Self {
            transforms: vec![Affine::IDENTITY],
            clips: vec![Clip::infinite()],
            viewport,
            scale_factor,
            ..Default::default()
        }
    }

    /// Clears the scene for reuse, keeping every allocation.
    ///
    /// Reusing one scene across frames is what keeps steady-state allocation at
    /// zero; a fresh `Scene` per frame would re-grow nine vectors every time.
    pub fn reset(&mut self, viewport: Size<Px>, scale_factor: sphere_core::ScaleFactor) {
        self.commands.clear();
        self.transforms.clear();
        self.transforms.push(Affine::IDENTITY);
        self.clips.clear();
        self.clips.push(Clip::infinite());
        self.paints.clear();
        self.strokes.clear();
        self.paths.clear();
        self.runs.clear();
        self.meshes.clear();
        self.layers.clear();
        self.viewport = viewport;
        self.scale_factor = scale_factor;
    }

    /// True when nothing was recorded.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Number of recorded commands.
    #[inline]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Interns a transform, reusing the previous entry when it repeats.
    ///
    /// Sibling elements overwhelmingly share a transform, so checking only the
    /// tail catches nearly every duplicate for the price of one comparison.
    /// A full hash map would cost more than it saves.
    pub fn intern_transform(&mut self, t: Affine) -> SceneIndex {
        if let Some(last) = self.transforms.last()
            && *last == t
        {
            return (self.transforms.len() - 1) as SceneIndex;
        }
        self.transforms.push(t);
        (self.transforms.len() - 1) as SceneIndex
    }

    /// Resolves a transform index, falling back to the identity.
    #[inline]
    pub fn transform(&self, i: SceneIndex) -> Affine {
        self.transforms.get(i as usize).copied().unwrap_or(Affine::IDENTITY)
    }

    /// Resolves a clip index.
    #[inline]
    pub fn clip(&self, i: SceneIndex) -> Option<&Clip> {
        self.clips.get(i as usize)
    }

    /// The scissor rectangle for a clip index, in logical pixels.
    ///
    /// Walks up the clip tree intersecting bounds, which is correct regardless
    /// of how the clip was expressed.
    pub fn scissor_for(&self, mut i: SceneIndex) -> Rect<Px> {
        let mut acc = Rect::INFINITE;
        let mut guard = 0;
        while let Some(c) = self.clips.get(i as usize) {
            acc = acc.intersection(c.bounds);
            if c.parent == NO_INDEX || c.parent == i {
                break;
            }
            i = c.parent;
            guard += 1;
            // A malformed clip tree must degrade to an over-large scissor, not
            // an infinite loop in the render path.
            debug_assert!(guard < 4096, "clip chain too deep or cyclic");
            if guard >= 4096 {
                break;
            }
        }
        acc
    }

    /// Approximate byte footprint, for the diagnostics overlay.
    pub fn memory_usage(&self) -> usize {
        use core::mem::size_of;
        self.commands.capacity() * size_of::<DrawCommand>()
            + self.transforms.capacity() * size_of::<Affine>()
            + self.clips.capacity() * size_of::<Clip>()
            + self.paints.capacity() * size_of::<Paint>()
            + self.strokes.capacity() * size_of::<Stroke>()
            + self.runs.capacity() * size_of::<GlyphRun>()
            + self.layers.capacity() * size_of::<Layer>()
            + self
                .meshes
                .iter()
                .map(|m| m.vertices.capacity() * size_of::<MeshVertex>() + m.indices.capacity() * 4)
                .sum::<usize>()
    }
}

/// Statistics gathered while recording and compiling a scene.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct SceneStats {
    /// Commands recorded.
    pub commands: u32,
    /// Commands discarded by the culler before batching.
    pub culled: u32,
    /// Batches produced.
    pub batches: u32,
    /// Quad instances emitted.
    pub quads: u32,
    /// Glyph instances emitted.
    pub glyphs: u32,
    /// Mesh triangles emitted.
    pub triangles: u32,
    /// Offscreen layers allocated.
    pub layers: u32,
}

#[cfg(test)]
mod tests {

    #[test]
    fn the_coverage_correction_reverses_between_light_and_dark_themes() {
        // The bug this exists for: applying the light-on-dark form to a light
        // theme does not merely fail to help, it bends the ramp the wrong way
        // and doubles the softness it was meant to remove.
        let on_dark = coverage_contrast_for(Color::WHITE, Color::BLACK);
        let on_light = coverage_contrast_for(Color::BLACK, Color::WHITE);
        assert!(on_dark > 0.0, "light on dark takes the positive branch: {on_dark}");
        assert!(on_light < 0.0, "dark on light takes the mirrored one: {on_light}");
        // The extreme pair is symmetric, and lands on the constant the docs cite.
        assert!((on_dark + on_light).abs() < 1e-3, "{on_dark} against {on_light}");
        assert!((on_dark - FULL_COVERAGE_CONTRAST).abs() < 0.05, "{on_dark}");
    }

    #[test]
    fn text_with_no_contrast_gets_no_correction() {
        // There is nothing to correct when there is nothing to see, and a
        // nonzero correction there would be an arbitrary thinning.
        let g = coverage_contrast_for(Color::WHITE, Color::WHITE);
        assert!((g.abs() - 1.0).abs() < 1e-4, "{g}");
        assert!((alpha_from_coverage(0.5, g) - 0.5).abs() < 1e-4);
    }

    #[test]
    fn the_correction_follows_the_colours_rather_than_switching_at_a_threshold() {
        // So a dark label on a light card inside a dark theme gets the right
        // answer, not the theme's answer.
        let strong = coverage_contrast_for(Color::WHITE, Color::BLACK);
        let weak = coverage_contrast_for(Color::WHITE, Color::hex(0x808080));
        assert!(weak > 1.0 && weak < strong, "weak {weak}, strong {strong}");
        // Dark on light mirrors, sign and all.
        let dark_on_light = coverage_contrast_for(Color::BLACK, Color::hex(0x808080));
        assert!(dark_on_light < -1.0, "{dark_on_light}");
    }

    /// The reason the exponent is solved per pair instead of interpolated from
    /// the luminance gap: the two are not the same function, and they disagree
    /// most on the secondary text every interface is full of.
    #[test]
    fn muted_text_needs_nearly_the_full_exponent_despite_half_the_contrast() {
        let ink = Color::hex(0x939BA8);
        let ground = Color::hex(0x14161A);
        let solved = coverage_contrast_for(ink, ground);
        let gap = ink.luminance() - ground.luminance();

        // Barely a third of the luminance gap of white on black...
        let full = Color::WHITE.luminance() - Color::BLACK.luminance();
        assert!(gap < 0.4 * full, "gap {gap} against {full}");
        // ...but nearly all of the correction, because the pair sits inside the
        // steep part of the encode. Interpolating on the gap would hand it
        // roughly 1.4 and leave every muted label soft.
        assert!(solved > 1.75, "solved {solved}");
    }

    /// What the corrected pixel actually shows, worked forward through the same
    /// steps the GPU takes: bend the coverage, blend premultiplied in linear
    /// light, let the sRGB surface encode the result.
    ///
    /// Compared against a gamma-space rasteriser, which is the thing being
    /// reproduced: it lerps the two colours in sRGB directly.
    fn shown(coverage: f32, ink: Color, ground: Color) -> (f32, f32) {
        let (i, g) = (ink.luminance(), ground.luminance());
        let alpha = alpha_from_coverage(coverage, coverage_contrast_for(ink, ground));
        let blended = linear_to_srgb(alpha * i + (1.0 - alpha) * g);
        let reference = linear_to_srgb(g) + coverage * (linear_to_srgb(i) - linear_to_srgb(g));
        (blended, reference)
    }

    /// The claim the whole correction rests on: what survives the linear blend
    /// and the sRGB encode is the ramp a gamma-space rasteriser would have drawn.
    #[test]
    fn the_correction_reproduces_a_gamma_space_blend() {
        // Every pair the shipped themes actually draw, plus the two extremes.
        let pairs = [
            ("white on black", Color::WHITE, Color::BLACK),
            ("black on white", Color::BLACK, Color::WHITE),
            ("dark theme", Color::hex(0xE6E9EF), Color::hex(0x14161A)),
            ("dark theme, muted", Color::hex(0x939BA8), Color::hex(0x14161A)),
            ("light theme", Color::hex(0x1A1D22), Color::hex(0xF5F6F8)),
            ("light theme, muted", Color::hex(0x606772), Color::hex(0xF5F6F8)),
        ];
        for (name, ink, ground) in pairs {
            let mut worst: f32 = 0.0;
            for step in 0..=32 {
                let coverage = step as f32 / 32.0;
                let (blended, reference) = shown(coverage, ink, ground);
                worst = worst.max((blended - reference).abs());
            }
            // Four per cent is ten levels out of 255, which is under what a
            // one-pixel edge can show. Before this correction the same pairs
            // were out by 21 to 24 per cent; see the test below.
            assert!(worst < 0.04, "{name}: worst error {worst}");
        }
    }

    /// The size of the problem being fixed, so the numbers in the doc comments
    /// are checked rather than asserted.
    #[test]
    fn an_uncorrected_linear_blend_lifts_the_midtones_by_a_sixth_of_the_ramp() {
        // Half coverage of white on black lands at sRGB 0.735, not 0.5. That
        // gap is the halo: the grey pixel beside a stem reads as three quarters
        // of the stem's own brightness.
        assert!((linear_to_srgb(0.5) - 0.735).abs() < 0.005);

        // And the 1.25 exponent this replaced closed a fifth of it.
        assert!((linear_to_srgb(0.5f32.powf(1.25)) - 0.680).abs() < 0.005);

        // Applied to the real dark theme, uncorrected, at half coverage: the
        // half-covered pixel shows at 0.673 where it should show at 0.499, so it
        // sits a sixth of the whole ramp too close to the ink.
        let (ink, ground) = (Color::hex(0xE6E9EF).luminance(), Color::hex(0x14161A).luminance());
        let uncorrected = linear_to_srgb(0.5 * ink + 0.5 * ground);
        let reference = (linear_to_srgb(ink) + linear_to_srgb(ground)) * 0.5;
        assert!(uncorrected - reference > 0.17, "{uncorrected} against {reference}");

        // And what the correction leaves: a hundredth of the ramp, at the same
        // point on the same pair.
        let (blended, corrected) = shown(0.5, Color::hex(0xE6E9EF), Color::hex(0x14161A));
        assert!((blended - corrected).abs() < 0.01, "{blended} against {corrected}");
    }

    #[test]
    fn a_disabled_or_malformed_correction_is_the_identity() {
        for contrast in [1.0, -1.0, 0.0, 0.5, f32::NAN, f32::INFINITY] {
            let a = alpha_from_coverage(0.3, contrast);
            assert!((a - 0.3).abs() < 1e-6, "contrast {contrast} gave {a}");
        }
    }

    #[test]
    fn the_correction_is_monotonic_and_pins_both_ends() {
        for contrast in [FULL_COVERAGE_CONTRAST, -FULL_COVERAGE_CONTRAST, 1.6, -1.6] {
            assert_eq!(alpha_from_coverage(0.0, contrast), 0.0, "contrast {contrast}");
            assert_eq!(alpha_from_coverage(1.0, contrast), 1.0, "contrast {contrast}");
            let mut previous = 0.0;
            for step in 0..=64 {
                let a = alpha_from_coverage(step as f32 / 64.0, contrast);
                assert!(a >= previous - 1e-6, "contrast {contrast} dipped at {step}");
                previous = a;
            }
        }
    }
    use super::*;
    use sphere_core::{ScaleFactor, px, rect, size};

    fn scene() -> Scene {
        Scene::new(size(px(800.0), px(600.0)), ScaleFactor::IDENTITY)
    }

    #[test]
    fn new_scene_has_identity_transform_and_root_clip_at_index_zero() {
        let s = scene();
        assert_eq!(s.transform(0), Affine::IDENTITY);
        assert!(s.clip(0).unwrap().is_scissor_only());
        assert_eq!(s.clip(0).unwrap().parent, NO_INDEX);
    }

    #[test]
    fn reset_keeps_capacity_but_restores_the_sentinels() {
        let mut s = scene();
        for _ in 0..100 {
            s.intern_transform(Affine::translate(size(px(1.0), px(1.0))));
            s.intern_transform(Affine::IDENTITY);
        }
        let cap = s.transforms.capacity();
        s.reset(size(px(10.0), px(10.0)), ScaleFactor::new(2.0));
        assert_eq!(s.transforms.len(), 1);
        assert_eq!(s.transforms[0], Affine::IDENTITY);
        assert_eq!(s.clips.len(), 1);
        assert_eq!(s.transforms.capacity(), cap, "reset must not release the allocation");
        assert_eq!(s.scale_factor.get(), 2.0);
    }

    #[test]
    fn interning_collapses_consecutive_duplicates() {
        let mut s = scene();
        let t = Affine::uniform_scale(2.0);
        let a = s.intern_transform(t);
        let b = s.intern_transform(t);
        assert_eq!(a, b);
        assert_eq!(s.transforms.len(), 2, "identity plus one");
    }

    #[test]
    fn interning_does_not_collapse_across_an_intervening_transform() {
        let mut s = scene();
        let a = s.intern_transform(Affine::uniform_scale(2.0));
        s.intern_transform(Affine::uniform_scale(3.0));
        let c = s.intern_transform(Affine::uniform_scale(2.0));
        assert_ne!(a, c, "order must be preserved even at the cost of a duplicate");
    }

    #[test]
    fn scissor_intersects_the_whole_clip_chain() {
        let mut s = scene();
        s.clips.push(Clip {
            bounds: rect(px(0.0), px(0.0), px(100.0), px(100.0)),
            kind: ClipKind::Rect,
            parent: 0,
        });
        s.clips.push(Clip {
            bounds: rect(px(50.0), px(50.0), px(100.0), px(100.0)),
            kind: ClipKind::Rect,
            parent: 1,
        });
        let sc = s.scissor_for(2);
        assert_eq!(sc, rect(px(50.0), px(50.0), px(50.0), px(50.0)));
    }

    #[test]
    fn scissor_of_disjoint_clips_is_empty_not_inverted() {
        let mut s = scene();
        s.clips.push(Clip {
            bounds: rect(px(0.0), px(0.0), px(10.0), px(10.0)),
            kind: ClipKind::Rect,
            parent: 0,
        });
        s.clips.push(Clip {
            bounds: rect(px(500.0), px(500.0), px(10.0), px(10.0)),
            kind: ClipKind::Rect,
            parent: 1,
        });
        let sc = s.scissor_for(2);
        assert!(sc.is_empty());
        assert!(sc.width() >= Px::ZERO && sc.height() >= Px::ZERO);
    }

    #[test]
    fn a_self_referential_clip_terminates() {
        let mut s = scene();
        s.clips.push(Clip {
            bounds: rect(px(0.0), px(0.0), px(10.0), px(10.0)),
            kind: ClipKind::Rect,
            parent: 1, // points at itself
        });
        let sc = s.scissor_for(1);
        assert_eq!(sc, rect(px(0.0), px(0.0), px(10.0), px(10.0)));
    }

    #[test]
    fn unknown_indices_degrade_instead_of_panicking() {
        let s = scene();
        assert_eq!(s.transform(9999), Affine::IDENTITY);
        assert!(s.clip(9999).is_none());
        assert_eq!(s.scissor_for(9999), Rect::INFINITE);
    }

    #[test]
    fn mesh_bounds_cover_every_vertex() {
        let m = Mesh {
            vertices: vec![
                MeshVertex::new(Point::new(px(-5.0), px(3.0)), LinearColor::TRANSPARENT),
                MeshVertex::new(Point::new(px(10.0), px(-2.0)), LinearColor::TRANSPARENT),
                MeshVertex::new(Point::new(px(1.0), px(20.0)), LinearColor::TRANSPARENT),
            ],
            indices: vec![0, 1, 2],
        };
        let b = m.bounds();
        assert_eq!(b.min_x(), px(-5.0));
        assert_eq!(b.min_y(), px(-2.0));
        assert_eq!(b.max_x(), px(10.0));
        assert_eq!(b.max_y(), px(20.0));
    }

    #[test]
    fn empty_mesh_is_reported_empty() {
        assert!(Mesh::default().is_empty());
        assert_eq!(Mesh::default().bounds(), Rect::ZERO);
    }

    #[test]
    fn command_record_stays_compact() {
        // A DrawCommand is stored by value in a hot Vec that a busy frame fills
        // with tens of thousands of entries. If this grows, look for a payload
        // that should have been moved into a side table.
        assert!(
            core::mem::size_of::<DrawCommand>() <= 128,
            "DrawCommand grew to {} bytes",
            core::mem::size_of::<DrawCommand>()
        );
    }
}
