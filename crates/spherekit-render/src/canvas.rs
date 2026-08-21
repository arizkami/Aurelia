//! The canvas: the recording API.
//!
//! A [`Canvas`] borrows a [`Scene`] and appends resolved commands to it. It
//! owns the transform and clip stacks, and resolves both at record time so that
//! nothing downstream has to replay them.
//!
//! The canvas is usable entirely without the UI layer, which is the point: the
//! graphics engine must stand on its own.
//!
//! ```
//! use spherekit_render::{Canvas, Scene};
//! use spherekit_core::{Color, ScaleFactor, px, rect, size};
//!
//! let mut scene = Scene::new(size(px(400.0), px(300.0)), ScaleFactor::IDENTITY);
//! {
//!     let mut canvas = Canvas::new(&mut scene);
//!     canvas.fill_rect(rect(px(10.0), px(10.0), px(100.0), px(40.0)), Color::hex(0x2E7D32));
//! }
//! assert_eq!(scene.len(), 1);
//! ```

use crate::scene::{
    Clip, ClipKind, DrawCommand, Filter, GlyphRun, Layer, Mesh, NO_INDEX, QuadCommand, Scene,
    SceneIndex,
};
use smallvec::SmallVec;
use spherekit_core::{
    Affine, BlendMode, Brush, Color, Corners, FillRule, ImageFit, ImageId, Paint, Path, Point, Px,
    Rect, RoundedRect, Shadow, Size, Stroke, TextureId,
};

/// One entry on the canvas state stack.
#[derive(Copy, Clone, Debug)]
struct SaveState {
    transform: Affine,
    transform_index: SceneIndex,
    clip_index: SceneIndex,
    opacity: f32,
    layer_depth: u32,
}

/// Records drawing operations into a [`Scene`].
pub struct Canvas<'a> {
    scene: &'a mut Scene,
    transform: Affine,
    transform_index: SceneIndex,
    clip_index: SceneIndex,
    opacity: f32,
    stack: SmallVec<[SaveState; 16]>,
    open_layers: SmallVec<[SceneIndex; 4]>,
    layer_depth: u32,
}

impl<'a> Canvas<'a> {
    /// Begins recording into `scene`.
    pub fn new(scene: &'a mut Scene) -> Self {
        Self {
            scene,
            transform: Affine::IDENTITY,
            transform_index: 0,
            clip_index: 0,
            opacity: 1.0,
            stack: SmallVec::new(),
            open_layers: SmallVec::new(),
            layer_depth: 0,
        }
    }

    /// The scene being recorded into.
    #[inline]
    pub fn scene(&self) -> &Scene {
        self.scene
    }

    /// The current accumulated transform.
    #[inline]
    pub fn transform(&self) -> Affine {
        self.transform
    }

    /// The current clip's scissor rectangle, in scene-absolute logical pixels.
    #[inline]
    pub fn clip_bounds(&self) -> Rect<Px> {
        self.scene.scissor_for(self.clip_index)
    }

    /// The current inherited opacity.
    #[inline]
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    // ---------------------------------------------------------------- state

    /// Pushes the current transform, clip and opacity.
    pub fn save(&mut self) {
        self.stack.push(SaveState {
            transform: self.transform,
            transform_index: self.transform_index,
            clip_index: self.clip_index,
            opacity: self.opacity,
            layer_depth: self.layer_depth,
        });
    }

    /// Pops the most recent [`Canvas::save`].
    ///
    /// Any layer opened since that save is closed first. Leaving a layer open
    /// across a restore would corrupt the command stream in a way that only
    /// shows up as a missing composite several frames later.
    pub fn restore(&mut self) {
        let Some(s) = self.stack.pop() else { return };
        while self.layer_depth > s.layer_depth {
            self.end_layer();
        }
        self.transform = s.transform;
        self.transform_index = s.transform_index;
        self.clip_index = s.clip_index;
        self.opacity = s.opacity;
    }

    /// Runs `f` between a [`Canvas::save`] and [`Canvas::restore`].
    pub fn with_save(&mut self, f: impl FnOnce(&mut Self)) {
        self.save();
        f(self);
        self.restore();
    }

    /// Applies `t` on top of the current transform.
    pub fn transform_by(&mut self, t: Affine) {
        self.transform = t.then(self.transform);
        self.transform_index = self.scene.intern_transform(self.transform);
    }

