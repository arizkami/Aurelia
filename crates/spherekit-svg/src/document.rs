//! Converting a parsed SVG into SphereKit geometry.
//!
//! `usvg` does the parsing, the `use`/`defs`/`symbol` resolution and the shape
//! normalisation; everything here turns its output into `spherekit-core` types and
//! then forgets `usvg` ever existed.

use smallvec::SmallVec;
use spherekit_core::{
    Brush, Color, FillRule, Gradient, GradientStop, LineCap, LineJoin, Path, PathBuilder, Point,
    Px, Rect, Size, Stroke, SvgError, px,
};

/// One drawable shape from an SVG document.
///
/// Coordinates are in the document's own user space; [`SvgDocument::view_box`]
/// says how that maps to a destination rectangle.
#[derive(Clone, Debug, PartialEq)]
pub struct SvgShape {
    /// The outline, already transformed by every ancestor's transform.
    pub path: Path,
    /// Fill, or `None` for an unfilled shape.
    pub fill: Option<Brush>,
    /// How the fill's interior is determined.
    pub fill_rule: FillRule,
    /// Stroke paint, or `None` for an unstroked shape.
    pub stroke: Option<Brush>,
    /// Stroke style. Meaningless when `stroke` is `None`.
    pub stroke_style: Stroke,
    /// Accumulated opacity from this shape and every ancestor group.
    ///
    /// Flattened during conversion rather than kept as a tree, because a
    /// correct group opacity would need an offscreen layer per group and icons
    /// essentially never have overlapping translucent children. Documented so
    /// the approximation is visible rather than surprising.
    pub opacity: f32,
}

impl SvgShape {
    /// True when the shape would draw nothing.
    pub fn is_invisible(&self) -> bool {
        self.opacity <= 0.0
            || (self.fill.is_none() && self.stroke.is_none())
            || self.path.is_empty()
    }
}

/// A parsed SVG document, ready to be drawn.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SvgDocument {
    /// The shapes, in paint order.
    pub shapes: Vec<SvgShape>,
    /// The document's coordinate space, from its `viewBox` or its width/height.
    pub view_box: Size<Px>,
}

impl SvgDocument {
    /// Parses an SVG document from bytes.
    pub fn parse(data: &[u8]) -> Result<Self, SvgError> {
        // Checked against the *source*, before parsing. `usvg` removes several
        // unsupported constructs while building its tree — `<text>` disappears
        // entirely when no fonts are loaded — so inspecting the parsed tree
        // would report a clean document that is missing content. Silently
        // dropping content is worse than refusing it: the caller has no way to
        // know the icon came out wrong.
        if let Some(feature) = unsupported_element(data) {
            return Err(SvgError::Unsupported(feature));
        }
        let options = usvg::Options::default();
        let tree =
            usvg::Tree::from_data(data, &options).map_err(|e| SvgError::Parse(e.to_string()))?;
        Self::from_tree(&tree)
    }

    /// Parses an SVG document from a string.
    pub fn parse_str(text: &str) -> Result<Self, SvgError> {
        Self::parse(text.as_bytes())
    }

