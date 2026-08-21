//! Path tessellation.
//!
//! Turning an arbitrary path into triangles is a solved, fiddly problem, so
//! Sphere delegates it to `lyon` rather than reimplementing a sweep-line
//! tessellator. None of `lyon`'s types appear in this module's public API: the
//! seam is one function in and a [`Mesh`] out, so the backing implementation is
//! replaceable.
//!
//! The specialised primitives — rectangles, rounded rectangles, circles,
//! borders, shadows — deliberately never come through here. They go to the
//! analytic quad pipeline instead, which is both faster and sharper than any
//! tessellation of the same shape.

use crate::scene::{Mesh, MeshVertex};
use lyon::path::Path as LyonPath;
use lyon::path::math::point as lyon_point;
use lyon::tessellation as tess;
use sphere_core::{
    Affine, FillRule, LineCap, LineJoin, LinearColor, Path, PathEvent, Point, Px, Stroke,
};

/// Controls how finely curves are approximated.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TessellationOptions {
    /// Maximum distance, in device pixels, between the true curve and the
    /// tessellated approximation.
    ///
    /// A quarter of a device pixel is below what any display can resolve while
    /// keeping triangle counts sane. Tightening it further costs geometry for
    /// no visible gain.
    pub tolerance: f32,
    /// The transform's approximate scale, folded into the tolerance.
    ///
    /// Tessellating in local space at a fixed tolerance means a shape zoomed to
    /// 800 % shows its facets. Dividing the tolerance by the scale keeps the
    /// *screen-space* error constant, which is the thing that actually matters.
    pub scale: f32,
}

impl Default for TessellationOptions {
    fn default() -> Self {
        Self { tolerance: 0.25, scale: 1.0 }
    }
}

impl TessellationOptions {
    /// The tolerance to hand the tessellator, in local units.
    #[inline]
    pub fn local_tolerance(&self) -> f32 {
        (self.tolerance / self.scale.max(1e-3)).clamp(1e-4, 100.0)
    }
}

/// Why tessellation failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TessellationError {
    /// The tessellator rejected the geometry.
    #[error("tessellation failed: {0}")]
    Failed(String),
    /// The result exceeded what a `u32` index buffer can address.
    #[error("tessellated geometry exceeded the index limit ({0} vertices)")]
    TooManyVertices(usize),
}

/// A reusable tessellator.
///
/// `lyon`'s tessellators hold sizeable internal scratch buffers. Keeping one
/// instance alive across frames is what makes per-frame path tessellation
/// allocation-free in steady state.
pub struct Tessellator {
    fill: tess::FillTessellator,
    stroke: tess::StrokeTessellator,
    buffers: tess::VertexBuffers<MeshVertex, u32>,
    scratch: LyonPath,
}

impl Default for Tessellator {
    fn default() -> Self {
        Self::new()
    }
}

impl Tessellator {
    /// A new tessellator.
    pub fn new() -> Self {
        Self {
            fill: tess::FillTessellator::new(),
            stroke: tess::StrokeTessellator::new(),
            buffers: tess::VertexBuffers::new(),
            scratch: LyonPath::new(),
        }
    }

    /// Tessellates a filled path into a mesh with one flat color.
    pub fn fill_path(
        &mut self,
        path: &Path,
        rule: FillRule,
        color: LinearColor,
        options: TessellationOptions,
    ) -> Result<Mesh, TessellationError> {
        if path.is_empty() {
            return Ok(Mesh::default());
        }
        self.scratch = convert_path(path);
        self.buffers.vertices.clear();
        self.buffers.indices.clear();

        let opts = tess::FillOptions::default()
            .with_tolerance(options.local_tolerance())
            .with_fill_rule(match rule {
                FillRule::NonZero => tess::FillRule::NonZero,
                FillRule::EvenOdd => tess::FillRule::EvenOdd,
            });

        let rgba = color.to_array();
        let mut builder =
            tess::BuffersBuilder::new(&mut self.buffers, move |v: tess::FillVertex| MeshVertex {
                position: [v.position().x, v.position().y],
                uv: [0.0, 0.0],
                color: rgba,
            });

        self.fill
            .tessellate_path(&self.scratch, &opts, &mut builder)
            .map_err(|e| TessellationError::Failed(format!("{e:?}")))?;

        self.take_mesh()
    }