    /// Translates the current transform.
    #[inline]
    pub fn translate(&mut self, offset: Size<Px>) {
        self.transform_by(Affine::translate(offset));
    }

    /// Scales the current transform about the local origin.
    #[inline]
    pub fn scale(&mut self, sx: f32, sy: f32) {
        self.transform_by(Affine::scale(sx, sy));
    }

    /// Rotates the current transform about the local origin.
    #[inline]
    pub fn rotate(&mut self, angle: spherekit_core::Rad) {
        self.transform_by(Affine::rotate(angle));
    }

    /// Multiplies the inherited opacity.
    ///
    /// This does **not** open a layer. Opacity applied to individual primitives
    /// is exact as long as they do not overlap; only overlapping content needs
    /// the group semantics of [`Canvas::push_opacity_layer`].
    #[inline]
    pub fn set_opacity(&mut self, opacity: f32) {
        self.opacity = (self.opacity * opacity).clamp(0.0, 1.0);
    }

    // ---------------------------------------------------------------- clips

    /// Intersects the clip with a rectangle.
    pub fn clip_rect(&mut self, r: Rect<Px>) {
        let bounds = self.transform.transform_rect_bounds(r);
        self.push_clip(Clip { bounds, kind: ClipKind::Rect, parent: self.clip_index });
    }

    /// Intersects the clip with a rounded rectangle.
    ///
    /// When the radii are all zero this degrades to a plain rectangle clip so
    /// the backend keeps the cheap scissor-only path.
    pub fn clip_rounded_rect(&mut self, rr: RoundedRect) {
        let radii = rr.clamped_radii();
        if radii.is_zero() {
            return self.clip_rect(rr.rect);
        }
        let bounds = self.transform.transform_rect_bounds(rr.rect);
        self.push_clip(Clip { bounds, kind: ClipKind::Rounded(radii), parent: self.clip_index });
    }

    /// Intersects the clip with an arbitrary path.
    pub fn clip_path(&mut self, path: Path, fill_rule: FillRule) {
        let bounds = self.transform.transform_rect_bounds(path.control_bounds());
        let idx = self.push_path(path);
        self.push_clip(Clip {
            bounds,
            kind: ClipKind::Path { path: idx, fill_rule },
            parent: self.clip_index,
        });
    }

    fn push_clip(&mut self, clip: Clip) {
        self.scene.clips.push(clip);
        self.clip_index = (self.scene.clips.len() - 1) as SceneIndex;
    }

    // --------------------------------------------------------------- layers

    /// Opens an offscreen layer with the given opacity and blend mode.
    ///
    /// Returns `true` when a layer was actually opened. A fully opaque,
    /// normally-blended, unfiltered group needs no offscreen target, and
    /// allocating one anyway is a render-target allocation and an extra pass for
    /// nothing.
    pub fn push_layer(
        &mut self,
        bounds: Rect<Px>,
        opacity: f32,
        blend: BlendMode,
        filter: Option<Filter>,
    ) -> bool {
        let needs_layer = opacity < 1.0
            || !blend.is_fixed_function()
            || filter.is_some()
            || blend != BlendMode::Normal;
        if !needs_layer {
            return false;
        }
        let bounds = self.transform.transform_rect_bounds(bounds);
        self.scene.layers.push(Layer {
            bounds,
            opacity: opacity.clamp(0.0, 1.0),
            blend,
            filter,
            end_command: NO_INDEX,
        });
        let layer = (self.scene.layers.len() - 1) as SceneIndex;
        self.scene.commands.push(DrawCommand::BeginLayer { layer });
        self.open_layers.push(layer);
        self.layer_depth += 1;
        true
    }

    /// Opens a layer purely to make a group opacity correct across overlapping
    /// children.
    #[inline]
    pub fn push_opacity_layer(&mut self, bounds: Rect<Px>, opacity: f32) -> bool {
        self.push_layer(bounds, opacity, BlendMode::Normal, None)
    }

    /// Closes the most recently opened layer.
    pub fn end_layer(&mut self) {
        let Some(layer) = self.open_layers.pop() else { return };
        self.scene.commands.push(DrawCommand::EndLayer);
        let end = (self.scene.commands.len() - 1) as SceneIndex;
        if let Some(l) = self.scene.layers.get_mut(layer as usize) {
            l.end_command = end;
        }
        self.layer_depth = self.layer_depth.saturating_sub(1);
    }

