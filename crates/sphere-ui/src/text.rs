//! Text elements.
//!
//! A [`Label`] is the bridge between the element tree and the text stack. It
//! reports its intrinsic size during layout — which is where shaping actually
//! happens — and emits glyph runs during paint.
//!
//! ## Why measurement and painting both shape
//!
//! They do not, in practice. Both go through
//! [`TextSystem::layout`](sphere_text::TextSystem::layout), which is backed by
//! the shaping cache, so the second call for the same string, style and width
//! is a lookup. Reshaping a paragraph every frame is the largest avoidable cost
//! in a text-heavy interface, and the cache is what removes it.

use crate::element::{AnyElement, Element, IntoElement, PaintContext, Styled};
use crate::style::PaintStyle;
use smallvec::SmallVec;
use sphere_core::{Brush, Color, ElementId, Px, Size};
use sphere_layout::{AvailableSpace, MeasureRequest, Style};
use sphere_render::{GlyphRun, PositionedGlyph, TextRasterMode};
use sphere_text::{TextAlign, TextStyle, TextSystem, WrapMode};

/// A run of text.
pub struct Label {
    id: Option<ElementId>,
    text: String,
    style: Style,
    paint: PaintStyle,
    text_style: TextStyle,
    /// `true` until the caller supplies an explicit weight or a complete text
    /// style. This lets the theme's body-weight token actually reach ordinary
    /// labels while preserving per-label overrides.
    inherit_theme_weight: bool,
    color: Option<Color>,
    raster: TextRasterMode,
    /// Synthetic outline, for text over a busy backdrop such as a waveform.
    outline: Option<(Px, Color)>,
    /// Signed coverage exponent, or `None` to derive it from the contrast
    /// between the text and the theme's background.
    ///
    /// Derived by default because the correction runs in opposite directions
    /// for light-on-dark and dark-on-light, and a constant is therefore wrong
    /// in one of the two themes every application ships.
    coverage_contrast: Option<f32>,
}

/// Creates a [`Label`].
pub fn label(text: impl Into<String>) -> Label {
    Label {
        id: None,
        text: text.into(),
        style: Style::DEFAULT,
        paint: PaintStyle::default(),
        text_style: TextStyle::default(),
        inherit_theme_weight: true,
        color: None,
        raster: TextRasterMode::Auto,
        outline: None,
        coverage_contrast: None,
    }
}

impl Label {
    /// Gives the label a stable identity.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Font size in logical pixels.
    pub fn text_size(mut self, size: Px) -> Self {
        self.text_style.font_size = size;
        self
    }

    /// Text colour. Defaults to the theme's primary text colour.
    pub fn text_color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    /// Font family.
    pub fn font(mut self, family: impl Into<String>) -> Self {
        self.text_style.font.families = vec![family.into()];
        self
    }

    /// Font weight.
    pub fn weight(mut self, weight: sphere_text::FontWeight) -> Self {
        self.text_style.font.weight = weight;
        self.inherit_theme_weight = false;
        self
    }

    /// Italic.
    pub fn italic(mut self) -> Self {
        self.text_style.font.style = sphere_text::FontStyle::Italic;
        self
    }

    /// Horizontal alignment within the label's box.
    pub fn align(mut self, align: TextAlign) -> Self {
        self.text_style.align = align;
        self
    }

    /// Line height in logical pixels, overriding the font's own metrics.
    pub fn line_height(mut self, height: Px) -> Self {
        self.text_style.line_height = Some(height);
        self
    }

    /// Extra space after every grapheme cluster.
    pub fn letter_spacing(mut self, spacing: Px) -> Self {
        self.text_style.letter_spacing = spacing;
        self
    }

    /// Keeps the text on one line.
    pub fn no_wrap(mut self) -> Self {
        self.text_style.wrap = WrapMode::None;
        self
    }

    /// Replaces overflowing text with a horizontal ellipsis.
    pub fn truncate(mut self) -> Self {
        self.text_style.wrap = WrapMode::None;
        self.text_style.overflow = sphere_text::Overflow::Ellipsis;
        self
    }

