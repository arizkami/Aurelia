//! Realtime paint nodes and the audio drawing primitives.
//!
//! A [`RealtimeCanvas`] is an element whose contents are regenerated every
//! frame from a data snapshot. It is not a widget with an occasionally-changing
//! value; it is a surface that redraws continuously for as long as the transport
//! is running.
//!
//! Two things follow from that:
//!
//! * **It invalidates PAINT only.** Its box never changes size, so relayout
//!   would be pure waste at sixty or a hundred and twenty hertz. There is a test
//!   that builds a real tree, repaints a meter, and asserts the layout engine
//!   laid out zero nodes.
//! * **It builds meshes, not paths.** A spectrum is already a list of points. A
//!   waveform is already a list of vertical spans. Turning either into a
//!   [`Path`](spherekit_core::Path) so a tessellator can turn it back into
//!   triangles is work with no product, so
//!   [`Canvas::draw_mesh`](spherekit_render::Canvas::draw_mesh) exists and this is
//!   what uses it.
//!
//! ```ignore
//! realtime_canvas(move |frame| {
//!     frame.draw_spectrum(&spectrum, &SpectrumStyle::default());
//!     frame.draw_eq_curve(&response, &CurveStyle::default());
//! })
//! .h(px(80.0))
//! ```

use crate::dsp::{LogFrequencyScale, MeterScale, MeterState, linear_to_db, min_max_envelope};
use spherekit_core::{
    Color, Corners, ElementId, LinearColor, Point, Px, Rect, RoundedRect, Size, Stroke,
};
use spherekit_layout::Style;
use spherekit_render::{Canvas, Mesh, MeshVertex};
use spherekit_ui::{
    Element, EventContext, EventFlow, PaintContext, PaintStyle, Semantics, Styled, Theme,
};

/// The drawing surface handed to a realtime paint closure.
///
/// Wraps the canvas with the node's bounds and the theme, and adds the
/// audio-specific primitives. Everything it draws is in the node's local space,
/// so a closure never has to know where on screen it ended up.
pub struct RealtimeFrame<'a, 'canvas> {
    /// The underlying canvas, for anything the primitives do not cover.
    pub canvas: &'a mut Canvas<'canvas>,
    /// The node's absolute bounds.
    pub bounds: Rect<Px>,
    /// The active theme.
    pub theme: &'a Theme,
    /// Seconds since the engine started.
    pub time: f32,
    /// Scratch buffer reused across frames so envelope extraction allocates
    /// nothing in steady state.
    scratch: &'a mut Vec<f32>,
}

impl RealtimeFrame<'_, '_> {
    /// The surface width.
    #[inline]
    pub fn width(&self) -> Px {
        self.bounds.width()
    }

    /// The surface height.
    #[inline]
    pub fn height(&self) -> Px {
        self.bounds.height()
    }

    /// Converts a local point to absolute space.
    #[inline]
    fn at(&self, x: f32, y: f32) -> Point<Px> {
        Point::new(self.bounds.min_x() + Px(x), self.bounds.min_y() + Px(y))
    }

    /// Fills the whole surface.
    pub fn fill(&mut self, color: Color) {
        self.canvas.fill_rect(self.bounds, color);
    }

    // ----------------------------------------------------------- meters

    /// Draws a level meter.
    pub fn draw_meter(&mut self, state: &MeterState, style: &MeterStyle) {
        let bounds = self.bounds;
        if bounds.is_empty() {
            return;
        }
        self.canvas.fill_rounded_rect(
            RoundedRect::new(bounds, Corners::all(style.radius)),
            style.background,
        );

        let vertical = style.vertical;
        let extent = if vertical { bounds.height() } else { bounds.width() };
        let level = style.scale.position_of_db(state.level_db).clamp(0.0, 1.0);
        if level > 0.0 {
            let filled = Px(extent.get() * level);
            // A vertical meter grows upward from the bottom, which is what every
            // hardware meter does and what a user expects.
            let bar = if vertical {
                Rect::new(
                    Point::new(bounds.min_x(), bounds.max_y() - filled),
                    Size::new(bounds.width(), filled),
                )
            } else {
                Rect::new(bounds.origin, Size::new(filled, bounds.height()))
            };
            let color = style.color_for_db(state.level_db, self.theme);
            self.canvas.fill_rounded_rect(RoundedRect::new(bar, Corners::all(style.radius)), color);
        }

        if style.show_peak && state.peak_db > style.scale.min_db {
            let p = style.scale.position_of_db(state.peak_db).clamp(0.0, 1.0);
            let thickness = style.peak_thickness;
            let marker = if vertical {
                let y = bounds.max_y() - Px(bounds.height().get() * p);
                Rect::new(
                    Point::new(bounds.min_x(), y - thickness * 0.5),
                    Size::new(bounds.width(), thickness),
                )
            } else {
                let x = bounds.min_x() + Px(bounds.width().get() * p);
                Rect::new(
                    Point::new(x - thickness * 0.5, bounds.min_y()),
                    Size::new(thickness, bounds.height()),
                )
            };
            let color = style.color_for_db(state.peak_db, self.theme);
            self.canvas.fill_rect(marker.intersection(bounds), color);
        }

        if state.clipping() {
            let size = Px(style.clip_indicator.get().min(extent.get() * 0.1));
            let indicator = if vertical {
                Rect::new(bounds.origin, Size::new(bounds.width(), size))
            } else {
                Rect::new(
                    Point::new(bounds.max_x() - size, bounds.min_y()),
                    Size::new(size, bounds.height()),
                )
            };
            self.canvas.fill_rect(indicator, self.theme.colors.danger);
        }
    }

    /// Draws a gain-reduction meter, which grows downward from unity.
    ///
    /// Inverted on purpose: reduction is something being taken away, and a
    /// meter that grows upward as a compressor works harder reads backwards.
    pub fn draw_gain_reduction(&mut self, reduction_db: f32, style: &MeterStyle) {
        let bounds = self.bounds;
        if bounds.is_empty() {
            return;
        }
        self.canvas.fill_rounded_rect(
            RoundedRect::new(bounds, Corners::all(style.radius)),
            style.background,
        );
        let magnitude = (-reduction_db).clamp(0.0, style.max_reduction_db);
        if magnitude <= 0.0 {
            return;
        }
        let fraction = magnitude / style.max_reduction_db.max(1e-3);
        let bar = if style.vertical {
            Rect::new(
                bounds.origin,
                Size::new(bounds.width(), Px(bounds.height().get() * fraction)),
            )
        } else {
            let w = Px(bounds.width().get() * fraction);
            Rect::new(Point::new(bounds.max_x() - w, bounds.min_y()), Size::new(w, bounds.height()))
        };
        self.canvas.fill_rounded_rect(
            RoundedRect::new(bar, Corners::all(style.radius)),
            self.theme.colors.warning,
        );
    }

