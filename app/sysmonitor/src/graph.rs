//! The two drawings a monitor needs that no widget library ships.
//!
//! A [`Graph`] is a history over time and a [`Meter`] is a proportion of a
//! fixed whole. Both are custom [`Element`]s rather than compositions of
//! `div`s, for the same reason the gallery's caption buttons are: they need the
//! canvas, and a chart made of a hundred one-pixel rectangles would be a
//! hundred nodes in the tree every frame.
//!
//! Neither owns any state. A `Graph` is handed the samples it should draw and a
//! `Meter` is handed a fraction; the ring buffer behind the first lives in the
//! application, which is what lets the sampler write it on its own schedule
//! while the tree is rebuilt on the renderer's.

use spherekit::core::{Color, Gradient, GradientStop, Length, Path, Point, Px, Rect, Stroke, px};
use spherekit::layout::Style;
use spherekit::ui::{Element, PaintContext};

/// A filled line chart of recent history, oldest sample first.
///
/// The samples are already normalised to `0..=1` — the element does not scale
/// to the data. A CPU graph whose axis rescaled to the maximum sample would
/// make an idle machine look identical to a busy one, which is the single most
/// misleading thing a monitor can do.
pub struct Graph {
    /// Oldest first, `0..=1`. Fewer than the capacity means the chart is still
    /// filling and is drawn against the right-hand edge.
    pub samples: Vec<f32>,
    /// How many samples the axis is wide, whatever `samples` currently holds.
    pub capacity: usize,
    pub line: Color,
    /// The area under the line, at full strength; it is ramped to nothing at
    /// the baseline by the element itself.
    pub fill: Color,
    pub grid: Color,
    pub height: Px,
}

impl Graph {
    pub fn new(samples: Vec<f32>, capacity: usize, line: Color, height: Px) -> Self {
        Self { samples, capacity, line, fill: line, grid: line.with_alpha(0.10), height }
    }

    pub fn fill(mut self, color: Color) -> Self {
        self.fill = color;
        self
    }

    /// Overrides the rule colour. The default is the line at a tenth alpha,
    /// which is right for a chart on a card and wrong for one on Mica.
    #[allow(dead_code)]
    pub fn grid(mut self, color: Color) -> Self {
        self.grid = color;
        self
    }
}

/// How many cells the graph is ruled into, both ways. Four is what Task Manager
/// uses, and the point of a grid here is to be a scale rather than a decoration.
const GRID_CELLS: usize = 4;

impl Element for Graph {
    fn layout_style(&self) -> Style {
        Style {
            size: spherekit::core::Size {
                width: Length::Fraction(1.0),
                height: Length::Px(self.height),
            },
            flex_shrink: 0.0,
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        let b = cx.bounds;
        if b.width() <= Px::ZERO || b.height() <= Px::ZERO {
            return;
        }

        // --- the rule -------------------------------------------------------
        for i in 1..GRID_CELLS {
            let t = i as f32 / GRID_CELLS as f32;
            let y = b.min_y() + b.height() * t;
            cx.canvas.draw_line(
                Point::new(b.min_x(), y),
                Point::new(b.max_x(), y),
                self.grid,
                px(1.0),
            );
            let x = b.min_x() + b.width() * t;
            cx.canvas.draw_line(
                Point::new(x, b.min_y()),
                Point::new(x, b.max_y()),
                self.grid,
                px(1.0),
            );
        }

        // A single sample is a value, not a history: there is no segment to
        // draw between one point and itself.
        if self.samples.len() < 2 {
            return;
        }

        // The chart is anchored to the *right*, so the newest sample is always
        // under the same pixel and a partly-filled buffer grows leftward into
        // the empty space rather than stretching to fill it. A chart that
        // rescaled its time axis as it filled would appear to speed up.
        let span = self.capacity.max(2) - 1;
        let step = b.width().get() / span as f32;
        let right = b.max_x().get();
        let last = self.samples.len() - 1;
        let point_at = |i: usize| {
            let x = right - (last - i) as f32 * step;
            let y = b.max_y().get() - self.samples[i].clamp(0.0, 1.0) * b.height().get();
            Point::new(Px(x.max(b.min_x().get())), Px(y))
        };

        // --- the area -------------------------------------------------------
        let mut area = Path::builder();
        area.move_to(Point::new(point_at(0).x, b.max_y()));
        for i in 0..self.samples.len() {
            area.line_to(point_at(i));
        }
        area.line_to(Point::new(point_at(last).x, b.max_y()));
        area.close();

        // Ramped to transparent at the baseline. A flat wash would read as a
        // solid block and hide the grid it is drawn over; the ramp keeps the
        // line itself the thing the eye lands on.
        let mut ramp = Gradient::vertical(b.height(), self.fill, self.fill);
        if let Gradient::Linear { start, end, stops } = &mut ramp {
            *start = Point::new(b.min_x(), b.min_y());
            *end = Point::new(b.min_x(), b.max_y());
            stops.clear();
            stops.push(GradientStop::new(0.0, self.fill.with_alpha(0.34)));
            stops.push(GradientStop::new(1.0, self.fill.with_alpha(0.02)));
        }
        cx.canvas.fill_path(area.build(), ramp);

        // --- the line -------------------------------------------------------
        let mut line = Path::builder();
        line.move_to(point_at(0));
        for i in 1..self.samples.len() {
            line.line_to(point_at(i));
        }
        cx.canvas.stroke_path(
            line.build(),
            self.line,
            Stroke {
                width: px(1.5),
                // Rounded joins because the series is spiky by nature: a miter
                // on a sample that jumps from 4% to 90% produces a spike far
                // outside the chart.
                join: spherekit::core::LineJoin::Round,
                cap: spherekit::core::LineCap::Round,
                ..Stroke::default()
            },
        );
    }
}

/// A proportion of a fixed whole, drawn as a track with a rounded fill.
///
/// Not [`progress`](spherekit::ui::progress): that widget means "work is under
/// way and this much of it is done", and this one means "this much of a fixed
/// resource is in use". They look similar and say different things — a full
/// progress bar is good news and a full meter is not — so this one can colour
/// itself by threshold, which a progress bar must never do.
pub struct Meter {
    /// `0..=1`. Values outside are clamped rather than drawn outside the track.
    pub fraction: f32,
    pub track: Color,
    pub fill: Color,
    pub height: Px,
}

impl Meter {
    pub fn new(fraction: f32, track: Color, fill: Color) -> Self {
        Self { fraction: fraction.clamp(0.0, 1.0), track, fill, height: px(6.0) }
    }

    pub fn height(mut self, height: Px) -> Self {
        self.height = height;
        self
    }
}

impl Element for Meter {
    fn layout_style(&self) -> Style {
        Style {
            size: spherekit::core::Size {
                width: Length::Fraction(1.0),
                height: Length::Px(self.height),
            },
            flex_shrink: 0.0,
            ..Style::DEFAULT
        }
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        use spherekit::core::RoundedRect;
        let b = cx.bounds;
        if b.width() <= Px::ZERO {
            return;
        }
        let radius = b.height() * 0.5;
        cx.canvas.fill_rounded_rect(RoundedRect::uniform(b, radius), self.track);

        let width = b.width() * self.fraction;
        // A one-pixel fill on a six-pixel track with a three-pixel radius is
        // not a shape; below the diameter there is nothing honest to draw.
        if width < b.height() {
            return;
        }
        let filled = Rect::from_corners(b.origin, Point::new(b.min_x() + width, b.max_y()));
        cx.canvas.fill_rounded_rect(RoundedRect::uniform(filled, radius), self.fill);
    }
}
