//! Realtime visualisers, declared from React and drawn by Rust.
//!
//! This is the part of the application worth reading twice.
//!
//! React cannot express a per-frame draw callback across the bridge — a
//! function is not JSON, and a visualiser that asked JavaScript for its pixels
//! sixty times a second would put the isolate on the paint path. So the split
//! is by responsibility rather than by convenience: React decides *that* there
//! is a spectrum here and a meter there, and what they look like via CSS;
//! Rust owns *what they draw*, straight from the audio ring, with no bridge
//! traffic at all.
//!
//! The seam is [`ReactHost::register_host_type`]. `<Native type="spectrum" />`
//! in a component becomes a `realtime_canvas` in the native tree, and from that
//! point on the audio path never crosses into JavaScript.

use std::cell::RefCell;
use std::rc::Rc;

use spherekit::audio::dsp::{MIN_DB, MeterBallistics, MeterState, linear_to_db};
use spherekit::audio::realtime::{MeterStyle, SpectrumStyle, WaveformStyle, realtime_canvas};
use spherekit::audio::transfer::ChannelLevel;
use spherekit::core::{Color, px};
use spherekit::css::Stylesheet;
use spherekit::react::{LowerContext, NativeNode, ReactHost, node_builder};
use spherekit::ui::IntoElement;

use crate::analysis::Analyzer;

/// Everything the visualisers read, refreshed once per frame by the shell.
///
/// One struct behind one `RefCell` rather than a cell per visualiser: they are
/// all read during a single paint walk on a single thread, and splitting them
/// would only create the possibility of two visualisers disagreeing about which
/// frame they are drawing.
#[derive(Default)]
pub struct VisualState {
    /// Smoothed magnitude spectrum, ascending to Nyquist.
    pub spectrum: Vec<f32>,
    /// Nyquist frequency for the spectrum's x axis.
    pub nyquist_hz: f32,
    /// Mono envelope of the most recent window, for the waveform.
    pub waveform: Vec<f32>,
    /// Left and right meter ballistics.
    pub left: MeterState,
    pub right: MeterState,
    /// True while audio is actually arriving.
    pub active: bool,
}

/// Shared handle to the visual state.
pub type SharedVisuals = Rc<RefCell<VisualState>>;

/// Advances the analysis from a window of interleaved samples.
///
/// Called once per frame from the shell, never from the audio thread.
pub struct VisualPipeline {
    analyzer: Analyzer,
    ballistics: MeterBallistics,
    visuals: SharedVisuals,
    /// Deinterleaved scratch, reused so a frame allocates nothing.
    ///
    /// `ChannelLevel::measure` wants a contiguous slice per channel and the
    /// ring hands over interleaved frames, so the split has to happen
    /// somewhere; here it costs one pass that the waveform needs anyway.
    left_channel: Vec<f32>,
    right_channel: Vec<f32>,
}

impl VisualPipeline {
    /// Creates the pipeline and the handle the visualisers will read.
    pub fn new() -> (Self, SharedVisuals) {
        let visuals = Rc::new(RefCell::new(VisualState {
            spectrum: vec![0.0; crate::analysis::BIN_COUNT],
            nyquist_hz: 22_050.0,
            waveform: Vec::new(),
            left: MeterState::silent(),
            right: MeterState::silent(),
            active: false,
        }));
        let pipeline = Self {
            analyzer: Analyzer::new(),
            ballistics: MeterBallistics::default(),
            visuals: Rc::clone(&visuals),
            left_channel: Vec::new(),
            right_channel: Vec::new(),
        };
        (pipeline, visuals)
    }