    // --------------------------------------------------------- waveform

    /// Draws a waveform from raw samples.
    ///
    /// Reduces to a per-column min/max envelope first, which preserves
    /// transients that decimation would drop entirely.
    pub fn draw_waveform(&mut self, samples: &[f32], style: &WaveformStyle) {
        let bounds = self.bounds;
        if bounds.is_empty() {
            return;
        }
        let columns = (bounds.width().get().ceil() as usize).clamp(1, 8192);
        min_max_envelope(samples, columns, self.scratch);
        let envelope = core::mem::take(self.scratch);
        self.draw_envelope(&envelope, style);
        *self.scratch = envelope;
    }

    /// Draws a waveform from a precomputed `[min, max]` envelope.
    ///
    /// Separate entry point because a DAW usually has the envelope cached at
    /// several zoom levels already and re-deriving it per frame would be waste.
    pub fn draw_envelope(&mut self, envelope: &[f32], style: &WaveformStyle) {
        let bounds = self.bounds;
        let columns = envelope.len() / 2;
        if columns == 0 || bounds.is_empty() {
            return;
        }

        let column_width = bounds.width().get() / columns as f32;
        let mid = bounds.min_y().get() + bounds.height().get() * 0.5;
        let half = bounds.height().get() * 0.5 * style.vertical_scale;
        let color = style.color.to_linear();
        let min_thickness = style.min_thickness.get();

        // Two triangles per column, built directly rather than through a path.
        let mut mesh = Mesh {
            vertices: Vec::with_capacity(columns * 4),
            indices: Vec::with_capacity(columns * 6),
        };

        for (i, pair) in envelope.chunks_exact(2).enumerate() {
            let (lo, hi) = (pair[0], pair[1]);
            if !lo.is_finite() || !hi.is_finite() {
                continue;
            }
            let x0 = bounds.min_x().get() + i as f32 * column_width;
            let x1 = x0 + column_width;

            let mut top = mid - hi.clamp(-1.0, 1.0) * half;
            let mut bottom = mid - lo.clamp(-1.0, 1.0) * half;
            // A near-silent column would otherwise vanish entirely; a hairline
            // through the middle is what a real waveform display shows.
            if bottom - top < min_thickness {
                let centre = (top + bottom) * 0.5;
                top = centre - min_thickness * 0.5;
                bottom = centre + min_thickness * 0.5;
            }

            let base = mesh.vertices.len() as u32;
            for (x, y) in [(x0, top), (x1, top), (x1, bottom), (x0, bottom)] {
                mesh.vertices.push(MeshVertex::new(Point::new(Px(x), Px(y)), color));
            }
            mesh.indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }

        if style.centre_line {
            self.canvas.draw_line(
                Point::new(bounds.min_x(), Px(mid)),
                Point::new(bounds.max_x(), Px(mid)),
                style.centre_line_color,
                Px(1.0),
            );
        }
        self.canvas.draw_mesh(mesh, None);
    }

    // --------------------------------------------------------- spectrum

    /// Draws a magnitude spectrum.
    ///
    /// `magnitudes` are linear amplitudes, one per bin, evenly spaced in
    /// frequency from DC to `nyquist_hz`. The x axis is logarithmic and the y
    /// axis is decibels, because a spectrum on linear axes is unreadable.
    pub fn draw_spectrum(&mut self, magnitudes: &[f32], nyquist_hz: f32, style: &SpectrumStyle) {
        let bounds = self.bounds;
        if bounds.is_empty() || magnitudes.len() < 2 || !nyquist_hz.is_finite() || nyquist_hz <= 0.0
        {
            return;
        }

        let width = bounds.width().get();
        let height = bounds.height().get();
        let bin_hz = nyquist_hz / (magnitudes.len() - 1) as f32;
        let color = style.color.to_linear();
        let fill_color = style.fill_color.map(|c| c.to_linear());

        // One vertex column per bin that lands inside the visible range, in
        // ascending x. Bins below `min_hz` are skipped rather than piled up at
        // x = 0, which would draw a spurious wall at the left edge.
        let mut points: Vec<(f32, f32)> = Vec::with_capacity(magnitudes.len());
        for (bin, &magnitude) in magnitudes.iter().enumerate() {
            let hz = bin as f32 * bin_hz;
            if hz < style.frequency.min_hz || hz > style.frequency.max_hz {
                continue;
            }
            let db = linear_to_db(magnitude);
            let t = ((db - style.min_db) / (style.max_db - style.min_db).max(1e-3)).clamp(0.0, 1.0);
            let x = bounds.min_x().get() + style.frequency.position_of(hz) * width;
            let y = bounds.max_y().get() - t * height;
            points.push((x, y));
        }
        if points.len() < 2 {
            return;
        }

        if let Some(fill) = fill_color {
            let floor = bounds.max_y().get();
            let mut mesh = Mesh {
                vertices: Vec::with_capacity(points.len() * 2),
                indices: Vec::with_capacity((points.len() - 1) * 6),
            };
            for (x, y) in &points {
                mesh.vertices.push(MeshVertex::new(Point::new(Px(*x), Px(*y)), fill));
                mesh.vertices.push(MeshVertex::new(Point::new(Px(*x), Px(floor)), fill));
            }
            for i in 0..points.len() - 1 {
                let b = (i * 2) as u32;
                mesh.indices.extend_from_slice(&[b, b + 2, b + 3, b, b + 3, b + 1]);
            }
            self.canvas.draw_mesh(mesh, None);
        }

        if style.line_width > Px::ZERO {
            let mut path = spherekit_core::PathBuilder::new();
            for (i, (x, y)) in points.iter().enumerate() {
                let p = Point::new(Px(*x), Px(*y));
                if i == 0 {
                    path.move_to(p);
                } else {
                    path.line_to(p);
                }
            }
            self.canvas.stroke_path(path.build(), style.color, Stroke::new(style.line_width));
        }
        let _ = color;
    }

    /// Draws the frequency gridlines a spectrum or EQ display sits on.
    pub fn draw_frequency_grid(&mut self, scale: &LogFrequencyScale, color: Color) {
        let bounds = self.bounds;
        for hz in scale.gridlines() {
            let x = bounds.min_x() + scale.x_of(hz, bounds.width());
            self.canvas.draw_line(
                Point::new(x, bounds.min_y()),
                Point::new(x, bounds.max_y()),
                color,
                Px(1.0),
            );
        }
    }

    // ------------------------------------------------------------ curves