    // ------------------------------------------------------------ primitives

    /// Fills a rectangle with a flat color.
    pub fn fill_rect(&mut self, r: Rect<Px>, color: impl Into<Color>) {
        self.fill_rect_with(r, Brush::Solid(color.into()));
    }

    /// Fills a rectangle with any brush.
    pub fn fill_rect_with(&mut self, r: Rect<Px>, brush: Brush) {
        self.quad(r, Corners::ZERO, Some(brush), Color::TRANSPARENT, Px::ZERO);
    }

    /// Fills a rounded rectangle with a flat color.
    pub fn fill_rounded_rect(&mut self, rr: RoundedRect, color: impl Into<Color>) {
        self.quad(
            rr.rect,
            rr.radii,
            Some(Brush::Solid(color.into())),
            Color::TRANSPARENT,
            Px::ZERO,
        );
    }

    /// Fills a rounded rectangle with any brush.
    pub fn fill_rounded_rect_with(&mut self, rr: RoundedRect, brush: Brush) {
        self.quad(rr.rect, rr.radii, Some(brush), Color::TRANSPARENT, Px::ZERO);
    }

    /// Strokes a rectangle's outline, inset from `r`.
    pub fn stroke_rect(&mut self, r: Rect<Px>, color: impl Into<Color>, width: Px) {
        self.quad(r, Corners::ZERO, None, color.into(), width);
    }

    /// Strokes a rounded rectangle's outline, inset from `rr`.
    pub fn stroke_rounded_rect(&mut self, rr: RoundedRect, color: impl Into<Color>, width: Px) {
        self.quad(rr.rect, rr.radii, None, color.into(), width);
    }

    /// Fills and strokes a rounded rectangle in one primitive.
    ///
    /// A filled-and-bordered box is one of the most common UI shapes, and
    /// emitting it as a single instance rather than two halves the instance
    /// count for a typical panel-heavy frame.
    pub fn quad(
        &mut self,
        bounds: Rect<Px>,
        radii: Corners<Px>,
        fill: Option<Brush>,
        border_color: Color,
        border_width: Px,
    ) {
        let has_border = border_width > Px::ZERO && !border_color.is_transparent();
        let fill = fill.filter(|b| !b.is_transparent());
        if fill.is_none() && !has_border {
            return;
        }
        if bounds.is_empty() {
            return;
        }
        let fill_index = match fill {
            Some(b) => self.push_paint(b),
            None => NO_INDEX,
        };
        self.scene.commands.push(DrawCommand::Quad(QuadCommand {
            bounds,
            radii,
            fill: fill_index,
            border_color: border_color.scale_alpha(self.opacity),
            border_width: if has_border { border_width } else { Px::ZERO },
            transform: self.transform_index,
            clip: self.clip_index,
        }));
    }

    /// Draws a circle.
    ///
    /// Emitted as a rounded rectangle whose radius is half its extent, which
    /// keeps circles on the analytic quad pipeline instead of tessellating them.
    pub fn fill_circle(&mut self, center: Point<Px>, radius: Px, color: impl Into<Color>) {
        let r = radius.get();
        let bounds = Rect::new(
            Point::new(Px(center.x.get() - r), Px(center.y.get() - r)),
            Size::new(Px(r * 2.0), Px(r * 2.0)),
        );
        self.quad(
            bounds,
            Corners::all(radius),
            Some(Brush::Solid(color.into())),
            Color::TRANSPARENT,
            Px::ZERO,
        );
    }

    /// Strokes a circle's outline.
    pub fn stroke_circle(
        &mut self,
        center: Point<Px>,
        radius: Px,
        color: impl Into<Color>,
        width: Px,
    ) {
        let r = radius.get();
        let bounds = Rect::new(
            Point::new(Px(center.x.get() - r), Px(center.y.get() - r)),
            Size::new(Px(r * 2.0), Px(r * 2.0)),
        );
        self.quad(bounds, Corners::all(radius), None, color.into(), width);
    }

    /// Draws a straight line.
    pub fn draw_line(&mut self, a: Point<Px>, b: Point<Px>, color: impl Into<Color>, width: Px) {
        let mut p = Path::builder();
        p.move_to(a);
        p.line_to(b);
        self.stroke_path(p.build(), color.into(), Stroke::new(width));
    }