    /// Folds one frame of audio into the display state.
    ///
    /// `samples` is interleaved and may be empty, which is what a paused or
    /// finished track looks like from the ring.
    ///
    /// `gain` is the player's output volume. The tap sits before the fader, so
    /// everything here scales by it — otherwise the meters stay pinned while
    /// the volume slider is at a quarter, which is what they did.
    pub fn update(
        &mut self,
        samples: &[f32],
        channels: usize,
        sample_rate: f32,
        gain: f32,
        dt: f32,
    ) {
        let mut visuals = self.visuals.borrow_mut();
        visuals.active = !samples.is_empty();
        visuals.nyquist_hz = sample_rate * 0.5;

        if samples.is_empty() {
            // Decay rather than freeze. A held spectrum on a paused track reads
            // as a hung renderer.
            self.analyzer.decay();
            visuals.left.advance(MIN_DB, false, dt, &self.ballistics);
            visuals.right.advance(MIN_DB, false, dt, &self.ballistics);
        } else {
            self.analyzer.analyze(samples, channels, gain);

            let channels = channels.max(1);
            self.left_channel.clear();
            self.right_channel.clear();
            visuals.waveform.clear();
            for frame in samples.chunks_exact(channels) {
                let left = frame[0] * gain;
                // Mono sources feed both meters from the one channel rather
                // than leaving the right one dead.
                let right = if channels > 1 { frame[1] * gain } else { left };
                self.left_channel.push(left);
                self.right_channel.push(right);
                visuals.waveform.push(frame.iter().sum::<f32>() / channels as f32 * gain);
            }

            let left = ChannelLevel::measure(&self.left_channel);
            let right = ChannelLevel::measure(&self.right_channel);
            // RMS drives the bar, peak only the clip indicator.
            //
            // A peak-fed bar reads correctly and displays uselessly: a modern
            // master peaks within a decibel of full scale almost continuously,
            // so on a -60..0 dB scale the bar sits pinned at the top and red,
            // and it stops telling you anything about the music. RMS on the
            // same scale sits around -18..-8 dB and actually moves.
            visuals.left.advance(linear_to_db(left.rms), left.clipped, dt, &self.ballistics);
            visuals.right.advance(linear_to_db(right.rms), right.clipped, dt, &self.ballistics);
        }

        visuals.spectrum.clear();
        visuals.spectrum.extend_from_slice(self.analyzer.magnitudes());
    }
}

/// Registers `spectrum`, `waveform` and `level-meter` as React host types.
///
/// Each builder is handed the same `SharedVisuals`, so a component can place as
/// many of them as it likes and they all read the same frame.
pub fn register(host: &mut ReactHost, visuals: &SharedVisuals) {
    let spectrum = Rc::clone(visuals);
    host.register_host_type(
        "spectrum",
        node_builder(move |context: &LowerContext<'_>, node: &NativeNode| {
            let state = Rc::clone(&spectrum);
            let accent = prop_color(node, "color", Color::hex(0x6EE7B7));
            let element = realtime_canvas(move |frame| {
                let visuals = state.borrow();
                let style = SpectrumStyle {
                    color: accent,
                    line_width: px(1.5),
                    fill_color: Some(accent.with_alpha(0.22)),
                    min_db: -78.0,
                    max_db: 0.0,
                    ..SpectrumStyle::default()
                };
                frame.draw_spectrum(&visuals.spectrum, visuals.nyquist_hz, &style);
            })
            .id(node.id);
            context.apply(element).into_element()
        }),
    );

    let waveform = Rc::clone(visuals);
    host.register_host_type(
        "waveform",
        node_builder(move |context: &LowerContext<'_>, node: &NativeNode| {
            let state = Rc::clone(&waveform);
            let accent = prop_color(node, "color", Color::hex(0x93C5FD));
            let element = realtime_canvas(move |frame| {
                let visuals = state.borrow();
                let style = WaveformStyle { color: accent, ..WaveformStyle::default() };
                frame.draw_waveform(&visuals.waveform, &style);
            })
            .id(node.id);
            context.apply(element).into_element()
        }),
    );

    let meter = Rc::clone(visuals);
    host.register_host_type(
        "level-meter",
        node_builder(move |context: &LowerContext<'_>, node: &NativeNode| {
            let state = Rc::clone(&meter);
            // Which channel this meter shows. Two elements, one prop apart,
            // rather than one element that knows it is a stereo pair.
            let right = node.prop_str("channel").is_some_and(|value| value == "right");
            let element = realtime_canvas(move |frame| {
                let visuals = state.borrow();
                let style = MeterStyle { vertical: true, ..MeterStyle::default() };
                let channel = if right { &visuals.right } else { &visuals.left };
                frame.draw_meter(channel, &style);
            })
            .id(node.id);
            context.apply(element).into_element()
        }),
    );
}