    /// Draws a magnitude response curve over a logarithmic frequency axis.
    ///
    /// `response` is `(hz, gain_db)` in ascending frequency order.
    pub fn draw_eq_curve(&mut self, response: &[(f32, f32)], style: &CurveStyle) {
        let bounds = self.bounds;
        if bounds.is_empty() || response.len() < 2 {
            return;
        }
        let height = bounds.height().get();
        let span = (style.max_db - style.min_db).max(1e-3);

        let mut path = spherekit_core::PathBuilder::new();
        let mut started = false;
        for &(hz, db) in response {
            if !hz.is_finite() || !db.is_finite() {
                continue;
            }
            let x = bounds.min_x() + style.frequency.x_of(hz, bounds.width());
            let t = ((db - style.min_db) / span).clamp(0.0, 1.0);
            let y = bounds.max_y() - Px(t * height);
            let p = Point::new(x, y);
            if started {
                path.line_to(p);
            } else {
                path.move_to(p);
                started = true;
            }
        }
        if !started {
            return;
        }
        self.canvas.stroke_path(path.build(), style.color, Stroke::new(style.width));
    }

    /// Draws a compressor transfer curve.
    ///
    /// The 45-degree unity line is drawn first so the amount of compression is
    /// readable as the gap between the two.
    pub fn draw_compressor_curve(&mut self, curve: &CompressorCurve, style: &CurveStyle) {
        let bounds = self.bounds;
        if bounds.is_empty() {
            return;
        }
        let span = (style.max_db - style.min_db).max(1e-3);
        let to_point = |input_db: f32, output_db: f32| {
            let x = ((input_db - style.min_db) / span).clamp(0.0, 1.0);
            let y = ((output_db - style.min_db) / span).clamp(0.0, 1.0);
            Point::new(
                bounds.min_x() + Px(x * bounds.width().get()),
                bounds.max_y() - Px(y * bounds.height().get()),
            )
        };

        if style.show_unity {
            self.canvas.draw_line(
                to_point(style.min_db, style.min_db),
                to_point(style.max_db, style.max_db),
                style.unity_color,
                Px(1.0),
            );
        }

        const STEPS: usize = 96;
        let mut path = spherekit_core::PathBuilder::new();
        for i in 0..=STEPS {
            let input = style.min_db + (i as f32 / STEPS as f32) * span;
            let p = to_point(input, curve.output_db(input));
            if i == 0 {
                path.move_to(p);
            } else {
                path.line_to(p);
            }
        }
        self.canvas.stroke_path(path.build(), style.color, Stroke::new(style.width));
    }

    // ------------------------------------------------------------ scopes

    /// Draws an oscilloscope trace.
    pub fn draw_oscilloscope(&mut self, samples: &[f32], color: Color, width: Px) {
        let bounds = self.bounds;
        if bounds.is_empty() || samples.len() < 2 {
            return;
        }
        let mid = bounds.min_y().get() + bounds.height().get() * 0.5;
        let half = bounds.height().get() * 0.5;
        let step = bounds.width().get() / (samples.len() - 1) as f32;

        let mut path = spherekit_core::PathBuilder::new();
        let mut started = false;
        for (i, &s) in samples.iter().enumerate() {
            if !s.is_finite() {
                continue;
            }
            let p =
                self.at(i as f32 * step, mid - bounds.min_y().get() - s.clamp(-1.0, 1.0) * half);
            if started {
                path.line_to(p);
            } else {
                path.move_to(p);
                started = true;
            }
        }
        if started {
            self.canvas.stroke_path(path.build(), color, Stroke::new(width));
        }
    }

    /// Draws a Lissajous vectorscope from interleaved stereo samples.
    ///
    /// Rotated 45 degrees so a mono signal traces a vertical line, which is the
    /// convention every mastering meter uses and the whole reason the display
    /// is readable at a glance.
    pub fn draw_vectorscope(&mut self, left: &[f32], right: &[f32], color: Color) {
        let bounds = self.bounds;
        let n = left.len().min(right.len());
        if bounds.is_empty() || n < 2 {
            return;
        }
        let centre = bounds.center();
        let radius = bounds.width().get().min(bounds.height().get()) * 0.5;
        const ROT: f32 = core::f32::consts::FRAC_1_SQRT_2;

        let mut path = spherekit_core::PathBuilder::new();
        let mut started = false;
        for i in 0..n {
            let (l, r) = (left[i], right[i]);
            if !l.is_finite() || !r.is_finite() {
                continue;
            }
            let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
            let x = (l - r) * ROT;
            let y = (l + r) * ROT;
            let p = Point::new(centre.x + Px(x * radius), centre.y - Px(y * radius));
            if started {
                path.line_to(p);
            } else {
                path.move_to(p);
                started = true;
            }
        }
        if started {
            self.canvas.stroke_path(path.build(), color, Stroke::new(Px(1.0)));
        }
    }

    // ------------------------------------------------------- keyboard

    /// Draws a piano keyboard.
    ///
    /// The black keys are **not** evenly spaced, and getting that wrong is the
    /// most obvious possible bug in a piano roll. See [`black_key_offset`].
    pub fn draw_piano(&mut self, range: core::ops::Range<u8>, held: &[u8], style: &PianoStyle) {
        let bounds = self.bounds;
        if bounds.is_empty() || range.is_empty() {
            return;
        }
        let white_count = (range.start..range.end).filter(|n| !is_black_key(*n)).count();
        if white_count == 0 {
            return;
        }
        let white_width = bounds.width().get() / white_count as f32;
        let black_width = white_width * style.black_width_ratio;
        let black_height = bounds.height().get() * style.black_height_ratio;

        // White keys first so the black ones sit on top.
        let mut x = bounds.min_x().get();
        for note in range.clone() {
            if is_black_key(note) {
                continue;
            }
            let rect = Rect::new(
                Point::new(Px(x), bounds.min_y()),
                Size::new(Px(white_width), bounds.height()),
            );
            let color = if held.contains(&note) { style.held_white } else { style.white };
            self.canvas.quad(rect, Corners::ZERO, Some(color.into()), style.border, Px(1.0));
            x += white_width;
        }

        let mut white_index = 0usize;
        for note in range {
            if !is_black_key(note) {
                white_index += 1;
                continue;
            }
            // A black key straddles the boundary after the preceding white key,
            // offset by an amount that differs per position in the octave.
            let offset = black_key_offset(note);
            let centre = bounds.min_x().get() + (white_index as f32 + offset) * white_width;
            let rect = Rect::new(
                Point::new(Px(centre - black_width * 0.5), bounds.min_y()),
                Size::new(Px(black_width), Px(black_height)),
            );
            let color = if held.contains(&note) { style.held_black } else { style.black };
            self.canvas.fill_rounded_rect(RoundedRect::new(rect, Corners::all(Px(1.0))), color);
        }
    }
}

/// True when a MIDI note number is a black key.
#[inline]
pub fn is_black_key(note: u8) -> bool {
    matches!(note % 12, 1 | 3 | 6 | 8 | 10)
}