    /// Fills a path.
    pub fn fill_path(&mut self, path: Path, brush: impl Into<Brush>) {
        let brush = brush.into();
        if brush.is_transparent() || path.is_empty() {
            return;
        }
        let fill_rule = path.fill_rule();
        let path_index = self.push_path(path);
        let paint = self.push_paint(brush);
        self.scene.commands.push(DrawCommand::FillPath {
            path: path_index,
            paint,
            fill_rule,
            transform: self.transform_index,
            clip: self.clip_index,
        });
    }

    /// Strokes a path.
    pub fn stroke_path(&mut self, path: Path, brush: impl Into<Brush>, stroke: Stroke) {
        let brush = brush.into();
        if brush.is_transparent() || stroke.is_invisible() || path.is_empty() {
            return;
        }
        let path_index = self.push_path(path);
        let paint = self.push_paint(brush);
        self.scene.strokes.push(stroke);
        let stroke_index = (self.scene.strokes.len() - 1) as SceneIndex;
        self.scene.commands.push(DrawCommand::StrokePath {
            path: path_index,
            paint,
            stroke: stroke_index,
            transform: self.transform_index,
            clip: self.clip_index,
        });
    }

    /// Draws a run of positioned glyphs.
    pub fn draw_glyph_run(&mut self, run: GlyphRun, brush: impl Into<Brush>) {
        let brush = brush.into();
        if brush.is_transparent() || run.glyphs.is_empty() {
            return;
        }
        self.scene.runs.push(run);
        let run_index = (self.scene.runs.len() - 1) as SceneIndex;
        let paint = self.push_paint(brush);
        self.scene.commands.push(DrawCommand::Text {
            run: run_index,
            paint,
            transform: self.transform_index,
            clip: self.clip_index,
        });
    }

    /// Draws an image into `dest`.
    pub fn draw_image(&mut self, image: ImageId, dest: Rect<Px>, fit: ImageFit) {
        self.draw_image_tinted(image, dest, fit, Color::WHITE, Corners::ZERO);
    }

    /// Draws an image with a tint and corner radii.
    pub fn draw_image_tinted(
        &mut self,
        image: ImageId,
        dest: Rect<Px>,
        _fit: ImageFit,
        tint: Color,
        radii: Corners<Px>,
    ) {
        if dest.is_empty() || tint.is_transparent() {
            return;
        }
        self.scene.commands.push(DrawCommand::Image {
            dest,
            // The full texture. Fit-aware source rectangles are resolved by the
            // image crate, which is the only place that knows the natural size.
            source: Rect::new(Point::ZERO, Size::new(Px::ONE, Px::ONE)),
            image,
            tint: tint.scale_alpha(self.opacity),
            radii,
            transform: self.transform_index,
            clip: self.clip_index,
        });
    }

    /// Draws pre-tessellated geometry.
    ///
    /// This is the path realtime audio primitives take: a spectrum or waveform
    /// generates its own triangles once per frame and hands them over directly,
    /// with no tessellator and no path allocation in between.
    pub fn draw_mesh(&mut self, mesh: Mesh, texture: Option<TextureId>) {
        if mesh.is_empty() {
            return;
        }
        self.scene.meshes.push(mesh);
        let mesh_index = (self.scene.meshes.len() - 1) as SceneIndex;
        self.scene.commands.push(DrawCommand::Mesh {
            mesh: mesh_index,
            texture,
            transform: self.transform_index,
            clip: self.clip_index,
        });
    }

    /// Draws a box shadow for a rounded rectangle.
    pub fn draw_shadow(&mut self, shape: RoundedRect, shadow: &Shadow) {
        if shadow.color.is_transparent() {
            return;
        }
        let spread = shadow.spread.get();
        let offset_rect = Rect::new(
            Point::new(
                shape.rect.min_x() + shadow.offset.width - Px(spread),
                shape.rect.min_y() + shadow.offset.height - Px(spread),
            ),
            Size::new(
                shape.rect.width() + Px(spread * 2.0),
                shape.rect.height() + Px(spread * 2.0),
            ),
        );
        self.scene.commands.push(DrawCommand::Shadow {
            shape: RoundedRect::new(
                offset_rect,
                shape.radii.map(|r| Px(r.get() + spread).max(Px::ZERO)),
            ),
            blur_radius: shadow.blur_radius,
            color: shadow.color.scale_alpha(self.opacity),
            inset: shadow.inset,
            transform: self.transform_index,
            clip: self.clip_index,
        });
    }