    /// Tessellates a stroked path into a mesh with one flat color.
    pub fn stroke_path(
        &mut self,
        path: &Path,
        stroke: &Stroke,
        color: LinearColor,
        options: TessellationOptions,
    ) -> Result<Mesh, TessellationError> {
        if path.is_empty() || stroke.is_invisible() {
            return Ok(Mesh::default());
        }
        self.scratch = convert_path(path);
        self.buffers.vertices.clear();
        self.buffers.indices.clear();

        let opts = tess::StrokeOptions::default()
            .with_tolerance(options.local_tolerance())
            .with_line_width(stroke.width.get())
            .with_miter_limit(stroke.miter_limit.max(1.0))
            .with_start_cap(convert_cap(stroke.cap))
            .with_end_cap(convert_cap(stroke.cap))
            .with_line_join(convert_join(stroke.join));

        let rgba = color.to_array();
        let mut builder =
            tess::BuffersBuilder::new(&mut self.buffers, move |v: tess::StrokeVertex| MeshVertex {
                position: [v.position().x, v.position().y],
                uv: [0.0, 0.0],
                color: rgba,
            });

        self.stroke
            .tessellate_path(&self.scratch, &opts, &mut builder)
            .map_err(|e| TessellationError::Failed(format!("{e:?}")))?;

        self.take_mesh()
    }

    fn take_mesh(&mut self) -> Result<Mesh, TessellationError> {
        if self.buffers.vertices.len() > u32::MAX as usize {
            return Err(TessellationError::TooManyVertices(self.buffers.vertices.len()));
        }
        Ok(Mesh {
            vertices: core::mem::take(&mut self.buffers.vertices),
            indices: core::mem::take(&mut self.buffers.indices),
        })
    }
}

/// Applies a transform to every vertex of a mesh in place.
///
/// Used when a mesh is baked into a batch whose transform cannot be expressed
/// as an index, such as content merged across differing parent transforms.
pub fn transform_mesh(mesh: &mut Mesh, t: Affine) {
    if t == Affine::IDENTITY {
        return;
    }
    for v in &mut mesh.vertices {
        let p = t.apply(Point::new(Px(v.position[0]), Px(v.position[1])));
        v.position = [p.x.get(), p.y.get()];
    }
}

/// Applies a dash pattern to a path, returning a new path of dash segments.
///
/// Done on the CPU before tessellation. Dashing in the shader would need arc
/// length along the stroke, which is not available once the path has become
/// triangles.
pub fn apply_dash(path: &Path, pattern: &[Px], offset: Px, tolerance: Px) -> Path {
    let widths: Vec<f32> = pattern.iter().map(|p| p.get().max(0.0)).filter(|p| *p > 0.0).collect();
    if widths.is_empty() {
        return path.clone();
    }
    let cycle: f32 = widths.iter().sum();
    if cycle <= 1e-6 {
        return path.clone();
    }

    let mut out = Path::builder();
    // `pos` is the distance travelled through the dash pattern, so a nonzero
    // offset starts mid-pattern the way SVG's `stroke-dashoffset` does.
    let mut pos = offset.get().rem_euclid(cycle);
    let mut idx = 0usize;
    while pos >= widths[idx] {
        pos -= widths[idx];
        idx = (idx + 1) % widths.len();
    }
    let mut drawing = idx.is_multiple_of(2);
    let mut pen_down = false;

    // Open subpaths must stay open: dashing a line from A to B must not also
    // dash the implicit return leg.
    path.flatten_open(tolerance, |a, b| {
        let mut seg_start = a;
        let mut remaining = a.distance_to(b).get();
        if remaining <= 1e-9 {
            return;
        }
        let total = remaining;

        while remaining > 1e-9 {
            let left_in_dash = widths[idx] - pos;
            let step = left_in_dash.min(remaining);
            let t_end = 1.0 - (remaining - step) / total;
            let seg_end = a.lerp(b, t_end);

            if drawing {
                if !pen_down {
                    out.move_to(seg_start);
                    pen_down = true;
                }
                out.line_to(seg_end);
            } else {
                pen_down = false;
            }

            pos += step;
            remaining -= step;
            seg_start = seg_end;

            if pos >= widths[idx] - 1e-9 {
                pos = 0.0;
                idx = (idx + 1) % widths.len();
                drawing = !drawing;
                pen_down = false;
            }
        }
    });

    out.build()
}