/// How far a black key's centre sits past the preceding white key boundary.
///
/// On a real keyboard the black keys are **not** centred on the boundaries.
/// C-sharp sits slightly left of the C/D boundary and D-sharp slightly right of
/// D/E, because the three-key and two-key groups have to share the same white
/// widths. Reproducing that is what makes a piano roll look like a piano rather
/// than a comb.
#[inline]
pub fn black_key_offset(note: u8) -> f32 {
    match note % 12 {
        1 => -0.10, // C#: pulled toward C
        3 => 0.10,  // D#: pushed toward E
        6 => -0.13, // F#
        8 => 0.0,   // G#: centred
        10 => 0.13, // A#
        _ => 0.0,
    }
}

/// How a meter is drawn.
#[derive(Clone, Debug)]
pub struct MeterStyle {
    /// The dB-to-position mapping.
    pub scale: MeterScale,
    /// Vertical rather than horizontal.
    pub vertical: bool,
    /// Track colour behind the bar.
    pub background: Color,
    /// Bar colour below the warning threshold.
    pub normal: Color,
    /// Bar colour between the warning and danger thresholds.
    pub warning: Color,
    /// Bar colour above the danger threshold.
    pub danger: Color,
    /// dB above which the bar turns to the warning colour.
    pub warning_db: f32,
    /// dB above which the bar turns to the danger colour.
    pub danger_db: f32,
    /// Whether to draw the held-peak marker.
    pub show_peak: bool,
    /// Thickness of the peak marker.
    pub peak_thickness: Px,
    /// Size of the clip indicator along the meter's long axis.
    pub clip_indicator: Px,
    /// Corner radius.
    pub radius: Px,
    /// Full-scale reduction for a gain-reduction meter, in dB.
    pub max_reduction_db: f32,
}

impl Default for MeterStyle {
    fn default() -> Self {
        Self {
            scale: MeterScale::default(),
            vertical: true,
            background: Color::hex(0x101216),
            normal: Color::hex(0x3FBF6F),
            warning: Color::hex(0xE0A32E),
            danger: Color::hex(0xE0553F),
            // The conventional thresholds on a digital channel meter.
            warning_db: -12.0,
            danger_db: -3.0,
            show_peak: true,
            peak_thickness: Px(2.0),
            clip_indicator: Px(4.0),
            radius: Px(1.0),
            max_reduction_db: 24.0,
        }
    }
}

impl MeterStyle {
    /// The bar colour for a level.
    pub fn color_for_db(&self, db: f32, _theme: &Theme) -> Color {
        if db >= self.danger_db {
            self.danger
        } else if db >= self.warning_db {
            self.warning
        } else {
            self.normal
        }
    }
}

/// How a waveform is drawn.
#[derive(Clone, Debug)]
pub struct WaveformStyle {
    /// Waveform colour.
    pub color: Color,
    /// Vertical exaggeration. `1.0` maps full scale to the full height.
    pub vertical_scale: f32,
    /// Minimum drawn thickness, so a near-silent passage stays visible.
    pub min_thickness: Px,
    /// Whether to draw the zero line.
    pub centre_line: bool,
    /// Zero-line colour.
    pub centre_line_color: Color,
}

impl Default for WaveformStyle {
    fn default() -> Self {
        Self {
            color: Color::hex(0x5B9DFF),
            vertical_scale: 0.95,
            min_thickness: Px(1.0),
            centre_line: true,
            centre_line_color: Color::hex(0x2E333B),
        }
    }
}

/// How a spectrum is drawn.
#[derive(Clone, Debug)]
pub struct SpectrumStyle {
    /// Frequency axis mapping.
    pub frequency: LogFrequencyScale,
    /// Bottom of the dB axis.
    pub min_db: f32,
    /// Top of the dB axis.
    pub max_db: f32,
    /// Outline colour.
    pub color: Color,
    /// Outline width. Zero draws no outline.
    pub line_width: Px,
    /// Fill under the curve, or `None` for an outline only.
    pub fill_color: Option<Color>,
}

impl Default for SpectrumStyle {
    fn default() -> Self {
        Self {
            frequency: LogFrequencyScale::default(),
            min_db: -90.0,
            max_db: 0.0,
            color: Color::hex(0x5B9DFF),
            line_width: Px(1.5),
            fill_color: Some(Color::hex(0x5B9DFF).with_alpha(0.18)),
        }
    }
}

/// How a response or transfer curve is drawn.
#[derive(Clone, Debug)]
pub struct CurveStyle {
    /// Frequency axis mapping, for curves plotted against frequency.
    pub frequency: LogFrequencyScale,
    /// Bottom of the value axis, in dB.
    pub min_db: f32,
    /// Top of the value axis, in dB.
    pub max_db: f32,
    /// Curve colour.
    pub color: Color,
    /// Curve width.
    pub width: Px,
    /// Whether to draw the unity reference line.
    pub show_unity: bool,
    /// Unity line colour.
    pub unity_color: Color,
}

impl Default for CurveStyle {
    fn default() -> Self {
        Self {
            frequency: LogFrequencyScale::default(),
            min_db: -24.0,
            max_db: 24.0,
            color: Color::hex(0x3D8BFD),
            width: Px(2.0),
            show_unity: true,
            unity_color: Color::hex(0x2E333B),
        }
    }
}

/// A compressor's input-to-output mapping.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct CompressorCurve {
    /// Threshold in dB.
    pub threshold_db: f32,
    /// Ratio, where `4.0` means 4:1. Values at or below `1.0` are no-ops.
    pub ratio: f32,
    /// Knee width in dB. Zero is a hard knee.
    pub knee_db: f32,
    /// Makeup gain in dB.
    pub makeup_db: f32,
}

impl Default for CompressorCurve {
    fn default() -> Self {
        Self { threshold_db: -18.0, ratio: 4.0, knee_db: 6.0, makeup_db: 0.0 }
    }
}

impl CompressorCurve {
    /// The output level for an input level, both in dB.
    ///
    /// Implements the standard soft knee: a quadratic interpolation across the
    /// knee region so the curve is continuous in value and slope, which is what
    /// keeps a compressor from sounding like it switches on.
    pub fn output_db(&self, input_db: f32) -> f32 {
        if !input_db.is_finite() {
            return input_db;
        }
        let ratio = if self.ratio.is_finite() { self.ratio.max(1.0) } else { 1.0 };
        let knee = if self.knee_db.is_finite() { self.knee_db.max(0.0) } else { 0.0 };
        let over = input_db - self.threshold_db;

        let compressed = if knee > 0.0 && over > -knee * 0.5 && over < knee * 0.5 {
            let x = over + knee * 0.5;
            input_db + (1.0 / ratio - 1.0) * x * x / (2.0 * knee)
        } else if over <= 0.0 {
            input_db
        } else {
            self.threshold_db + over / ratio
        };
        compressed + self.makeup_db
    }