    // ------------------------------------------------------------- internals

    fn push_path(&mut self, path: Path) -> SceneIndex {
        self.scene.paths.push(path);
        (self.scene.paths.len() - 1) as SceneIndex
    }

    fn push_paint(&mut self, brush: Brush) -> SceneIndex {
        let brush = if self.opacity < 1.0 { brush.scale_alpha(self.opacity) } else { brush };
        self.scene.paints.push(Paint { brush, blend: BlendMode::Normal, opacity: 1.0 });
        (self.scene.paints.len() - 1) as SceneIndex
    }
}

impl Drop for Canvas<'_> {
    /// Closes any layer left open, so a scene is always well-formed even if a
    /// caller panics or forgets an [`Canvas::end_layer`].
    fn drop(&mut self) {
        while !self.open_layers.is_empty() {
            self.end_layer();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::Scene;
    use spherekit_core::{Gradient, ScaleFactor, px, rect, size};

    fn new_scene() -> Scene {
        Scene::new(size(px(800.0), px(600.0)), ScaleFactor::IDENTITY)
    }

    #[test]
    fn fill_rect_records_one_quad() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
        }
        assert_eq!(s.len(), 1);
        assert!(matches!(s.commands[0], DrawCommand::Quad(_)));
    }

    #[test]
    fn fully_transparent_and_empty_primitives_are_dropped_at_record_time() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::TRANSPARENT);
            c.fill_rect(rect(px(0.0), px(0.0), px(0.0), px(10.0)), Color::RED);
            c.stroke_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED, Px::ZERO);
        }
        assert_eq!(s.len(), 0, "invisible work must never reach a vertex buffer");
    }

    #[test]
    fn save_restore_returns_the_transform() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.save();
            c.translate(size(px(50.0), px(50.0)));
            assert!(
                !c.transform().is_translation_only()
                    || c.transform().translation().width == px(50.0)
            );
            c.restore();
            assert_eq!(c.transform(), Affine::IDENTITY);
        }
    }

    #[test]
    fn nested_transforms_compose_outermost_last() {
        let mut s = new_scene();
        let mut c = Canvas::new(&mut s);
        c.translate(size(px(10.0), px(0.0)));
        c.scale(2.0, 2.0);
        // A local point at x=1 scales to 2 then translates to 12.
        let p = c.transform().apply(Point::new(px(1.0), px(0.0)));
        assert!((p.x.get() - 12.0).abs() < 1e-4, "{p:?}");
    }

    #[test]
    fn clip_stack_narrows_and_restores() {
        let mut s = new_scene();
        let mut c = Canvas::new(&mut s);
        c.save();
        c.clip_rect(rect(px(0.0), px(0.0), px(100.0), px(100.0)));
        c.clip_rect(rect(px(50.0), px(50.0), px(200.0), px(200.0)));
        assert_eq!(c.clip_bounds(), rect(px(50.0), px(50.0), px(50.0), px(50.0)));
        c.restore();
        assert_eq!(c.clip_bounds(), Rect::INFINITE);
    }

    #[test]
    fn clip_is_recorded_in_scene_absolute_space() {
        let mut s = new_scene();
        let mut c = Canvas::new(&mut s);
        c.translate(size(px(100.0), px(0.0)));
        c.clip_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)));
        // The clip must already account for the transform; the backend gets a
        // scissor rect and has no transform to apply.
        assert_eq!(c.clip_bounds().min_x(), px(100.0));
    }

    #[test]
    fn zero_radius_rounded_clip_degrades_to_a_scissor() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.clip_rounded_rect(RoundedRect::uniform(
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                Px::ZERO,
            ));
        }
        assert!(s.clips.last().unwrap().is_scissor_only(), "must not force a mask pass");
    }

    #[test]
    fn an_opaque_normal_group_does_not_allocate_a_layer() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            let opened = c.push_layer(
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                1.0,
                BlendMode::Normal,
                None,
            );
            assert!(!opened, "a no-op group must not cost a render target");
        }
        assert!(s.layers.is_empty());
        assert!(s.commands.is_empty());
    }

    #[test]
    fn a_translucent_group_does_allocate_a_layer_and_closes_it() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            assert!(c.push_opacity_layer(rect(px(0.0), px(0.0), px(10.0), px(10.0)), 0.5));
            c.fill_rect(rect(px(0.0), px(0.0), px(5.0), px(5.0)), Color::RED);
            c.end_layer();
        }
        assert_eq!(s.layers.len(), 1);
        assert!(matches!(s.commands[0], DrawCommand::BeginLayer { .. }));
        assert!(matches!(s.commands[2], DrawCommand::EndLayer));
        assert_eq!(s.layers[0].end_command, 2, "layer must record where it ends");
    }

    #[test]
    fn dropping_the_canvas_closes_a_forgotten_layer() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.push_opacity_layer(rect(px(0.0), px(0.0), px(10.0), px(10.0)), 0.5);
            c.fill_rect(rect(px(0.0), px(0.0), px(5.0), px(5.0)), Color::RED);
            // deliberately no end_layer
        }
        assert!(
            matches!(s.commands.last(), Some(DrawCommand::EndLayer)),
            "an unbalanced scene would desync the backend's target stack"
        );
    }

    #[test]
    fn restore_closes_layers_opened_since_the_save() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.save();
            c.push_opacity_layer(rect(px(0.0), px(0.0), px(10.0), px(10.0)), 0.5);
            c.restore();
            c.fill_rect(rect(px(0.0), px(0.0), px(5.0), px(5.0)), Color::RED);
        }
        let ends = s.commands.iter().filter(|c| matches!(c, DrawCommand::EndLayer)).count();
        assert_eq!(ends, 1);
        assert!(matches!(s.commands[1], DrawCommand::EndLayer));
    }

    #[test]
    fn inherited_opacity_multiplies_into_recorded_paint() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.set_opacity(0.5);
            c.set_opacity(0.5);
            c.fill_rect(rect(px(0.0), px(0.0), px(10.0), px(10.0)), Color::RED);
        }
        match &s.paints[0].brush {
            Brush::Solid(col) => assert!((col.a - 0.25).abs() < 1e-5, "{col:?}"),
            other => panic!("expected solid, got {other:?}"),
        }
    }

    #[test]
    fn circles_stay_on_the_analytic_quad_path() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.fill_circle(Point::new(px(50.0), px(50.0)), px(20.0), Color::RED);
        }
        // A tessellated circle would show up as FillPath and cost a mesh.
        match &s.commands[0] {
            DrawCommand::Quad(q) => {
                assert_eq!(q.bounds, rect(px(30.0), px(30.0), px(40.0), px(40.0)));
                assert_eq!(q.radii.top_left, px(20.0));
            }
            other => panic!("expected Quad, got {other:?}"),
        }
    }

    #[test]
    fn shadow_offset_and_spread_are_baked_into_the_command() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.draw_shadow(
                RoundedRect::uniform(rect(px(10.0), px(10.0), px(100.0), px(50.0)), px(4.0)),
                &Shadow {
                    offset: size(px(0.0), px(6.0)),
                    blur_radius: px(12.0),
                    spread: px(2.0),
                    color: Color::BLACK.with_alpha(0.4),
                    inset: false,
                },
            );
        }
        match &s.commands[0] {
            DrawCommand::Shadow { shape, .. } => {
                assert_eq!(shape.rect.min_y(), px(10.0 + 6.0 - 2.0));
                assert_eq!(shape.rect.width(), px(104.0));
                assert_eq!(shape.radii.top_left, px(6.0), "spread grows the radius too");
            }
            other => panic!("expected Shadow, got {other:?}"),
        }
    }

    #[test]
    fn gradients_survive_the_opacity_multiplier() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            c.set_opacity(0.5);
            c.fill_rect_with(
                rect(px(0.0), px(0.0), px(10.0), px(10.0)),
                Brush::Gradient(Gradient::vertical(px(10.0), Color::RED, Color::BLUE)),
            );
        }
        match &s.paints[0].brush {
            Brush::Gradient(g) => {
                assert!(g.stops().iter().all(|st| (st.color.a - 0.5).abs() < 1e-5))
            }
            other => panic!("expected gradient, got {other:?}"),
        }
    }

    #[test]
    fn ten_thousand_rects_reuse_one_transform_entry() {
        let mut s = new_scene();
        {
            let mut c = Canvas::new(&mut s);
            for i in 0..10_000 {
                c.fill_rect(rect(px(i as f32), px(0.0), px(1.0), px(1.0)), Color::RED);
            }
        }
        assert_eq!(s.len(), 10_000);
        assert_eq!(s.transforms.len(), 1, "static transform must not be duplicated per primitive");
    }
}