    /// Loads and parses an SVG file.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, SvgError> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|source| SvgError::Io { path: path.display().to_string(), source })?;
        Self::parse(&data)
    }

    fn from_tree(tree: &usvg::Tree) -> Result<Self, SvgError> {
        let size = tree.size();
        let mut document =
            Self { shapes: Vec::new(), view_box: Size::new(px(size.width()), px(size.height())) };
        document.walk(tree.root(), 1.0)?;
        Ok(document)
    }

    /// Walks a group, flattening its opacity into each shape.
    fn walk(&mut self, group: &usvg::Group, inherited_opacity: f32) -> Result<(), SvgError> {
        if !group.filters().is_empty() {
            return Err(SvgError::Unsupported("filters".to_string()));
        }
        if group.mask().is_some() {
            return Err(SvgError::Unsupported("masks".to_string()));
        }

        let opacity = inherited_opacity * group.opacity().get();
        // Guard against a pathological document nesting groups thousands deep;
        // the recursion below is bounded by the tree usvg built, but a bound
        // here makes the failure a clean error rather than a stack overflow.
        if self.shapes.len() > 100_000 {
            return Err(SvgError::Unsupported("more than 100000 shapes".to_string()));
        }

        for node in group.children() {
            match node {
                usvg::Node::Group(child) => self.walk(child, opacity)?,
                usvg::Node::Path(path) => {
                    if let Some(shape) = convert_path(path, opacity)? {
                        self.shapes.push(shape);
                    }
                }
                usvg::Node::Image(_) => {
                    return Err(SvgError::Unsupported("embedded raster images".to_string()));
                }
                usvg::Node::Text(_) => {
                    return Err(SvgError::Unsupported("<text>".to_string()));
                }
            }
        }
        Ok(())
    }

    /// True when the document has nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.shapes.iter().all(SvgShape::is_invisible)
    }

    /// The tight bounds of every shape, in user space.
    pub fn content_bounds(&self) -> Rect<Px> {
        self.shapes
            .iter()
            .filter(|s| !s.is_invisible())
            .map(|s| s.path.control_bounds())
            .reduce(|a, b| a.union(b))
            .unwrap_or(Rect::ZERO)
    }

    /// The transform mapping this document's view box into `dest`.
    ///
    /// Preserves aspect ratio and centres, which is what `preserveAspectRatio`
    /// defaults to and what an icon in a fixed-size slot always wants.
    pub fn fit_transform(&self, dest: Rect<Px>) -> spherekit_core::Affine {
        let (vw, vh) = (self.view_box.width.get(), self.view_box.height.get());
        if vw <= 0.0 || vh <= 0.0 || dest.is_empty() {
            return spherekit_core::Affine::IDENTITY;
        }
        let scale = (dest.width().get() / vw).min(dest.height().get() / vh);
        let offset = Size::new(
            Px(dest.min_x().get() + (dest.width().get() - vw * scale) * 0.5),
            Px(dest.min_y().get() + (dest.height().get() - vh * scale) * 0.5),
        );
        spherekit_core::Affine::uniform_scale(scale).then(spherekit_core::Affine::translate(offset))
    }
}

/// Names the first unsupported element in an SVG source, if any.
///
/// A deliberately shallow scan: it looks for element open tags, not for a full
/// parse. That is enough to refuse a document rather than render it wrong, and
/// it runs before `usvg` gets a chance to quietly remove anything.
fn unsupported_element(data: &[u8]) -> Option<String> {
    const UNSUPPORTED: &[(&str, &str)] = &[
        ("<text", "<text>; convert text to paths when exporting"),
        ("<tspan", "<tspan>; convert text to paths when exporting"),
        ("<filter", "filters"),
        ("<mask", "masks"),
        ("<pattern", "pattern fills"),
        ("<foreignObject", "<foreignObject>"),
        ("<animate", "animation"),
        ("<image", "embedded raster images"),
    ];
    // Only valid UTF-8 is worth scanning; anything else fails in the parser
    // moments later with a better message than this could produce.
    let text = core::str::from_utf8(data).ok()?;
    UNSUPPORTED.iter().find(|(tag, _)| text.contains(tag)).map(|(_, name)| (*name).to_string())
}

fn convert_path(path: &usvg::Path, opacity: f32) -> Result<Option<SvgShape>, SvgError> {
    if !path.is_visible() {
        return Ok(None);
    }
    // `abs_transform` already folds in every ancestor's transform, so the
    // resulting path is in the document's user space and nothing downstream
    // needs a transform stack.
    let outline = convert_geometry(path.data(), path.abs_transform());
    if outline.is_empty() {
        return Ok(None);
    }

    let fill = path.fill();
    let stroke = path.stroke();
    let fill_rule = match fill.map(usvg::Fill::rule) {
        Some(usvg::FillRule::EvenOdd) => FillRule::EvenOdd,
        _ => FillRule::NonZero,
    };

    let mut outline = outline;
    outline.set_fill_rule(fill_rule);

    Ok(Some(SvgShape {
        path: outline,
        fill: fill.map(|f| convert_paint(f.paint(), f.opacity().get())).transpose()?,
        fill_rule,
        stroke: stroke.map(|s| convert_paint(s.paint(), s.opacity().get())).transpose()?,
        stroke_style: stroke.map(convert_stroke).unwrap_or_default(),
        opacity,
    }))
}