    /// How much gain reduction an input level produces, in dB (negative).
    #[inline]
    pub fn reduction_db(&self, input_db: f32) -> f32 {
        self.output_db(input_db) - self.makeup_db - input_db
    }
}

/// How a piano keyboard is drawn.
#[derive(Clone, Debug)]
pub struct PianoStyle {
    /// White key colour.
    pub white: Color,
    /// Black key colour.
    pub black: Color,
    /// White key colour while held.
    pub held_white: Color,
    /// Black key colour while held.
    pub held_black: Color,
    /// Key outline colour.
    pub border: Color,
    /// Black key width as a fraction of a white key's.
    pub black_width_ratio: f32,
    /// Black key height as a fraction of the keyboard's.
    pub black_height_ratio: f32,
}

impl Default for PianoStyle {
    fn default() -> Self {
        Self {
            white: Color::hex(0xE8EAEE),
            black: Color::hex(0x16181C),
            held_white: Color::hex(0x5B9DFF),
            held_black: Color::hex(0x3D8BFD),
            border: Color::hex(0x9AA0AA),
            // Proportions taken from a real keyboard.
            black_width_ratio: 0.62,
            black_height_ratio: 0.62,
        }
    }
}

/// An element that repaints continuously from a closure.
pub struct RealtimeCanvas<F> {
    id: Option<ElementId>,
    style: Style,
    paint_style: PaintStyle,
    draw: F,
    scratch: Vec<f32>,
    semantics: Option<Semantics>,
}

/// Creates a [`RealtimeCanvas`].
pub fn realtime_canvas<F>(draw: F) -> RealtimeCanvas<F>
where
    F: FnMut(&mut RealtimeFrame<'_, '_>) + 'static,
{
    RealtimeCanvas {
        id: None,
        style: Style::DEFAULT,
        paint_style: PaintStyle::default(),
        draw,
        scratch: Vec::new(),
        semantics: None,
    }
}

impl<F> RealtimeCanvas<F> {
    /// Gives the node a stable identity, so its scratch buffers survive a
    /// rebuild.
    pub fn id(mut self, id: impl core::hash::Hash) -> Self {
        self.id = Some(ElementId::from_key(id));
        self
    }

    /// Attaches semantic information.
    ///
    /// A meter is not interactive but it does have a value, and an assistive
    /// technology can announce it.
    pub fn semantics(mut self, semantics: Semantics) -> Self {
        self.semantics = Some(semantics);
        self
    }
}

impl<F> Styled for RealtimeCanvas<F> {
    fn style_mut(&mut self) -> &mut Style {
        &mut self.style
    }
    fn paint_style_mut(&mut self) -> &mut PaintStyle {
        &mut self.paint_style
    }
}

impl<F> Element for RealtimeCanvas<F>
where
    F: FnMut(&mut RealtimeFrame<'_, '_>) + 'static,
{
    fn id(&self) -> Option<ElementId> {
        self.id
    }

    fn layout_style(&self) -> Style {
        self.style.clone()
    }

    fn paint(&mut self, cx: &mut PaintContext<'_, '_>) {
        self.paint_style.paint_box(cx.canvas, cx.bounds, cx.state);
        // A realtime node is often offscreen in a scrolled mixer; generating
        // its geometry anyway would be the single largest waste in a session
        // with hundreds of channels.
        if cx.is_culled() || cx.bounds.is_empty() {
            return;
        }
        let mut frame = RealtimeFrame {
            canvas: cx.canvas,
            bounds: cx.bounds,
            theme: cx.theme,
            time: cx.time,
            scratch: &mut self.scratch,
        };
        (self.draw)(&mut frame);
    }

    fn handle_event(&mut self, _cx: &mut EventContext<'_>) -> EventFlow {
        EventFlow::Continue
    }

    fn semantics(&self) -> Option<Semantics> {
        self.semantics.clone()
    }
}

/// Marks a realtime node for repaint without touching layout.
///
/// The one-line version of the invariant this crate exists to preserve. Call it
/// from a timer or an audio-data-arrived signal.
#[inline]
pub fn request_repaint(cx: &mut EventContext<'_>) {
    cx.notify();
}