fn convert_path(path: &Path) -> LyonPath {
    let mut b = LyonPath::builder();
    let mut open = false;
    for ev in path.iter() {
        match ev {
            PathEvent::MoveTo(p) => {
                if open {
                    b.end(false);
                }
                b.begin(lyon_point(p.x.get(), p.y.get()));
                open = true;
            }
            PathEvent::LineTo(p) => {
                if open {
                    b.line_to(lyon_point(p.x.get(), p.y.get()));
                }
            }
            PathEvent::QuadTo(c, p) => {
                if open {
                    b.quadratic_bezier_to(
                        lyon_point(c.x.get(), c.y.get()),
                        lyon_point(p.x.get(), p.y.get()),
                    );
                }
            }
            PathEvent::CubicTo(c1, c2, p) => {
                if open {
                    b.cubic_bezier_to(
                        lyon_point(c1.x.get(), c1.y.get()),
                        lyon_point(c2.x.get(), c2.y.get()),
                        lyon_point(p.x.get(), p.y.get()),
                    );
                }
            }
            PathEvent::Close => {
                if open {
                    b.end(true);
                    open = false;
                }
            }
        }
    }
    if open {
        b.end(false);
    }
    b.build()
}

fn convert_cap(c: LineCap) -> tess::LineCap {
    match c {
        LineCap::Butt => tess::LineCap::Butt,
        LineCap::Round => tess::LineCap::Round,
        LineCap::Square => tess::LineCap::Square,
    }
}