fn convert_geometry(data: &usvg::tiny_skia_path::Path, transform: usvg::Transform) -> Path {
    use usvg::tiny_skia_path::PathSegment;

    let mut builder = PathBuilder::new();
    let map = |x: f32, y: f32| {
        let mut p = usvg::tiny_skia_path::Point::from_xy(x, y);
        transform.map_point(&mut p);
        Point::new(px(p.x), px(p.y))
    };

    for segment in data.segments() {
        match segment {
            PathSegment::MoveTo(p) => {
                builder.move_to(map(p.x, p.y));
            }
            PathSegment::LineTo(p) => {
                builder.line_to(map(p.x, p.y));
            }
            PathSegment::QuadTo(c, p) => {
                builder.quad_to(map(c.x, c.y), map(p.x, p.y));
            }
            PathSegment::CubicTo(c1, c2, p) => {
                builder.cubic_to(map(c1.x, c1.y), map(c2.x, c2.y), map(p.x, p.y));
            }
            PathSegment::Close => {
                builder.close();
            }
        }
    }
    builder.build()
}

fn convert_color(color: usvg::Color, opacity: f32) -> Color {
    Color::rgba8(color.red, color.green, color.blue, 255).with_alpha(opacity.clamp(0.0, 1.0))
}

fn convert_stops(base: &[usvg::Stop], opacity: f32) -> SmallVec<[GradientStop; 8]> {
    base.iter()
        .map(|s| {
            GradientStop::new(
                s.offset().get(),
                convert_color(s.color(), s.opacity().get() * opacity),
            )
        })
        .collect()
}

fn convert_paint(paint: &usvg::Paint, opacity: f32) -> Result<Brush, SvgError> {
    Ok(match paint {
        usvg::Paint::Color(c) => Brush::Solid(convert_color(*c, opacity)),
        usvg::Paint::LinearGradient(g) => {
            let mut gradient = Gradient::Linear {
                start: Point::new(px(g.x1()), px(g.y1())),
                end: Point::new(px(g.x2()), px(g.y2())),
                stops: convert_stops(g.stops(), opacity),
            };
            gradient.normalize();
            Brush::Gradient(gradient)
        }
        usvg::Paint::RadialGradient(g) => {
            let mut gradient = Gradient::Radial {
                center: Point::new(px(g.cx()), px(g.cy())),
                // SphereKit's radial gradient is elliptical; SVG's is circular
                // with an optional transform, so the radius goes on both axes.
                radius: Size::new(px(g.r().get()), px(g.r().get())),
                stops: convert_stops(g.stops(), opacity),
            };
            gradient.normalize();
            Brush::Gradient(gradient)
        }
        usvg::Paint::Pattern(_) => {
            return Err(SvgError::Unsupported("pattern fills".to_string()));
        }
    })
}

