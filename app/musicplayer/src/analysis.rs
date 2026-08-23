//! Spectrum analysis for the visualiser.
//!
//! A radix-2 FFT rather than a dependency: the player needs one transform of
//! one fixed size on one thread, and `rustfft`'s planner, SIMD kernels and
//! arbitrary-radix support all exist to solve problems this does not have.
//! Sixty lines that can be tested against a known tone is the smaller
//! liability.
//!
//! Everything here runs on the UI thread, on a window of samples drained from
//! the audio ring. Nothing in this module is reachable from the audio callback.

use std::f32::consts::PI;

/// Samples per analysis frame.
///
/// 2048 at 44.1 kHz is a ~21 Hz bin width and a ~46 ms window: fine enough to
/// separate the low end, short enough that the display still tracks a beat.
pub const FFT_SIZE: usize = 2048;

/// Magnitude bins produced per frame, `FFT_SIZE / 2 + 1` up to Nyquist.
pub const BIN_COUNT: usize = FFT_SIZE / 2 + 1;

/// Turns a window of interleaved samples into a smoothed magnitude spectrum.
pub struct Analyzer {
    /// Precomputed Hann window, so the multiply per frame is the only cost.
    window: Vec<f32>,
    real: Vec<f32>,
    imaginary: Vec<f32>,
    /// Bin magnitudes after the transform, before smoothing.
    raw: Vec<f32>,
    /// What the visualiser reads: `raw` with an asymmetric time constant.
    smoothed: Vec<f32>,
}

impl Default for Analyzer {
    fn default() -> Self {
        Self::new()
    }
}

impl Analyzer {
    /// Creates an analyser with its window and scratch buffers allocated.
    pub fn new() -> Self {
        let window = (0..FFT_SIZE)
            .map(|i| {
                // Hann. A rectangular window smears a pure tone across the
                // whole display through spectral leakage, which looks like
                // broadband noise and hides everything quieter than it.
                0.5 * (1.0 - (2.0 * PI * i as f32 / FFT_SIZE as f32).cos())
            })
            .collect();
        Self {
            window,
            real: vec![0.0; FFT_SIZE],
            imaginary: vec![0.0; FFT_SIZE],
            raw: vec![0.0; BIN_COUNT],
            smoothed: vec![0.0; BIN_COUNT],
        }
    }

    /// Analyses the most recent frame of interleaved audio.
    ///
    /// Fewer than [`FFT_SIZE`] frames of audio is not an error — the remainder
    /// is zero-padded, which is what happens at the very start of a track and
    /// for the last partial buffer before silence.
    pub fn analyze(&mut self, interleaved: &[f32], channels: usize) {
        let channels = channels.max(1);
        let frames = interleaved.len() / channels;
        let take = frames.min(FFT_SIZE);
        let skip = frames - take;

        self.real.fill(0.0);
        self.imaginary.fill(0.0);
        for index in 0..take {
            // Downmix to mono. A stereo spectrum would need two transforms and
            // two curves, and tells you nothing extra about a mix's balance
            // that the level meters do not already show.
            let frame = skip + index;
            let mut sum = 0.0;
            for channel in 0..channels {
                sum += interleaved[frame * channels + channel];
            }
            self.real[index] = (sum / channels as f32) * self.window[index];
        }

        fft(&mut self.real, &mut self.imaginary);

        // Normalised so a full-scale sine reads back as amplitude ~1.0 rather
        // than as a number that depends on FFT_SIZE. The window halves the
        // coherent gain, hence the 2.
        let scale = 2.0 / (FFT_SIZE as f32 * 0.5);
        for bin in 0..BIN_COUNT {
            let magnitude = (self.real[bin] * self.real[bin]
                + self.imaginary[bin] * self.imaginary[bin])
                .sqrt();
            self.raw[bin] = magnitude * scale;
        }

        for bin in 0..BIN_COUNT {
            // Asymmetric: rise almost immediately so a transient registers,
            // fall slowly so the eye can follow it. Symmetric smoothing makes
            // a spectrum either jittery or unreadably laggy, never both right.
            let target = self.raw[bin];
            let previous = self.smoothed[bin];
            self.smoothed[bin] =
                if target > previous { target } else { previous * 0.82 + target * 0.18 };
        }
    }

    /// The smoothed magnitudes, in ascending frequency order up to Nyquist.
    pub fn magnitudes(&self) -> &[f32] {
        &self.smoothed
    }

    /// Decays the display toward silence when no audio is arriving.
    ///
    /// Without this a paused track leaves its last spectrum frozen on screen,
    /// which reads as a stuck renderer rather than as stopped audio.
    pub fn decay(&mut self) {
        for value in &mut self.smoothed {
            *value *= 0.8;
        }
    }
}

