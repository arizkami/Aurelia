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
    Px, Rect, RoundedRect, Size, Stroke, TextureId,
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
    /// Exponent applied to glyph coverage before compositing.
    ///
    /// Antialiasing coverage is a geometric quantity, and blending it in linear
    /// light — which is physically correct and what this engine does everywhere
    /// else — makes light-on-dark text bloom and read as soft. Fifty per cent
    /// coverage of white on black is linear 0.5, which is sRGB 0.735, noticeably
    /// heavier than the 0.5 a traditional gamma-space rasteriser produces.
    ///
    /// An exponent above 1.0 pulls the midtones back down and restores the
    /// weight the rasteriser intended. This is a *perceptual* correction, not a
    /// physical one, which is why it is a knob rather than a constant: dark text
    /// on a light background wants the opposite adjustment, and text over an
    /// image wants neither.
    ///
    /// [`DEFAULT_COVERAGE_GAMMA`] is a modest correction tuned for the
    /// light-on-dark case that dominates this engine's target applications.
    /// `1.0` disables it.
    pub coverage_gamma: f32,
}

/// The default glyph coverage exponent. See [`GlyphRun::coverage_gamma`].
pub const DEFAULT_COVERAGE_GAMMA: f32 = 1.25;

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