/// Reads a `#rrggbb` prop, falling back when it is absent or malformed.
///
/// Goes through the CSS colour parser rather than a bespoke one so that a
/// colour written in a prop and the same colour written in the stylesheet
/// cannot disagree.
fn prop_color(node: &NativeNode, name: &str, fallback: Color) -> Color {
    let Some(value) = node.prop_str(name) else { return fallback };
    let source = format!(".x {{ color: {value}; }}");
    Stylesheet::parse(&source)
        .ok()
        .map(|sheet| sheet.resolve_node(spherekit::css::Node::new("x").with_classes("x")))
        .and_then(|resolved| resolved.text.color)
        .unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::FFT_SIZE;

    #[test]
    fn the_pipeline_decays_to_silence_when_no_audio_arrives() {
        // A paused track must not leave the meters pinned where the music
        // stopped, which is the single most obvious way for a visualiser to
        // look broken.
        let (mut pipeline, visuals) = VisualPipeline::new();
        let tone: Vec<f32> =
            (0..FFT_SIZE * 2).map(|i| (i as f32 * 0.05).sin()).flat_map(|s| [s, s]).collect();
        pipeline.update(&tone, 2, 44_100.0, 1.0, 1.0 / 60.0);
        let loud = visuals.borrow().left.level_db;

        for _ in 0..240 {
            pipeline.update(&[], 2, 44_100.0, 1.0, 1.0 / 60.0);
        }
        let quiet = visuals.borrow().left.level_db;
        assert!(quiet < loud - 20.0, "meter held at {quiet} dB from {loud} dB");
        assert!(!visuals.borrow().active);
    }

    /// A loud, near-full-scale signal, like any modern master.
    fn loud(seconds: f32) -> Vec<f32> {
        let frames = (44_100.0 * seconds) as usize;
        (0..frames).map(|i| 0.95 * (i as f32 * 0.07).sin()).flat_map(|s| [s, s]).collect()
    }

    #[test]
    fn a_loud_master_does_not_pin_the_meter_at_the_top() {
        // Feeding peak into a -60..0 dB scale is correct and useless: a modern
        // master peaks within a decibel of full scale almost continuously, so
        // the bar sits welded to the top in permanent clip red and stops saying
        // anything about the music. RMS on the same scale has somewhere to go.
        let (mut pipeline, visuals) = VisualPipeline::new();
        pipeline.update(&loud(0.1), 2, 44_100.0, 1.0, 1.0 / 60.0);

        let level = visuals.borrow().left.level_db;
        assert!(level < -2.0, "the meter is pinned at {level} dB on ordinary loud material");
        assert!(level > -30.0, "the meter is reading far too quiet at {level} dB");
    }

    #[test]
    fn the_meters_follow_the_volume_control() {
        // The tap is before the player's fader, so without applying the gain
        // here the meters stay wherever the source put them while the speakers
        // go quiet — which is exactly what turning the volume down looked like.
        let signal = loud(0.1);

        let (mut full, full_visuals) = VisualPipeline::new();
        full.update(&signal, 2, 44_100.0, 1.0, 1.0 / 60.0);
        let at_unity = full_visuals.borrow().left.level_db;

        let (mut quiet, quiet_visuals) = VisualPipeline::new();
        quiet.update(&signal, 2, 44_100.0, 0.25, 1.0 / 60.0);
        let at_quarter = quiet_visuals.borrow().left.level_db;

        // A quarter of the amplitude is about 12 dB down.
        assert!(
            (at_unity - at_quarter - 12.0).abs() < 2.0,
            "a quarter volume moved the meter from {at_unity} to {at_quarter} dB"
        );
    }

    #[test]
    fn the_spectrum_follows_the_volume_control_too() {
        // Otherwise the spectrum and the meters disagree about the same audio.
        let signal = loud(0.1);

        let (mut full, full_visuals) = VisualPipeline::new();
        full.update(&signal, 2, 44_100.0, 1.0, 1.0 / 60.0);
        let loud_peak = full_visuals.borrow().spectrum.iter().copied().fold(0.0f32, f32::max);

        let (mut quiet, quiet_visuals) = VisualPipeline::new();
        quiet.update(&signal, 2, 44_100.0, 0.25, 1.0 / 60.0);
        let quiet_peak = quiet_visuals.borrow().spectrum.iter().copied().fold(0.0f32, f32::max);

        assert!(quiet_peak < loud_peak * 0.5, "{quiet_peak} is not below {loud_peak}");
    }

    #[test]
    fn a_prop_colour_falls_back_rather_than_failing_the_frame() {
        let node = NativeNode {
            id: 1,
            node_type: "spectrum".into(),
            props: Default::default(),
            text: None,
            hidden: false,
            children: Vec::new(),
        };
        assert_eq!(prop_color(&node, "color", Color::RED), Color::RED);
    }
}