/// In-place iterative radix-2 Cooley-Tukey FFT.
///
/// `real` and `imaginary` must be the same power-of-two length.
fn fft(real: &mut [f32], imaginary: &mut [f32]) {
    let n = real.len();
    debug_assert!(n.is_power_of_two());
    debug_assert_eq!(n, imaginary.len());

    // Bit-reversal permutation.
    let mut target = 0usize;
    for source in 1..n {
        let mut bit = n >> 1;
        while target & bit != 0 {
            target ^= bit;
            bit >>= 1;
        }
        target |= bit;
        if source < target {
            real.swap(source, target);
            imaginary.swap(source, target);
        }
    }

    let mut length = 2;
    while length <= n {
        let angle = -2.0 * PI / length as f32;
        let (step_sin, step_cos) = angle.sin_cos();
        for start in (0..n).step_by(length) {
            let (mut twiddle_re, mut twiddle_im) = (1.0f32, 0.0f32);
            for offset in 0..length / 2 {
                let a = start + offset;
                let b = a + length / 2;
                let product_re = real[b] * twiddle_re - imaginary[b] * twiddle_im;
                let product_im = real[b] * twiddle_im + imaginary[b] * twiddle_re;
                real[b] = real[a] - product_re;
                imaginary[b] = imaginary[a] - product_im;
                real[a] += product_re;
                imaginary[a] += product_im;

                let next_re = twiddle_re * step_cos - twiddle_im * step_sin;
                twiddle_im = twiddle_re * step_sin + twiddle_im * step_cos;
                twiddle_re = next_re;
            }
        }
        length <<= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generates `FFT_SIZE` frames of a mono sine at `hz`.
    fn tone(hz: f32, sample_rate: f32, amplitude: f32) -> Vec<f32> {
        (0..FFT_SIZE).map(|i| amplitude * (2.0 * PI * hz * i as f32 / sample_rate).sin()).collect()
    }

    fn peak_bin(magnitudes: &[f32]) -> usize {
        magnitudes
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(bin, _)| bin)
            .unwrap()
    }

    #[test]
    fn a_pure_tone_peaks_in_the_bin_that_contains_it() {
        // The property the whole visualiser rests on. If this is off by a bin
        // scaling factor the display is plausible and wrong, which is the worst
        // kind of wrong for something judged by eye.
        let sample_rate = 44_100.0;
        let mut analyzer = Analyzer::new();
        for hz in [110.0, 440.0, 1000.0, 5000.0] {
            analyzer.analyze(&tone(hz, sample_rate, 0.5), 1);
            let expected = (hz / (sample_rate / FFT_SIZE as f32)).round() as usize;
            let found = peak_bin(analyzer.magnitudes());
            assert!(
                found.abs_diff(expected) <= 1,
                "{hz} Hz peaked in bin {found}, expected {expected}"
            );
        }
    }

    #[test]
    fn a_full_scale_tone_reads_back_near_unit_amplitude() {
        // Fixes the normalisation to something meaningful rather than to
        // whatever happened to look right at one window size: the dB axis in
        // the visualiser is only honest if 0 dBFS in means 0 dBFS on screen.
        //
        // The tone sits exactly on a bin centre. A frequency that falls between
        // two bins splits its energy across them — scalloping loss, up to
        // -1.4 dB with a Hann window — so testing an arbitrary frequency here
        // would be measuring the window's shape, not the scale factor.
        let sample_rate = 44_100.0;
        let bin_centred = 46.0 * sample_rate / FFT_SIZE as f32;
        let mut analyzer = Analyzer::new();
        analyzer.analyze(&tone(bin_centred, sample_rate, 1.0), 1);
        let peak = analyzer.magnitudes()[peak_bin(analyzer.magnitudes())];
        assert!((peak - 1.0).abs() < 0.05, "full-scale tone read back as {peak}");
    }

    #[test]
    fn an_off_centre_tone_loses_no_more_than_the_window_allows() {
        // The other half of the scaling contract: a tone landing between bins
        // must still read close to full scale. Much below this and the display
        // would under-read most real content, since almost nothing in music
        // lands on a bin centre.
        let sample_rate = 44_100.0;
        let bin_width = sample_rate / FFT_SIZE as f32;
        let worst_case = (46.5) * bin_width;
        let mut analyzer = Analyzer::new();
        analyzer.analyze(&tone(worst_case, sample_rate, 1.0), 1);
        let peak = analyzer.magnitudes()[peak_bin(analyzer.magnitudes())];
        assert!(peak > 0.8, "a tone between bins read back as {peak}, below Hann's -1.4 dB");
    }

    #[test]
    fn silence_produces_no_spectrum() {
        let mut analyzer = Analyzer::new();
        analyzer.analyze(&vec![0.0; FFT_SIZE], 1);
        assert!(analyzer.magnitudes().iter().all(|m| *m < 1e-6));
    }

    #[test]
    fn stereo_input_is_downmixed_rather_than_read_as_twice_the_frequency() {
        // Reading interleaved stereo as if it were mono doubles the apparent
        // frequency of everything. Both channels carry the same tone here, so
        // a correct downmix lands on the same bin the mono case does.
        let sample_rate = 44_100.0;
        let mono = tone(440.0, sample_rate, 0.5);
        let stereo: Vec<f32> = mono.iter().flat_map(|s| [*s, *s]).collect();

        let mut from_mono = Analyzer::new();
        from_mono.analyze(&mono, 1);
        let mut from_stereo = Analyzer::new();
        from_stereo.analyze(&stereo, 2);

        assert_eq!(peak_bin(from_mono.magnitudes()), peak_bin(from_stereo.magnitudes()));
    }

    #[test]
    fn a_short_buffer_is_zero_padded_instead_of_panicking() {
        // The first frames of a track, and the last before silence, are always
        // short. This used to be the obvious place for an index panic.
        let mut analyzer = Analyzer::new();
        analyzer.analyze(&[0.1, -0.1, 0.2, -0.2], 2);
        assert_eq!(analyzer.magnitudes().len(), BIN_COUNT);
    }

    #[test]
    fn the_display_decays_toward_silence_when_audio_stops() {
        let mut analyzer = Analyzer::new();
        analyzer.analyze(&tone(1000.0, 44_100.0, 1.0), 1);
        let before = analyzer.magnitudes()[peak_bin(analyzer.magnitudes())];
        for _ in 0..60 {
            analyzer.decay();
        }
        let after = analyzer.magnitudes()[peak_bin(analyzer.magnitudes())];
        assert!(after < before * 0.01, "spectrum froze at {after} from {before}");
    }
}