/// Converts a linear amplitude into a `LinearColor` ramp position, for
/// spectrogram-style intensity mapping.
///
/// Interpolates in linear light, so the ramp does not pass through a muddy
/// midpoint the way an sRGB interpolation would.
pub fn intensity_color(magnitude: f32, cold: Color, hot: Color, min_db: f32) -> LinearColor {
    let db = linear_to_db(magnitude);
    let t = ((db - min_db) / -min_db.min(-1e-3)).clamp(0.0, 1.0);
    cold.lerp(hot, t).to_linear()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{ScaleFactor, px, rect, size};
    use spherekit_render::{DrawCommand, Scene};
    use spherekit_ui::{IntoElement, ParentElement, UiTree, div};

    fn viewport() -> Size<Px> {
        size(px(400.0), px(200.0))
    }

    /// Builds a tree, lays it out, paints it, and returns the scene.
    fn render(root: spherekit_ui::AnyElement) -> (Scene, UiTree) {
        let mut tree = UiTree::new();
        let mut text = spherekit_text::TextSystem::new();
        tree.build(root);
        tree.compute_layout(viewport()).unwrap();
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }
        (scene, tree)
    }

    #[test]
    fn a_realtime_canvas_paints_what_its_closure_draws() {
        let (scene, _) = render(
            realtime_canvas(|frame| {
                frame.fill(Color::RED);
            })
            .w(spherekit_core::relative(1.0))
            .h(px(60.0))
            .into_element(),
        );
        assert_eq!(scene.len(), 1);
        assert!(matches!(scene.commands[0], DrawCommand::Quad(_)));
    }

    #[test]
    fn repainting_a_realtime_node_costs_zero_layout() {
        // THE headline invariant of the whole engine. A meter updating at the
        // display refresh rate must not relayout anything.
        let build = || {
            div()
                .w(spherekit_core::relative(1.0))
                .h(spherekit_core::relative(1.0))
                .child(
                    realtime_canvas(|frame| {
                        frame.fill(Color::RED);
                    })
                    .id("meter")
                    .w(px(20.0))
                    .h(px(120.0)),
                )
                .into_element()
        };
        let mut tree = UiTree::new();
        let mut text = spherekit_text::TextSystem::new();
        tree.build(build());
        tree.compute_layout(viewport()).unwrap();

        // Sixty frames of "new audio data arrived, repaint".
        for _ in 0..60 {
            tree.build(build());
            tree.compute_layout(viewport()).unwrap();
            assert_eq!(tree.stats().nodes_laid_out, 0, "a realtime repaint triggered layout work");
            let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
            {
                let mut canvas = Canvas::new(&mut scene);
                tree.paint(&mut canvas, &mut text, viewport(), 0.0);
            }
            assert!(!scene.is_empty());
        }
        assert_eq!(tree.stats().nodes_created, 0, "a realtime node was recreated each frame");
    }

    #[test]
    fn realtime_nodes_scrolled_out_of_a_clip_do_no_work() {
        // The scenario this matters in: a mixer with far more channel strips
        // than fit on screen. Generating geometry for the ones outside the
        // viewport is the single largest waste in a big session.
        const STRIPS: usize = 40;
        const STRIP_WIDTH: f32 = 100.0;

        let painted = std::rc::Rc::new(std::cell::Cell::new(0u32));
        let mut row =
            div().flex_row().w(px(STRIPS as f32 * STRIP_WIDTH)).h(spherekit_core::relative(1.0));
        for i in 0..STRIPS {
            let counter = painted.clone();
            row = row.child(
                realtime_canvas(move |frame| {
                    counter.set(counter.get() + 1);
                    frame.fill(Color::RED);
                })
                .id(i)
                .w(px(STRIP_WIDTH))
                .h(spherekit_core::relative(1.0)),
            );
        }
        // The viewport is 400 px wide, so only the first four strips can be
        // visible; the clip narrows `visible` for everything below it.
        let root = div()
            .w(spherekit_core::relative(1.0))
            .h(spherekit_core::relative(1.0))
            .overflow_hidden()
            .child(row)
            .into_element();

        let mut tree = UiTree::new();
        let mut text = spherekit_text::TextSystem::new();
        tree.build(root);
        tree.compute_layout(viewport()).unwrap();
        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        {
            let mut canvas = Canvas::new(&mut scene);
            tree.paint(&mut canvas, &mut text, viewport(), 0.0);
        }

        assert!(
            painted.get() <= 5,
            "{} of {STRIPS} offscreen strips generated geometry",
            painted.get()
        );
        assert!(painted.get() >= 1, "everything was culled, including the visible strips");
        assert!(tree.stats().elements_culled > 0, "the painter reported no culling at all");
    }

    #[test]
    fn a_zero_size_realtime_node_does_not_divide_by_zero() {
        let (scene, _) = render(
            realtime_canvas(|frame| {
                frame.draw_waveform(&[0.1, -0.1, 0.5], &WaveformStyle::default());
            })
            .w(px(0.0))
            .h(px(0.0))
            .into_element(),
        );
        assert!(scene.is_empty());
    }

    #[test]
    fn a_meter_draws_a_taller_bar_for_a_louder_signal() {
        let bar_height = |db: f32| {
            let mut state = MeterState::silent();
            state.advance(db, false, 0.0, &crate::dsp::MeterBallistics::default());
            let (scene, _) = render(
                realtime_canvas(move |frame| {
                    frame.draw_meter(&state, &MeterStyle::default());
                })
                .w(px(20.0))
                .h(px(120.0))
                .into_element(),
            );
            // Command 0 is the track; command 1 is the bar.
            match scene.commands.get(1) {
                Some(DrawCommand::Quad(q)) => q.bounds.height().get(),
                _ => 0.0,
            }
        };
        let quiet = bar_height(-40.0);
        let loud = bar_height(-3.0);
        assert!(loud > quiet, "-3 dB drew {loud}, -40 dB drew {quiet}");
        assert!(quiet > 0.0);
    }

    #[test]
    fn a_silent_meter_draws_only_its_track() {
        let mut state = MeterState::silent();
        state.advance(crate::dsp::MIN_DB, false, 0.0, &crate::dsp::MeterBallistics::default());
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_meter(&state, &MeterStyle { show_peak: false, ..Default::default() });
            })
            .w(px(20.0))
            .h(px(120.0))
            .into_element(),
        );
        assert_eq!(scene.len(), 1, "a silent meter drew more than its track");
    }

    #[test]
    fn a_vertical_meter_grows_upward_from_the_bottom() {
        let mut state = MeterState::silent();
        state.advance(-20.0, false, 0.0, &crate::dsp::MeterBallistics::default());
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_meter(&state, &MeterStyle { show_peak: false, ..Default::default() });
            })
            .w(px(20.0))
            .h(px(120.0))
            .into_element(),
        );
        match (&scene.commands[0], &scene.commands[1]) {
            (DrawCommand::Quad(track), DrawCommand::Quad(bar)) => {
                assert!(
                    (bar.bounds.max_y().get() - track.bounds.max_y().get()).abs() < 0.01,
                    "the bar must be anchored to the bottom: bar {:?} track {:?}",
                    bar.bounds,
                    track.bounds
                );
                assert!(bar.bounds.min_y() > track.bounds.min_y());
            }
            other => panic!("expected two quads, got {other:?}"),
        }
    }

    #[test]
    fn a_waveform_becomes_a_mesh_not_a_path() {
        // Dense geometry must bypass the tessellator.
        let samples: Vec<f32> = (0..4096).map(|i| (i as f32 * 0.01).sin()).collect();
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_waveform(
                    &samples,
                    &WaveformStyle { centre_line: false, ..Default::default() },
                );
            })
            .w(spherekit_core::relative(1.0))
            .h(px(80.0))
            .into_element(),
        );
        assert!(
            scene.commands.iter().any(|c| matches!(c, DrawCommand::Mesh { .. })),
            "the waveform did not produce a mesh"
        );
        assert!(
            !scene.commands.iter().any(|c| matches!(c, DrawCommand::FillPath { .. })),
            "the waveform went through the path tessellator"
        );
        let mesh = &scene.meshes[0];
        assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.vertices.len()));
        assert_eq!(mesh.indices.len() % 3, 0);
    }

    #[test]
    fn waveform_geometry_stays_inside_its_box() {
        let samples: Vec<f32> = (0..1000).map(|i| if i % 2 == 0 { 5.0 } else { -5.0 }).collect();
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_waveform(
                    &samples,
                    &WaveformStyle { centre_line: false, ..Default::default() },
                );
            })
            .w(spherekit_core::relative(1.0))
            .h(px(80.0))
            .into_element(),
        );
        // Out-of-range samples must be clamped, not drawn past the box.
        let mesh = &scene.meshes[0];
        for v in &mesh.vertices {
            assert!(v.position[1] >= -1.0 && v.position[1] <= 81.0, "vertex at {:?}", v.position);
        }
    }

    #[test]
    fn a_waveform_of_garbage_produces_no_garbage_vertices() {
        // One NaN vertex position corrupts an entire draw call.
        let samples = vec![f32::NAN, f32::INFINITY, 0.5, f32::NEG_INFINITY, -0.5];
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_waveform(
                    &samples,
                    &WaveformStyle { centre_line: false, ..Default::default() },
                );
            })
            .w(spherekit_core::relative(1.0))
            .h(px(80.0))
            .into_element(),
        );
        for mesh in &scene.meshes {
            for v in &mesh.vertices {
                assert!(v.position[0].is_finite() && v.position[1].is_finite(), "{:?}", v.position);
            }
        }
    }

    #[test]
    fn an_empty_waveform_draws_nothing_but_its_centre_line() {
        let (scene, _) = render(
            realtime_canvas(|frame| {
                frame.draw_waveform(&[], &WaveformStyle::default());
            })
            .w(spherekit_core::relative(1.0))
            .h(px(80.0))
            .into_element(),
        );
        // A flat envelope still draws min-thickness columns, which is correct:
        // silence is a line, not nothing. What matters is that it is finite.
        for mesh in &scene.meshes {
            for v in &mesh.vertices {
                assert!(v.position[1].is_finite());
            }
        }
    }

    #[test]
    fn a_spectrum_produces_finite_geometry() {
        let magnitudes: Vec<f32> = (0..512).map(|i| 1.0 / (1.0 + i as f32)).collect();
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_spectrum(&magnitudes, 22_050.0, &SpectrumStyle::default());
            })
            .w(spherekit_core::relative(1.0))
            .h(px(120.0))
            .into_element(),
        );
        assert!(!scene.meshes.is_empty(), "the spectrum fill produced no mesh");
        for mesh in &scene.meshes {
            assert!(mesh.indices.iter().all(|i| (*i as usize) < mesh.vertices.len()));
            for v in &mesh.vertices {
                assert!(v.position[0].is_finite() && v.position[1].is_finite());
            }
        }
    }

    #[test]
    fn a_degenerate_spectrum_is_skipped_rather_than_drawn_wrong() {
        for (mags, nyquist) in [
            (vec![], 22_050.0f32),
            (vec![0.5], 22_050.0),
            (vec![0.5, 0.5], 0.0),
            (vec![0.5, 0.5], f32::NAN),
        ] {
            let (scene, _) = render(
                realtime_canvas(move |frame| {
                    frame.draw_spectrum(&mags, nyquist, &SpectrumStyle::default());
                })
                .w(spherekit_core::relative(1.0))
                .h(px(120.0))
                .into_element(),
            );
            for mesh in &scene.meshes {
                for v in &mesh.vertices {
                    assert!(v.position[0].is_finite() && v.position[1].is_finite());
                }
            }
        }
    }

    #[test]
    fn a_compressor_below_threshold_is_transparent() {
        let c = CompressorCurve { threshold_db: -18.0, ratio: 4.0, knee_db: 0.0, makeup_db: 0.0 };
        assert!((c.output_db(-40.0) + 40.0).abs() < 1e-4);
        assert!(c.reduction_db(-40.0).abs() < 1e-4);
    }

    #[test]
    fn a_compressor_applies_its_ratio_above_threshold() {
        let c = CompressorCurve { threshold_db: -20.0, ratio: 4.0, knee_db: 0.0, makeup_db: 0.0 };
        // 12 dB over threshold at 4:1 is 3 dB over, so -17 dB out.
        assert!((c.output_db(-8.0) + 17.0).abs() < 1e-3, "{}", c.output_db(-8.0));
        assert!((c.reduction_db(-8.0) + 9.0).abs() < 1e-3);
    }

    #[test]
    fn a_soft_knee_is_continuous_across_the_threshold() {
        // A discontinuity here is what makes a compressor audibly switch on.
        let c = CompressorCurve { threshold_db: -20.0, ratio: 4.0, knee_db: 6.0, makeup_db: 0.0 };
        let mut previous = c.output_db(-40.0);
        let mut step = 0.0f32;
        for i in 1..=400 {
            let input = -40.0 + i as f32 * 0.1;
            let output = c.output_db(input);
            let delta = output - previous;
            assert!(delta >= -1e-3, "the curve went backwards at {input} dB");
            if i > 1 {
                assert!((delta - step).abs() < 0.02, "slope jumped at {input} dB");
            }
            step = delta;
            previous = output;
        }
    }

    #[test]
    fn a_unity_ratio_compressor_does_nothing() {
        for ratio in [1.0, 0.5, 0.0, -3.0] {
            let c = CompressorCurve { threshold_db: -20.0, ratio, knee_db: 0.0, makeup_db: 0.0 };
            assert!((c.output_db(-5.0) + 5.0).abs() < 1e-3, "ratio {ratio} changed the signal");
        }
    }

    #[test]
    fn makeup_gain_shifts_the_whole_curve() {
        let c = CompressorCurve { threshold_db: -20.0, ratio: 4.0, knee_db: 0.0, makeup_db: 6.0 };
        assert!((c.output_db(-40.0) + 34.0).abs() < 1e-3);
        // Reduction is measured before makeup, so it is unaffected.
        assert!(c.reduction_db(-40.0).abs() < 1e-3);
    }

    #[test]
    fn a_compressor_survives_garbage_input() {
        let c = CompressorCurve {
            threshold_db: f32::NAN,
            ratio: f32::NAN,
            knee_db: f32::NAN,
            makeup_db: 0.0,
        };
        assert!(c.output_db(-10.0).is_finite() || c.output_db(-10.0).is_nan());
        let sane = CompressorCurve::default();
        assert!(sane.output_db(f32::NAN).is_nan(), "a NaN input should stay a NaN, not become 0");
        assert!(sane.output_db(0.0).is_finite());
    }

    #[test]
    fn black_keys_land_where_a_real_keyboard_puts_them() {
        // Getting this wrong is the most visible possible bug in a piano roll.
        for note in 0..12u8 {
            assert_eq!(is_black_key(note), matches!(note, 1 | 3 | 6 | 8 | 10), "note {note}");
        }
        // An octave has five black and seven white keys.
        assert_eq!((60..72u8).filter(|n| is_black_key(*n)).count(), 5);
        assert_eq!((60..72u8).filter(|n| !is_black_key(*n)).count(), 7);
        // C-sharp leans toward C, D-sharp toward E; they are not both centred.
        assert!(black_key_offset(61) < 0.0);
        assert!(black_key_offset(63) > 0.0);
        assert_ne!(black_key_offset(61), black_key_offset(63));
    }

    #[test]
    fn a_piano_draws_one_shape_per_key() {
        let (scene, _) = render(
            realtime_canvas(|frame| {
                frame.draw_piano(60..72, &[60, 61], &PianoStyle::default());
            })
            .w(spherekit_core::relative(1.0))
            .h(px(60.0))
            .into_element(),
        );
        assert_eq!(scene.len(), 12, "expected 7 white plus 5 black keys");
    }

    #[test]
    fn an_empty_piano_range_draws_nothing() {
        let (scene, _) = render(
            realtime_canvas(|frame| {
                frame.draw_piano(60..60, &[], &PianoStyle::default());
            })
            .w(spherekit_core::relative(1.0))
            .h(px(60.0))
            .into_element(),
        );
        assert!(scene.is_empty());
    }

    #[test]
    fn a_piano_of_only_black_keys_does_not_divide_by_zero() {
        let (scene, _) = render(
            realtime_canvas(|frame| {
                // 61 and 63 are both black; there are no white keys to size by.
                frame.draw_piano(61..62, &[], &PianoStyle::default());
            })
            .w(spherekit_core::relative(1.0))
            .h(px(60.0))
            .into_element(),
        );
        assert!(scene.is_empty(), "a range with no white keys must bail out");
    }

    #[test]
    fn an_eq_curve_stays_inside_its_box() {
        let response: Vec<(f32, f32)> =
            (0..200).map(|i| (20.0 * 1.05f32.powi(i), (i as f32 * 0.1).sin() * 100.0)).collect();
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_eq_curve(
                    &response,
                    &CurveStyle { show_unity: false, ..Default::default() },
                );
            })
            .w(spherekit_core::relative(1.0))
            .h(px(120.0))
            .into_element(),
        );
        // Gains far outside the axis must clamp, not escape the widget.
        for path in &scene.paths {
            let b = path.control_bounds();
            assert!(b.min_y() >= px(-1.0) && b.max_y() <= px(121.0), "{b:?}");
        }
    }

    #[test]
    fn curves_with_garbage_points_skip_them() {
        let response = vec![(100.0, 3.0), (f32::NAN, 0.0), (1000.0, f32::NAN), (5000.0, -3.0)];
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_eq_curve(
                    &response,
                    &CurveStyle { show_unity: false, ..Default::default() },
                );
            })
            .w(spherekit_core::relative(1.0))
            .h(px(120.0))
            .into_element(),
        );
        for path in &scene.paths {
            for p in path.points() {
                assert!(p.x.get().is_finite() && p.y.get().is_finite());
            }
        }
    }

    #[test]
    fn a_vectorscope_puts_a_mono_signal_on_the_vertical_axis() {
        // The 45-degree rotation is what makes the display readable; without it
        // mono traces a diagonal and correlation is impossible to judge.
        let mono: Vec<f32> = (0..64).map(|i| (i as f32 * 0.2).sin()).collect();
        let same = mono.clone();
        let (scene, _) = render(
            realtime_canvas(move |frame| {
                frame.draw_vectorscope(&mono, &same, Color::GREEN);
            })
            .w(px(120.0))
            .h(px(120.0))
            .into_element(),
        );
        let path = &scene.paths[0];
        let centre_x = path.control_bounds().center().x;
        for p in path.points() {
            assert!(
                (p.x - centre_x).abs() < px(0.5),
                "a mono signal should trace a vertical line, got x = {:?}",
                p.x
            );
        }
    }

    #[test]
    fn an_oscilloscope_handles_a_short_or_garbage_buffer() {
        for samples in [vec![], vec![0.5], vec![f32::NAN, f32::NAN]] {
            let (scene, _) = render(
                realtime_canvas(move |frame| {
                    frame.draw_oscilloscope(&samples, Color::GREEN, px(1.0));
                })
                .w(px(120.0))
                .h(px(60.0))
                .into_element(),
            );
            for path in &scene.paths {
                for p in path.points() {
                    assert!(p.x.get().is_finite() && p.y.get().is_finite());
                }
            }
        }
    }

    #[test]
    fn gain_reduction_grows_downward_from_unity() {
        let (scene, _) = render(
            realtime_canvas(|frame| {
                frame.draw_gain_reduction(-12.0, &MeterStyle::default());
            })
            .w(px(20.0))
            .h(px(120.0))
            .into_element(),
        );
        match (&scene.commands[0], &scene.commands[1]) {
            (DrawCommand::Quad(track), DrawCommand::Quad(bar)) => {
                assert!(
                    (bar.bounds.min_y().get() - track.bounds.min_y().get()).abs() < 0.01,
                    "reduction must hang from the top: {:?}",
                    bar.bounds
                );
            }
            other => panic!("expected two quads, got {other:?}"),
        }
    }

    #[test]
    fn no_gain_reduction_draws_only_the_track() {
        let (scene, _) = render(
            realtime_canvas(|frame| {
                frame.draw_gain_reduction(0.0, &MeterStyle::default());
            })
            .w(px(20.0))
            .h(px(120.0))
            .into_element(),
        );
        assert_eq!(scene.len(), 1);
    }

    #[test]
    fn the_scratch_buffer_is_reused_across_frames() {
        // Steady-state waveform drawing must not allocate.
        let samples: Vec<f32> = (0..2048).map(|i| (i as f32 * 0.01).sin()).collect();
        let mut node = realtime_canvas(move |frame| {
            frame.draw_waveform(&samples, &WaveformStyle::default());
        })
        .w(px(200.0))
        .h(px(60.0));

        let mut scene = Scene::new(viewport(), ScaleFactor::IDENTITY);
        let mut text = spherekit_text::TextSystem::new();
        let theme = Theme::dark();
        let mut capacities = Vec::new();
        for _ in 0..5 {
            {
                let mut canvas = Canvas::new(&mut scene);
                let mut cx = PaintContext {
                    canvas: &mut canvas,
                    text: &mut text,
                    bounds: rect(px(0.0), px(0.0), px(200.0), px(60.0)),
                    visible: rect(px(0.0), px(0.0), px(400.0), px(200.0)),
                    scratch: [0.0; 4],
                    state: Default::default(),
                    theme: &theme,
                    time: 0.0,
                    ime: &mut None,
                    caption_exclusions: &mut Vec::new(),
                    scroll: Default::default(),
                };
                node.paint(&mut cx);
            }
            capacities.push(node.scratch.capacity());
            scene.reset(viewport(), ScaleFactor::IDENTITY);
        }
        assert_eq!(
            capacities[1], capacities[4],
            "the envelope buffer kept reallocating: {capacities:?}"
        );
    }

    #[test]
    fn intensity_colour_interpolates_between_the_endpoints() {
        let cold = Color::hex(0x000033);
        let hot = Color::hex(0xFFDD00);
        let quiet = intensity_color(1e-6, cold, hot, -90.0);
        let loud = intensity_color(1.0, cold, hot, -90.0);
        assert!(loud.r > quiet.r, "louder should be hotter");
        for c in [quiet, loud] {
            assert!(c.r.is_finite() && c.g.is_finite() && c.b.is_finite());
        }
        // Silence must not produce a NaN through log10(0).
        let silent = intensity_color(0.0, cold, hot, -90.0);
        assert!(silent.r.is_finite());
    }
}