fn convert_stroke(stroke: &usvg::Stroke) -> Stroke {
    Stroke {
        width: px(stroke.width().get()),
        cap: match stroke.linecap() {
            usvg::LineCap::Butt => LineCap::Butt,
            usvg::LineCap::Round => LineCap::Round,
            usvg::LineCap::Square => LineCap::Square,
        },
        join: match stroke.linejoin() {
            usvg::LineJoin::Miter | usvg::LineJoin::MiterClip => LineJoin::Miter,
            usvg::LineJoin::Round => LineJoin::Round,
            usvg::LineJoin::Bevel => LineJoin::Bevel,
        },
        miter_limit: stroke.miterlimit().get(),
        dash: stroke.dasharray().map(|d| d.iter().map(|v| px(*v)).collect()).unwrap_or_default(),
        dash_offset: px(stroke.dashoffset()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CIRCLE: &str = r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
        <circle cx="50" cy="50" r="40" fill="red"/></svg>"#;

    #[test]
    fn a_simple_path_parses() {
        let doc = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16">
               <path d="M2 8 L14 8" stroke="black" stroke-width="2"/></svg>"#,
        )
        .unwrap();
        assert_eq!(doc.shapes.len(), 1);
        assert_eq!(doc.view_box, Size::new(px(16.0), px(16.0)));
        assert!(doc.shapes[0].stroke.is_some());
        assert_eq!(doc.shapes[0].stroke_style.width, px(2.0));
    }

    #[test]
    fn basic_shapes_become_paths() {
        // usvg normalises rect/circle/ellipse/line/polygon into paths, which is
        // exactly the work this crate does not want to duplicate.
        for markup in [
            r#"<rect x="1" y="1" width="8" height="8" fill="red"/>"#,
            r#"<circle cx="5" cy="5" r="4" fill="red"/>"#,
            r#"<ellipse cx="5" cy="5" rx="4" ry="2" fill="red"/>"#,
            r#"<polygon points="0,0 10,0 5,10" fill="red"/>"#,
            r#"<polyline points="0,0 10,0 5,10" fill="none" stroke="red"/>"#,
        ] {
            let svg = format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">{markup}</svg>"#
            );
            let doc = SvgDocument::parse_str(&svg).unwrap();
            assert_eq!(doc.shapes.len(), 1, "{markup}");
            assert!(!doc.shapes[0].path.is_empty(), "{markup}");
        }
    }

    #[test]
    fn a_fill_colour_survives_conversion() {
        let doc = SvgDocument::parse_str(CIRCLE).unwrap();
        match &doc.shapes[0].fill {
            Some(Brush::Solid(c)) => assert_eq!(c.to_rgba8(), [255, 0, 0, 255]),
            other => panic!("expected a solid red fill, got {other:?}"),
        }
    }

    #[test]
    fn nested_transforms_compose() {
        // The inner rect is translated twice and scaled once, so its geometry
        // must land at (10 + 5) * 2 = 30.
        let doc = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
               <g transform="scale(2)"><g transform="translate(10,0)">
               <rect x="5" y="0" width="1" height="1" fill="black"/>
               </g></g></svg>"#,
        )
        .unwrap();
        let bounds = doc.shapes[0].path.control_bounds();
        assert!((bounds.min_x().get() - 30.0).abs() < 0.01, "{bounds:?}");
    }

    #[test]
    fn group_opacity_is_flattened_into_its_shapes() {
        let doc = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
               <g opacity="0.5"><g opacity="0.5">
               <rect width="10" height="10" fill="black"/></g></g></svg>"#,
        )
        .unwrap();
        assert!((doc.shapes[0].opacity - 0.25).abs() < 1e-4, "{}", doc.shapes[0].opacity);
    }

    #[test]
    fn fill_rule_is_carried_through() {
        let doc = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
               <path d="M0 0h10v10h-10z M2 2h6v6h-6z" fill="black" fill-rule="evenodd"/></svg>"#,
        )
        .unwrap();
        assert_eq!(doc.shapes[0].fill_rule, FillRule::EvenOdd);
        assert_eq!(doc.shapes[0].path.fill_rule(), FillRule::EvenOdd);
    }

    #[test]
    fn stroke_caps_joins_and_dashes_convert() {
        let doc = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
               <path d="M0 5h10" stroke="black" stroke-width="3" stroke-linecap="round"
                     stroke-linejoin="bevel" stroke-dasharray="2 1" stroke-dashoffset="1"/></svg>"#,
        )
        .unwrap();
        let s = &doc.shapes[0].stroke_style;
        assert_eq!(s.width, px(3.0));
        assert_eq!(s.cap, LineCap::Round);
        assert_eq!(s.join, LineJoin::Bevel);
        assert_eq!(s.dash.as_slice(), &[px(2.0), px(1.0)]);
        assert_eq!(s.dash_offset, px(1.0));
    }

    #[test]
    fn a_linear_gradient_converts_with_its_stops_in_order() {
        let doc = SvgDocument::parse_str(
            r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
               <defs><linearGradient id="g" x1="0" y1="0" x2="10" y2="0"
                     gradientUnits="userSpaceOnUse">
                 <stop offset="0" stop-color="red"/>
                 <stop offset="1" stop-color="blue"/>
               </linearGradient></defs>
               <rect width="10" height="10" fill="url(#g)"/></svg>"##,
        )
        .unwrap();
        match &doc.shapes[0].fill {
            Some(Brush::Gradient(g)) => {
                let offsets: Vec<f32> = g.stops().iter().map(|s| s.offset).collect();
                assert_eq!(offsets, vec![0.0, 1.0]);
                assert_eq!(g.sample(0.0).to_rgba8(), [255, 0, 0, 255]);
                assert_eq!(g.sample(1.0).to_rgba8(), [0, 0, 255, 255]);
                // Offsets must be non-decreasing or the shader reads garbage.
                assert!(offsets.windows(2).all(|w| w[0] <= w[1]));
            }
            other => panic!("expected a gradient, got {other:?}"),
        }
    }

    #[test]
    fn a_use_element_is_resolved() {
        let doc = SvgDocument::parse_str(
            // A wider raw-string delimiter: the fragment identifiers contain
            // `"#`, which would close an `r#"..."#` early.
            r##"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"
                    viewBox="0 0 20 10">
               <defs><rect id="r" width="5" height="5" fill="black"/></defs>
               <use xlink:href="#r"/><use xlink:href="#r" x="10"/></svg>"##,
        )
        .unwrap();
        assert_eq!(doc.shapes.len(), 2, "each <use> should produce a shape");
    }

    #[test]
    fn the_fit_transform_scales_and_centres() {
        let doc = SvgDocument::parse_str(CIRCLE).unwrap();
        // A 100x100 view box into a 200x200 destination doubles everything.
        let t = doc.fit_transform(Rect::new(Point::ZERO, Size::new(px(200.0), px(200.0))));
        let p = t.apply(Point::new(px(50.0), px(50.0)));
        assert!((p.x.get() - 100.0).abs() < 0.01, "{p:?}");
        assert!((p.y.get() - 100.0).abs() < 0.01, "{p:?}");
    }

    #[test]
    fn the_fit_transform_preserves_aspect_and_centres_the_letterbox() {
        let doc = SvgDocument::parse_str(CIRCLE).unwrap();
        // 100x100 into 200x100: scale is 1.0, and the result is centred.
        let t = doc.fit_transform(Rect::new(Point::ZERO, Size::new(px(200.0), px(100.0))));
        let origin = t.apply(Point::ZERO);
        assert!((origin.x.get() - 50.0).abs() < 0.01, "expected a 50 px letterbox, got {origin:?}");
        assert!(origin.y.get().abs() < 0.01, "{origin:?}");
    }

    #[test]
    fn a_degenerate_destination_yields_the_identity_rather_than_a_nan() {
        let doc = SvgDocument::parse_str(CIRCLE).unwrap();
        for dest in [
            Rect::new(Point::ZERO, Size::new(Px::ZERO, px(10.0))),
            Rect::new(Point::ZERO, Size::new(px(10.0), Px::ZERO)),
            Rect::ZERO,
        ] {
            let t = doc.fit_transform(dest);
            let p = t.apply(Point::new(px(1.0), px(1.0)));
            assert!(p.x.get().is_finite() && p.y.get().is_finite(), "{dest:?}");
        }
    }

    #[test]
    fn malformed_xml_is_an_error_not_a_panic() {
        assert!(SvgDocument::parse_str("<svg><path d=").is_err());
        assert!(SvgDocument::parse_str("").is_err());
        assert!(SvgDocument::parse_str("not xml at all").is_err());
    }

    #[test]
    fn random_bytes_do_not_panic() {
        let garbage: Vec<u8> = (0..4096).map(|i| (i * 37 % 251) as u8).collect();
        assert!(SvgDocument::parse(&garbage).is_err());
    }

    #[test]
    fn an_empty_document_parses_to_nothing() {
        let doc = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10"></svg>"#,
        )
        .unwrap();
        assert!(doc.is_empty());
        assert_eq!(doc.content_bounds(), Rect::ZERO);
    }

    #[test]
    fn text_is_reported_unsupported_rather_than_dropped() {
        // Silently dropping content is worse than refusing it: the caller has
        // no way to know the icon is wrong.
        let err = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
               <text x="0" y="5">hi</text></svg>"#,
        )
        .unwrap_err();
        assert!(matches!(err, SvgError::Unsupported(ref s) if s.contains("text")), "{err}");
    }

    #[test]
    fn content_bounds_cover_every_visible_shape() {
        let doc = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
               <rect x="10" y="10" width="10" height="10" fill="black"/>
               <rect x="60" y="70" width="10" height="10" fill="black"/></svg>"#,
        )
        .unwrap();
        let b = doc.content_bounds();
        assert!((b.min_x().get() - 10.0).abs() < 0.01, "{b:?}");
        assert!((b.max_x().get() - 70.0).abs() < 0.01, "{b:?}");
        assert!((b.max_y().get() - 80.0).abs() < 0.01, "{b:?}");
    }

    #[test]
    fn an_unfilled_unstroked_shape_is_invisible() {
        let doc = SvgDocument::parse_str(
            r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 10">
               <rect width="10" height="10" fill="none"/></svg>"#,
        )
        .unwrap();
        assert!(doc.is_empty(), "a fill:none rect with no stroke draws nothing");
    }
}
