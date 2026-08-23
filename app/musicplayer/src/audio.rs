//! Playback, and the one-way road from the audio thread to the UI.
//!
//! ## The boundary
//!
//! Rodio decodes and mixes on its own thread. That thread must never block, so
//! it never touches a lock, never allocates, and never reads anything the UI
//! writes. It does exactly one thing for the display: pushes the samples it has
//! already produced into a lock-free ring.
//!
//! The UI drains that ring once per frame and does all the expensive work —
//! windowing, the FFT, meter ballistics — on its own thread, where being late
//! costs a dropped frame rather than an audible glitch. `spherekit-audio-ui`
//! exists for exactly this split; see `docs/audio-ui.md`.
//!
//! ## What a dropped sample means
//!
//! The ring is allowed to overflow. If the UI stalls, the producer drops the
//! samples it cannot fit and carries on, because the alternative — blocking the
//! audio thread until the UI catches up — is a dropout the listener can hear.
//! A visualiser that misses a few milliseconds during a stall is invisible.

use std::fs::File;
use std::num::NonZero;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rodio::source::SeekError;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use spherekit::audio::transfer::{RingConsumer, RingProducer, ring};

/// Samples the ring holds: about a third of a second of stereo at 48 kHz.
///
/// Large enough that a slow frame loses nothing, small enough that the UI is
/// never drawing audio the listener heard a noticeable time ago.
const RING_CAPACITY: usize = 32_768;

/// Errors the player reports to the application.
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    /// No usable output device, or the platform refused to open one.
    #[error("no audio output device is available: {0}")]
    NoDevice(String),
    /// The file could not be opened.
    #[error("cannot open {path}: {source}")]
    Open {
        /// The file that could not be read.
        path: String,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
    /// The file opened but held nothing this build can decode.
    #[error("cannot decode {path}: {message}")]
    Decode {
        /// The file that could not be decoded.
        path: String,
        /// What the decoder said.
        message: String,
    },
    /// Seeking is not supported for the current source, or failed.
    #[error("cannot seek: {0}")]
    Seek(String),
}

/// A source that passes audio through untouched and copies it to the UI.
///
/// Deliberately does no work beyond a push: this runs on the mixing thread.
struct Tap<S> {
    inner: S,
    ring: RingProducer<f32>,
    /// Written here so the UI knows how to interpret the interleaving without
    /// having to ask the player, which would mean a lock.
    channels: Arc<AtomicU32>,
    /// Reused so the pass-through never allocates.
    scratch: Vec<f32>,
}

impl<S: Source> Iterator for Tap<S> {
    type Item = f32;

    fn next(&mut self) -> Option<f32> {
        let sample = self.inner.next()?;
        self.scratch.push(sample);
        // Batched rather than one push per sample: `push_slice` is one
        // acquire/release pair instead of hundreds, and this is the hottest
        // path in the program.
        if self.scratch.len() >= 256 {
            self.ring.push_slice(&self.scratch);
            self.scratch.clear();
        }
        Some(sample)
    }
}

impl<S: Source> Source for Tap<S> {
    fn current_span_len(&self) -> Option<usize> {
        self.inner.current_span_len()
    }

    fn channels(&self) -> NonZero<u16> {
        let channels = self.inner.channels();
        self.channels.store(u32::from(channels.get()), Ordering::Relaxed);
        channels
    }

    fn sample_rate(&self) -> NonZero<u32> {
        self.inner.sample_rate()
    }

    fn total_duration(&self) -> Option<Duration> {
        self.inner.total_duration()
    }

    fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
        self.inner.try_seek(position)
    }
}

/// Hands a new track's ring consumer to the UI.
///
/// Both ends of this are the UI thread — `load` puts one in, `drain` takes it
/// out — so the lock is uncontended and, more importantly, is never reachable
/// from the audio callback. The audio thread only ever holds a producer.
type ConsumerHandoff = Arc<Mutex<Option<RingConsumer<f32>>>>;

/// What the UI thread reads to draw the visualiser.
pub struct AudioTap {
    /// Where `AudioEngine::load` leaves the consumer for the next track.
    incoming: ConsumerHandoff,
    /// The track being drained now, absent until the first `load`.
    current: Option<RingConsumer<f32>>,
    channels: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    /// Reused across frames so drawing never allocates.
    buffer: Vec<f32>,
}

impl AudioTap {
    /// Drains the most recent `sample_limit` samples, discarding any backlog.
    ///
    /// Returns an empty slice when nothing has been produced since the last
    /// call, which is what silence, a pause and a stall all look like from
    /// here — the caller decides what that means.
    pub fn drain(&mut self, sample_limit: usize) -> &[f32] {
        if let Ok(mut slot) = self.incoming.lock()
            && let Some(next) = slot.take()
        {
            // The previous track's consumer is dropped with whatever it still
            // held. Carrying it over would splice the tail of the old track
            // onto the head of the new one in the display.
            self.current = Some(next);
        }

        self.buffer.clear();
        if let Some(ring) = self.current.as_mut() {
            ring.drain_keeping_last(sample_limit, &mut self.buffer);
        }
        &self.buffer
    }