    /// Forces a rasterisation strategy instead of choosing per glyph.
    pub fn raster_mode(mut self, mode: TextRasterMode) -> Self {
        self.raster = mode;
        self
    }

    /// Draws a synthetic outline behind the glyphs.
    ///
    /// Uses the distance field's true-distance channel, so it costs no extra
    /// rasterisation. Worth having for labels drawn over a waveform or a
    /// spectrum, where no single colour reads against every backdrop.
    pub fn outline(mut self, width: Px, color: Color) -> Self {
        self.outline = Some((width, color));
        self
    }

    /// Overrides the coverage correction.
    ///
    /// A signed exponent: the magnitude sets how hard the coverage ramp bends
    /// and the sign says which end bends, positive for light text on a dark
    /// surface. `1.0` leaves coverage physically linear, which is right only
    /// for text over an image, where there is no one background to correct
    /// against. See [`sphere_render::GlyphRun::coverage_contrast`].
    pub fn coverage_contrast(mut self, contrast: f32) -> Self {
        self.coverage_contrast = Some(contrast);
        self
    }

    /// Replaces the whole text style.
    pub fn text_style(mut self, style: TextStyle) -> Self {
        self.text_style = style;
        self.inherit_theme_weight = false;
        self
    }

    /// Resolves inherited typography without mutating the authored style.
    fn resolved_text_style(&self, theme: &crate::theme::Theme) -> TextStyle {
        let mut style = self.text_style.clone();
        if self.inherit_theme_weight {
            style.font.weight = theme.typography.weight;
        }
        style
    }

    /// The text this label displays.
    #[inline]
    pub fn content(&self) -> &str {
        &self.text
    }
}

impl Styled for Label {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }
    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint
    }
}

impl Element for Label {
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        self.style.clone()
    }

    fn measure(
        &mut self,
        request: &MeasureRequest<'_>,
        text: &mut TextSystem,
        theme: &crate::theme::Theme,
    ) -> Option<Size<Px>> {
        if self.text.is_empty() {
            return Some(Size::ZERO);
        }
        // A known width means "how tall are you at this width" — the wrapping
        // question. `MaxContent` means "how wide on one line". `MinContent`
        // means "your longest unbreakable run".
        let max_width = match (request.known.width, request.available.width) {
            (Some(w), _) => Some(w),
            (None, AvailableSpace::Definite(w)) => Some(w),
            (None, AvailableSpace::MaxContent) => None,
            (None, AvailableSpace::MinContent) => Some(Px::ZERO),
        };
        let style = self.resolved_text_style(theme);
        let layout = text.layout(&self.text, &style, max_width);
        Some(layout.size)
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        self.paint.paint_box(cx.canvas, cx.bounds, cx.state);
        if self.text.is_empty() || cx.is_culled() {
            return;
        }

        let color = self.color.unwrap_or(cx.theme.colors.text);
        let style = self.resolved_text_style(cx.theme);
        let layout = cx.text.layout(&self.text, &style, Some(cx.bounds.width()));
        let origin = cx.bounds.origin;
        let (outline_width, outline_color) = self.outline.unwrap_or((Px::ZERO, Color::TRANSPARENT));
        let coverage_contrast = self.coverage_contrast.unwrap_or_else(|| {
            sphere_render::coverage_contrast_for(color, cx.theme.colors.background)
        });

        draw_layout(
            cx.canvas,
            &layout,
            origin,
            color,
            self.raster,
            (outline_width, outline_color),
            coverage_contrast,
        );
    }

    fn semantics(&self) -> Option<crate::semantics::Semantics> {
        Some(crate::semantics::Semantics::new(crate::semantics::Role::Label, self.text.clone()))
    }
}