fn convert_join(j: LineJoin) -> tess::LineJoin {
    match j {
        LineJoin::Miter => tess::LineJoin::MiterClip,
        LineJoin::Round => tess::LineJoin::Round,
        LineJoin::Bevel => tess::LineJoin::Bevel,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sphere_core::{Color, PathBuilder, Rect, Size, point, px, rect};

    fn square() -> Path {
        let mut b = PathBuilder::new();
        b.rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)));
        b.build()
    }

    #[test]
    fn filling_a_square_produces_two_triangles_worth_of_geometry() {
        let mut t = Tessellator::new();
        let m = t
            .fill_path(&square(), FillRule::NonZero, Color::RED.to_linear(), Default::default())
            .unwrap();
        assert!(!m.is_empty());
        assert_eq!(m.indices.len() % 3, 0, "index count must be a multiple of three");
        assert!(m.vertices.len() >= 4);
        // Every index must be in range, or the GPU reads garbage.
        assert!(m.indices.iter().all(|i| (*i as usize) < m.vertices.len()));
    }

    #[test]
    fn filled_geometry_stays_within_the_path_bounds() {
        let mut t = Tessellator::new();
        let m = t
            .fill_path(&square(), FillRule::NonZero, Color::RED.to_linear(), Default::default())
            .unwrap();
        let b = m.bounds();
        assert!(b.min_x() >= px(-0.01) && b.max_x() <= px(10.01), "{b:?}");
        assert!(b.min_y() >= px(-0.01) && b.max_y() <= px(10.01), "{b:?}");
    }

    #[test]
    fn stroke_geometry_extends_half_the_width_outside() {
        let mut t = Tessellator::new();
        let m = t
            .stroke_path(
                &square(),
                &Stroke::new(px(4.0)),
                Color::RED.to_linear(),
                Default::default(),
            )
            .unwrap();
        let b = m.bounds();
        assert!(b.min_x() <= px(-1.9), "stroke should reach ~-2, got {b:?}");
        assert!(b.max_x() >= px(11.9), "{b:?}");
    }

    #[test]
    fn empty_and_invisible_input_produce_empty_meshes_without_error() {
        let mut t = Tessellator::new();
        let empty = Path::new();
        assert!(
            t.fill_path(&empty, FillRule::NonZero, Color::RED.to_linear(), Default::default())
                .unwrap()
                .is_empty()
        );
        assert!(
            t.stroke_path(
                &square(),
                &Stroke::new(Px::ZERO),
                Color::RED.to_linear(),
                Default::default()
            )
            .unwrap()
            .is_empty()
        );
    }

    #[test]
    fn tolerance_scales_inversely_with_zoom() {
        // Zooming in must tessellate more finely, or curves show facets.
        let coarse = TessellationOptions { tolerance: 0.25, scale: 1.0 };
        let fine = TessellationOptions { tolerance: 0.25, scale: 8.0 };
        assert!(fine.local_tolerance() < coarse.local_tolerance());

        let mut b = PathBuilder::new();
        b.circle(point(px(0.0), px(0.0)), px(100.0));
        let circle = b.build();
        let mut t = Tessellator::new();
        let a = t.fill_path(&circle, FillRule::NonZero, Color::RED.to_linear(), coarse).unwrap();
        let z = t.fill_path(&circle, FillRule::NonZero, Color::RED.to_linear(), fine).unwrap();
        assert!(
            z.vertices.len() > a.vertices.len(),
            "{} vs {}",
            z.vertices.len(),
            a.vertices.len()
        );
    }

    #[test]
    fn degenerate_scale_does_not_produce_a_nan_tolerance() {
        for s in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let tol = TessellationOptions { tolerance: 0.25, scale: s }.local_tolerance();
            assert!(tol.is_finite() && tol > 0.0, "scale {s} gave tolerance {tol}");
        }
    }

    #[test]
    fn even_odd_and_nonzero_tessellate_a_ring_differently() {
        let mut b = PathBuilder::new();
        b.circle(point(px(0.0), px(0.0)), px(50.0));
        b.circle(point(px(0.0), px(0.0)), px(25.0));
        let p = b.build();
        let mut t = Tessellator::new();
        let nz =
            t.fill_path(&p, FillRule::NonZero, Color::RED.to_linear(), Default::default()).unwrap();
        let eo =
            t.fill_path(&p, FillRule::EvenOdd, Color::RED.to_linear(), Default::default()).unwrap();
        // EvenOdd punches the middle out, so it needs different geometry.
        assert_ne!(nz.indices.len(), eo.indices.len());
    }

    #[test]
    fn transform_mesh_moves_every_vertex() {
        let mut m = Mesh {
            vertices: vec![MeshVertex::new(point(px(1.0), px(2.0)), LinearColor::TRANSPARENT)],
            indices: vec![0],
        };
        transform_mesh(&mut m, Affine::translate(Size::new(px(10.0), px(20.0))));
        assert_eq!(m.vertices[0].position, [11.0, 22.0]);
    }

    #[test]
    fn transform_mesh_with_identity_is_a_no_op() {
        let mut m = Mesh {
            vertices: vec![MeshVertex::new(point(px(1.0), px(2.0)), LinearColor::TRANSPARENT)],
            indices: vec![0],
        };
        transform_mesh(&mut m, Affine::IDENTITY);
        assert_eq!(m.vertices[0].position, [1.0, 2.0]);
    }

    #[test]
    fn dashing_a_line_produces_alternating_segments() {
        let mut b = PathBuilder::new();
        b.move_to(point(px(0.0), px(0.0)));
        b.line_to(point(px(100.0), px(0.0)));
        let dashed = apply_dash(&b.build(), &[px(10.0), px(10.0)], Px::ZERO, px(0.1));
        // 100 units of 10-on/10-off gives five drawn dashes.
        let moves = dashed.iter().filter(|e| matches!(e, PathEvent::MoveTo(_))).count();
        assert_eq!(moves, 5, "expected 5 dashes, got {moves}");
        assert!((dashed.length(px(0.1)).get() - 50.0).abs() < 0.5);
    }

    #[test]
    fn dash_offset_shifts_the_pattern() {
        let mut b = PathBuilder::new();
        b.move_to(point(px(0.0), px(0.0)));
        b.line_to(point(px(100.0), px(0.0)));
        let p = b.build();
        let first_dash_start = |path: &Path| match path.iter().next() {
            Some(PathEvent::MoveTo(p)) => p.x.get(),
            other => panic!("expected a leading MoveTo, got {other:?}"),
        };
        // A symmetric pattern shifted by exactly one dash keeps the same dash
        // *count*, so only the positions reveal whether the offset applied.
        let a = apply_dash(&p, &[px(10.0), px(10.0)], Px::ZERO, px(0.1));
        let shifted = apply_dash(&p, &[px(10.0), px(10.0)], px(10.0), px(0.1));
        assert!((first_dash_start(&a) - 0.0).abs() < 0.01, "{}", first_dash_start(&a));
        assert!(
            (first_dash_start(&shifted) - 10.0).abs() < 0.01,
            "offset did not shift the pattern: first dash at {}",
            first_dash_start(&shifted)
        );
    }

    #[test]
    fn an_empty_or_zero_dash_pattern_leaves_the_path_alone() {
        let p = square();
        assert_eq!(apply_dash(&p, &[], Px::ZERO, px(0.1)).verb_count(), p.verb_count());
        assert_eq!(apply_dash(&p, &[Px::ZERO], Px::ZERO, px(0.1)).verb_count(), p.verb_count());
    }

    #[test]
    fn dashing_terminates_on_a_degenerate_path() {
        // A zero-length segment with a tiny pattern is the shape that hangs a
        // naive dash loop.
        let mut b = PathBuilder::new();
        b.move_to(point(px(0.0), px(0.0)));
        b.line_to(point(px(0.0), px(0.0)));
        b.line_to(point(px(1.0), px(0.0)));
        let out = apply_dash(&b.build(), &[px(0.001), px(0.001)], Px::ZERO, px(0.1));
        assert!(out.verb_count() < 100_000);
    }

    #[test]
    fn open_subpaths_are_not_force_closed_by_conversion() {
        let mut b = PathBuilder::new();
        b.move_to(point(px(0.0), px(0.0)));
        b.line_to(point(px(10.0), px(0.0)));
        let open = b.build();
        let mut t = Tessellator::new();
        // A butt-capped open stroke must not wrap back to the start.
        let m = t
            .stroke_path(&open, &Stroke::new(px(2.0)), Color::RED.to_linear(), Default::default())
            .unwrap();
        let bounds = m.bounds();
        assert!(bounds.width() >= px(9.9) && bounds.height() <= px(2.1), "{bounds:?}");
    }

    #[test]
    fn all_generated_indices_are_in_range_for_a_complex_path() {
        let mut b = PathBuilder::new();
        for i in 0..20 {
            let f = i as f32;
            b.circle(point(px(f * 7.0), px(f * 3.0)), px(10.0 + f));
        }
        let mut t = Tessellator::new();
        let m = t
            .fill_path(&b.build(), FillRule::NonZero, Color::RED.to_linear(), Default::default())
            .unwrap();
        assert!(m.indices.iter().all(|i| (*i as usize) < m.vertices.len()));
        assert_eq!(m.indices.len() % 3, 0);
    }

    #[test]
    fn tessellator_reuse_does_not_leak_geometry_between_calls() {
        let mut t = Tessellator::new();
        let a = t
            .fill_path(&square(), FillRule::NonZero, Color::RED.to_linear(), Default::default())
            .unwrap();
        let b = t
            .fill_path(&square(), FillRule::NonZero, Color::RED.to_linear(), Default::default())
            .unwrap();
        assert_eq!(a.vertices.len(), b.vertices.len(), "second call inherited the first's buffers");
        assert_eq!(a.indices.len(), b.indices.len());
    }

    #[test]
    fn unused_rect_import_is_exercised() {
        // Keeps the Rect import honest and documents the expected empty bounds.
        let m = Mesh::default();
        assert_eq!(m.bounds(), Rect::<Px>::ZERO);
    }
}