    /// Channels in the samples [`AudioTap::drain`] returns.
    pub fn channels(&self) -> usize {
        self.channels.load(Ordering::Relaxed).max(1) as usize
    }

    /// Sample rate of the track currently playing.
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate.load(Ordering::Relaxed).max(1) as f32
    }
}

/// Owns the output device and the currently playing track.
pub struct AudioEngine {
    /// Held for its lifetime: dropping it closes the output stream, and every
    /// player connected to its mixer goes silent.
    _device: MixerDeviceSink,
    player: Player,
    channels: Arc<AtomicU32>,
    sample_rate: Arc<AtomicU32>,
    /// Where a freshly created consumer is left for [`AudioTap`] to pick up.
    handoff: ConsumerHandoff,
    /// Length of the loaded track, when the decoder could tell us.
    duration: Option<Duration>,
    volume: f32,
}

impl AudioEngine {
    /// Opens the default output device.
    pub fn new() -> Result<(Self, AudioTap), AudioError> {
        let device = DeviceSinkBuilder::open_default_sink()
            .map_err(|error| AudioError::NoDevice(error.to_string()))?;
        let player = Player::connect_new(device.mixer());

        let channels = Arc::new(AtomicU32::new(2));
        let sample_rate = Arc::new(AtomicU32::new(44_100));
        let handoff: ConsumerHandoff = Arc::new(Mutex::new(None));

        let engine = Self {
            _device: device,
            player,
            channels: Arc::clone(&channels),
            sample_rate: Arc::clone(&sample_rate),
            handoff: Arc::clone(&handoff),
            duration: None,
            volume: 0.8,
        };
        engine.player.set_volume(engine.volume);

        let tap = AudioTap {
            incoming: handoff,
            current: None,
            channels,
            sample_rate,
            buffer: Vec::with_capacity(RING_CAPACITY),
        };
        Ok((engine, tap))
    }

    /// Decodes `path` and starts playing it, replacing whatever was playing.
    pub fn load(&mut self, path: &Path) -> Result<Option<Duration>, AudioError> {
        let display = path.display().to_string();
        let file = File::open(path)
            .map_err(|source| AudioError::Open { path: display.clone(), source })?;
        let decoded = Decoder::try_from(file)
            .map_err(|error| AudioError::Decode { path: display, message: error.to_string() })?;

        self.duration = decoded.total_duration();
        self.sample_rate.store(decoded.sample_rate().get(), Ordering::Relaxed);
        self.channels.store(u32::from(decoded.channels().get()), Ordering::Relaxed);

        // The previous track's tail is still queued; drop it before the new one
        // is appended or they play in sequence rather than one replacing the
        // other.
        self.player.clear();

        // A fresh ring per track. The producer is moved into the source and is
        // gone for good; the matching consumer is left for the UI to pick up on
        // its next frame. Reusing one ring would mean recovering the producer
        // out of a source rodio owns, which there is no sound way to do.
        let (producer, consumer) = ring::<f32>(RING_CAPACITY);
        if let Ok(mut slot) = self.handoff.lock() {
            *slot = Some(consumer);
        }

        self.player.append(Tap {
            inner: decoded,
            ring: producer,
            channels: Arc::clone(&self.channels),
            scratch: Vec::with_capacity(256),
        });
        self.player.play();
        Ok(self.duration)
    }

    /// Resumes playback.
    pub fn play(&self) {
        self.player.play();
    }

    /// Pauses without discarding the queued track.
    pub fn pause(&self) {
        self.player.pause();
    }

    /// True while paused.
    pub fn is_paused(&self) -> bool {
        self.player.is_paused()
    }

    /// True once the queue has run dry, which is how a finished track is
    /// noticed without the decoder telling anyone.
    pub fn is_finished(&self) -> bool {
        self.player.empty()
    }

    /// Stops and discards the queue.
    pub fn stop(&mut self) {
        self.player.clear();
        self.duration = None;
    }

    /// Playback position within the current track.
    pub fn position(&self) -> Duration {
        self.player.get_pos()
    }

    /// Length of the current track, when the decoder reported one.
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// Jumps to `position`.
    pub fn seek(&self, position: Duration) -> Result<(), AudioError> {
        self.player.try_seek(position).map_err(|error| AudioError::Seek(error.to_string()))
    }

    /// Output gain, `0.0..=1.0`.
    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// Sets the output gain.
    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        self.player.set_volume(self.volume);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_which_file_and_why() {
        // A player that cannot open a file has to name it: a library scan hands
        // it dozens, and "decode failed" on its own is unactionable.
        let error = AudioError::Open {
            path: "C:/music/track.mp3".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
        };
        let text = error.to_string();
        assert!(text.contains("track.mp3"), "{text}");
        assert!(text.contains("no such file"), "{text}");
    }

    #[test]
    fn a_missing_device_is_an_error_and_not_a_panic() {
        // CI runners have no sound card. The application has to survive that
        // and still show its UI, so this path must stay a `Result`.
        let error = AudioError::NoDevice("no output device".into());
        assert!(error.to_string().contains("no audio output device"));
    }
}