/// Records a laid-out paragraph into a canvas at `origin`.
///
/// Shared by [`Label`] and [`crate::widgets::TextField`] so that the two cannot
/// disagree about where a glyph goes. A field draws a caret and a selection
/// against the same layout it draws the text from, and a second copy of this
/// loop would eventually drift from the first.
pub fn draw_layout(
    canvas: &mut sphere_render::Canvas<'_>,
    layout: &sphere_text::TextLayout,
    origin: sphere_core::Point<Px>,
    color: Color,
    raster: TextRasterMode,
    outline: (Px, Color),
    coverage_contrast: f32,
) {
    for line in &layout.lines {
        for run in &line.runs {
            if run.glyphs.is_empty() {
                continue;
            }
            // One `GlyphRun` per shaped run: the batch compiler needs a single
            // face and size per run so it can resolve every glyph against the
            // same atlas page.
            let glyphs: SmallVec<[PositionedGlyph; 8]> = run
                .glyphs
                .iter()
                .map(|g| PositionedGlyph {
                    glyph: g.glyph,
                    position: sphere_core::Point::new(
                        origin.x + g.position.x,
                        origin.y + line.baseline + g.position.y,
                    ),
                })
                .collect();

            canvas.draw_glyph_run(
                GlyphRun {
                    font: run.font,
                    font_size: run.font_size,
                    glyphs,
                    raster,
                    outline_width: outline.0,
                    outline_color: outline.1,
                    coverage_contrast,
                },
                Brush::Solid(color),
            );
        }
    }
}

impl IntoElement for &str {
    fn into_element(self) -> AnyElement {
        Box::new(label(self))
    }
}

impl IntoElement for String {
    fn into_element(self) -> AnyElement {
        Box::new(label(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{ParentElement, div};
    use crate::theme::Theme;
    use crate::tree::UiTree;
    use sphere_core::{ScaleFactor, px, relative, size};
    use sphere_render::{Canvas, DrawCommand, Scene};

    fn viewport() -> Size<Px> {
        size(px(400.0), px(300.0))
    }

    /// A text system with a real face, or `None` on a machine with no fonts.
    fn text_system() -> Option<TextSystem> {
        let mut system = TextSystem::with_system_fonts();
        system.fonts_mut().resolve(&sphere_text::FontRequest::default())?;
        Some(system)
    }

    #[test]
    fn a_str_becomes_a_label() {
        let mut d = div().child("Threshold");
        assert_eq!(d.children().len(), 1);
    }

    #[test]
    fn a_label_inherits_theme_weight_and_an_explicit_weight_wins() {
        let mut theme = Theme::dark();
        theme.typography.weight = sphere_text::FontWeight::BOLD;

        assert_eq!(
            label("Inherited").resolved_text_style(&theme).font.weight,
            sphere_text::FontWeight::BOLD
        );
        assert_eq!(
            label("Override")
                .weight(sphere_text::FontWeight::LIGHT)
                .resolved_text_style(&theme)
                .font
                .weight,
            sphere_text::FontWeight::LIGHT
        );
    }

    #[test]
    fn label_weight_reaches_the_painted_glyph_run() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut tree = UiTree::new();
        tree.build(
            div()
                .child(label("Regular").weight(sphere_text::FontWeight::NORMAL))
                .child(label("SemiBold").weight(sphere_text::FontWeight::SEMI_BOLD))
                .into_element(),
        );
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();

        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut system, viewport(), 0.0);
        }
        assert_eq!(scene.runs.len(), 2, "each label should produce one shaped run");
        assert_ne!(
            scene.runs[0].font, scene.runs[1].font,
            "regular and semi-bold were painted with the same face"
        );
    }

