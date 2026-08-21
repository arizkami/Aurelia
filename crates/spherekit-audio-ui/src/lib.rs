//! # spherekit-audio-ui
//!
//! Realtime visualisation primitives for audio software, and the lock-free
//! boundary that makes them safe.
//!
//! ## The rule this crate exists to enforce
//!
//! **Realtime audio data never renders from the audio thread.** The audio
//! callback has a hard deadline; missing it is an audible click. So it may not
//! allocate, take a contended lock, touch the GPU, shape text, or block. What
//! it may do is write a value into one of the transfer primitives in
//! [`transfer`] and return.
//!
//! ```text
//! Audio thread                UI / render thread
//! ------------                ------------------
//! measure block
//!      |
//!      v
//! AtomicSnapshot  --------->  read newest level
//! SpscRing        --------->  drain sample stream
//! Seqlock         --------->  copy spectrum frame
//!                                  |
//!                                  v
//!                             RealtimePaintNode marked PAINT-dirty
//!                                  |
//!                                  v
//!                             mesh generated, GPU instance updated, drawn
//! ```
//!
//! What must never happen:
//!
//! ```text
//! DSP callback -> mutex -> UI tree update -> GPU call
//! ```
//!
//! ## Why these are not ordinary widgets
//!
//! A VU meter updates sixty times a second forever. Routing that through the
//! normal element path would rebuild and relayout the tree at the display
//! refresh rate for content that never changes size. A
//! [`RealtimeCanvas`](realtime::RealtimeCanvas) instead marks itself
//! **PAINT**-dirty only: nothing relayouts, no text reshapes, and no unrelated
//! widget is touched. That property is asserted in this crate's tests, not just
//! claimed here.
//!
//! Dense primitives — waveforms, spectrums — also bypass the path tessellator
//! and build [`spherekit_render::Mesh`] geometry directly, because a waveform is
//! already a list of vertices and turning it into a path first would be pure
//! overhead.

#![deny(missing_docs)]

pub mod dsp;
pub mod realtime;
pub mod transfer;

pub use dsp::{
    LogFrequencyScale, MIN_DB, MeterBallistics, MeterScale, MeterState, db_to_linear, linear_to_db,
    min_max_envelope,
};
pub use realtime::{
    CompressorCurve, CurveStyle, MeterStyle, PianoStyle, RealtimeCanvas, RealtimeFrame,
    SpectrumStyle, WaveformStyle, black_key_offset, intensity_color, is_black_key, realtime_canvas,
    request_repaint,
};
pub use transfer::{
    ChannelLevel, LevelConsumer, LevelProducer, RingConsumer, RingProducer, Seqlock,
    SnapshotConsumer, SnapshotProducer, StereoLevel, level_channel, ring, snapshot,
};

/// Everything a typical consumer needs, in one import.
pub mod prelude {
    pub use crate::dsp::{
        LogFrequencyScale, MeterBallistics, MeterScale, MeterState, db_to_linear, linear_to_db,
    };
    pub use crate::realtime::{
        CurveStyle, MeterStyle, RealtimeFrame, SpectrumStyle, WaveformStyle, realtime_canvas,
    };
    pub use crate::transfer::{ChannelLevel, StereoLevel, level_channel, ring, snapshot};
}