    #[test]
    fn an_empty_label_measures_zero_and_paints_nothing() {
        let mut system = TextSystem::new();
        let mut tree = UiTree::new();
        tree.build(label("").into_element());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();

        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut system, viewport(), 0.0);
        }
        assert!(scene.is_empty());
    }

    #[test]
    fn a_label_with_no_fonts_available_does_not_panic() {
        // A stripped container image has no fonts at all; the engine must
        // degrade to drawing nothing rather than crashing.
        let mut system = TextSystem::new();
        let mut tree = UiTree::new();
        tree.build(label("Hello").into_element());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut system, viewport(), 0.0);
        }
    }

    #[test]
    fn a_label_reports_a_nonzero_intrinsic_size() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut tree = UiTree::new();
        tree.build(label("Threshold").text_size(px(14.0)).into_element());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();

        let node = tree.layout().roots()[0];
        let bounds = tree.layout().layout(node).unwrap().bounds;
        assert!(bounds.width() > px(10.0), "label measured {bounds:?}");
        assert!(bounds.height() > px(5.0), "label measured {bounds:?}");
    }

    #[test]
    fn a_longer_string_measures_wider() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let measure = |s: &str, system: &mut TextSystem| {
            let mut tree = UiTree::new();
            tree.build(label(s).text_size(px(14.0)).into_element());
            tree.compute_layout_with_text(viewport(), system).unwrap();
            let node = tree.layout().roots()[0];
            tree.layout().layout(node).unwrap().bounds.width()
        };
        assert!(measure("Threshold and more", &mut system) > measure("Th", &mut system));
    }

    #[test]
    fn a_label_emits_glyph_runs_when_painted() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut tree = UiTree::new();
        tree.build(label("Threshold").text_size(px(14.0)).w(relative(1.0)).into_element());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();

        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut system, viewport(), 0.0);
        }
        let runs = scene.commands.iter().filter(|c| matches!(c, DrawCommand::Text { .. })).count();
        assert!(runs > 0, "a painted label produced no glyph runs");
        assert!(scene.runs.iter().all(|r| !r.glyphs.is_empty()));
    }

    #[test]
    fn glyphs_are_positioned_inside_the_labels_box() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut tree = UiTree::new();
        tree.build(div().p(px(20.0)).child(label("Hi").text_size(px(14.0))).into_element());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();

        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut system, viewport(), 0.0);
        }
        // Padding must actually offset the glyphs; a label that ignores its own
        // box origin draws at 0,0 and looks fine only when there is no padding.
        for run in &scene.runs {
            for g in &run.glyphs {
                assert!(g.position.x >= px(19.0), "glyph at {:?} ignored padding", g.position);
            }
        }
    }

    #[test]
    fn measuring_twice_hits_the_shaping_cache() {
        // Reshaping every frame is the cost this cache exists to remove.
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let style = TextStyle::default();
        system.layout("Threshold", &style, Some(px(200.0)));
        let after_first = system.shape_stats();
        system.layout("Threshold", &style, Some(px(200.0)));
        let after_second = system.shape_stats();
        assert!(after_second.hits > after_first.hits, "the second layout missed the cache");
    }

    #[test]
    fn a_paint_only_frame_does_no_shaping_work() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut tree = UiTree::new();
        let build = || label("Threshold").id("l").text_size(px(14.0)).into_element();
        tree.build(build());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();

        tree.build(build());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();
        assert_eq!(
            tree.stats().nodes_laid_out,
            0,
            "an unchanged label was measured again, which means it was reshaped"
        );
    }

    #[test]
    fn an_outline_is_carried_into_the_glyph_run() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut tree = UiTree::new();
        tree.build(
            label("Peak")
                .text_size(px(14.0))
                .w(relative(1.0))
                .outline(px(1.5), Color::BLACK)
                .into_element(),
        );
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut system, viewport(), 0.0);
        }
        assert!(scene.runs.iter().all(|r| r.outline_width == px(1.5)));
    }

    #[test]
    fn the_theme_supplies_the_default_text_colour() {
        let Some(mut system) = text_system() else {
            eprintln!("no system font; skipping");
            return;
        };
        let mut tree = UiTree::new();
        tree.set_theme(Theme::light());
        tree.build(label("Hi").text_size(px(14.0)).w(relative(1.0)).into_element());
        tree.compute_layout_with_text(viewport(), &mut system).unwrap();
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut system, viewport(), 0.0);
        }
        let expected = Theme::light().colors.text;
        assert!(scene.paints.iter().any(|p| p.brush == Brush::Solid(expected)));
    }
}
