//! The audio-thread boundary.
//!
//! This module is the reason the crate exists. Everything else here draws
//! pictures; this decides whether the audio callback stays real-time safe while
//! it does.
//!
//! ## What is forbidden on the audio callback
//!
//! The callback runs on a thread with a hard deadline — miss it and the user
//! hears a click. So on that thread there must be **no** allocation, **no**
//! contended lock, **no** GPU work, **no** text shaping, **no** filesystem
//! access, and **no** unbounded loop. A `Mutex` is the usual mistake: it is
//! fast until the frame it is not, and the one time the UI thread holds it
//! during a page fault is the time the user hears the glitch.
//!
//! ## The three mechanisms
//!
//! | Type | Shape of data | Use it for |
//! |---|---|---|
//! | [`AtomicSnapshot`] | one small `T`, latest wins | Meter levels, gain reduction, a transport position |
//! | [`SpscRing`] | a stream, none may be dropped | Waveform samples, FFT frames, MIDI events |
//! | [`Seqlock`] | one large `T`, latest wins | A whole spectrum frame, an impulse response |
//!
//! The distinction that matters: a **meter** only ever wants the newest value,
//! so dropping intermediate ones is correct and a snapshot is the right shape.
//! A **scrolling waveform** must not skip samples or it draws a lie, so it
//! needs a queue.
//!
//! ## Why `unsafe` appears here
//!
//! A lock-free buffer hands out `&T` to memory another thread may be writing;
//! that is exactly what `UnsafeCell` exists for and it cannot be expressed
//! safely. Every block has a `// SAFETY:` comment naming the invariant that
//! makes it sound. The invariants are enforced by the type: producer and
//! consumer are separate, non-`Clone` handles, so "single producer, single
//! consumer" is a compile-time property rather than a documentation request.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU8, AtomicU32, AtomicUsize, Ordering, fence};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Triple-buffered snapshot
// ---------------------------------------------------------------------------

/// Index bits of the control word.
const IDX_MASK: u8 = 0b0000_0011;
/// Set by the producer to mean "there is a newer buffer than the consumer has".
const DIRTY_BIT: u8 = 0b0000_0100;

struct SnapshotInner<T> {
    /// Three slots: one being written, one published, one being read.
    ///
    /// Three is the minimum that lets both sides make progress without ever
    /// waiting. With two, a producer that wants to publish while the consumer
    /// is mid-read has nowhere to go.
    slots: [UnsafeCell<T>; 3],
    /// `index | DIRTY_BIT`, the handoff slot.
    control: AtomicU8,
    /// Published values, for diagnostics.
    writes: AtomicUsize,
}

// SAFETY: `SnapshotInner` is only ever reached through a `SnapshotProducer` and
// a `SnapshotConsumer`, which are created as a pair, are not `Clone`, and each
// own a disjoint slot index at all times (see the swap protocol below). No two
// threads therefore access the same slot, and the control word is atomic.
unsafe impl<T: Send> Send for SnapshotInner<T> {}
// SAFETY: as above — sharing the `Arc` is what the producer/consumer split is
// for, and the only mutable access is through the disjoint owned indices.
unsafe impl<T: Send> Sync for SnapshotInner<T> {}

/// The writing half of a triple-buffered snapshot. Lives on the audio thread.
pub struct SnapshotProducer<T> {
    inner: Arc<SnapshotInner<T>>,
    /// The slot this side owns and is free to write.
    index: u8,
}

/// The reading half of a triple-buffered snapshot. Lives on the UI thread.
pub struct SnapshotConsumer<T> {
    inner: Arc<SnapshotInner<T>>,
    /// The slot this side owns and is free to read.
    index: u8,
    /// Fetches that found new data, for diagnostics.
    reads: usize,
}

/// Creates a triple-buffered snapshot channel.
///
/// `initial` fills all three slots, so the consumer has something coherent to
/// read before the producer has ever run — a meter must draw *something* on the
/// first frame, not garbage.
pub fn snapshot<T: Clone + Send>(initial: T) -> (SnapshotProducer<T>, SnapshotConsumer<T>) {
    let inner = Arc::new(SnapshotInner {
        slots: [
            UnsafeCell::new(initial.clone()),
            UnsafeCell::new(initial.clone()),
            UnsafeCell::new(initial),
        ],
        // Producer owns 0, consumer owns 1, slot 2 is the handoff. Not dirty:
        // nothing has been published yet.
        control: AtomicU8::new(2),
        writes: AtomicUsize::new(0),
    });
    (
        SnapshotProducer { inner: Arc::clone(&inner), index: 0 },
        SnapshotConsumer { inner, index: 1, reads: 0 },
    )
}

impl<T> SnapshotProducer<T> {
    /// Writes a value and publishes it.
    ///
    /// Wait-free: one closure call and one atomic swap, no allocation, no
    /// branch that can spin. Safe to call from an audio callback.
    ///
    /// The closure receives the slot the producer already owns, so a large `T`
    /// can be updated in place rather than constructed and moved.
    pub fn publish(&mut self, fill: impl FnOnce(&mut T)) {
        // SAFETY: `self.index` names the slot this producer exclusively owns.
        // The consumer can only obtain it by swapping the control word, which
        // happens strictly after the swap below hands it over.
        let slot = unsafe { &mut *self.inner.slots[self.index as usize].get() };
        fill(slot);

        // Release: everything written to the slot above must be visible to a
        // consumer that acquires this control word. Without Release the
        // consumer can observe the new index while still seeing stale payload.
        let previous = self.inner.control.swap(self.index | DIRTY_BIT, Ordering::Release);
        self.index = previous & IDX_MASK;
        self.inner.writes.fetch_add(1, Ordering::Relaxed);
    }

    /// Replaces the published value.
    pub fn set(&mut self, value: T) {
        self.publish(|slot| *slot = value);
    }

    /// How many values have been published.
    #[inline]
    pub fn published(&self) -> usize {
        self.inner.writes.load(Ordering::Relaxed)
    }
}

impl<T> SnapshotConsumer<T> {
    /// Fetches the newest published value if there is one, and returns it.
    ///
    /// Never blocks and never allocates. When the producer has published
    /// nothing since the last call, the previous value is returned again —
    /// which is the correct behaviour for a meter between audio buffers.
    pub fn read(&mut self) -> &T {
        // Relaxed is enough for the test: if it is stale we simply keep the
        // value we have and try again next frame. The swap below carries the
        // synchronisation.
        if self.inner.control.load(Ordering::Relaxed) & DIRTY_BIT != 0 {
            // AcqRel: Acquire so the payload the producer wrote before its
            // Release swap is visible; Release so our own index handover is
            // visible to the producer.
            let previous = self.inner.control.swap(self.index, Ordering::AcqRel);
            self.index = previous & IDX_MASK;
            self.reads += 1;
        }
        // SAFETY: `self.index` names the slot this consumer exclusively owns.
        // The producer can only take it back via the swap above, which has
        // already completed.
        unsafe { &*self.inner.slots[self.index as usize].get() }
    }

    /// True when the producer has published since the last [`SnapshotConsumer::read`].
    #[inline]
    pub fn has_new_data(&self) -> bool {
        self.inner.control.load(Ordering::Relaxed) & DIRTY_BIT != 0
    }

    /// The current value without checking for a newer one.
    #[inline]
    pub fn peek(&self) -> &T {
        // SAFETY: as in `read` — the consumer owns this slot.
        unsafe { &*self.inner.slots[self.index as usize].get() }
    }

    /// How many times a newer value was picked up.
    #[inline]
    pub fn fetched(&self) -> usize {
        self.reads
    }
}

// ---------------------------------------------------------------------------
// SPSC ring buffer
// ---------------------------------------------------------------------------

struct RingInner<T> {
    slots: Box<[UnsafeCell<Option<T>>]>,
    /// Next index the producer will write. Only the producer stores to it.
    head: AtomicUsize,
    /// Next index the consumer will read. Only the consumer stores to it.
    tail: AtomicUsize,
    /// `capacity - 1`; capacity is a power of two so wrapping is a mask.
    mask: usize,
    /// Pushes rejected because the ring was full.
    dropped: AtomicUsize,
}

// SAFETY: producer and consumer are separate non-`Clone` handles. The producer
// only ever writes the slot at `head` and only stores to `head`; the consumer
// only ever reads the slot at `tail` and only stores to `tail`. The emptiness
// and fullness checks below guarantee those slots are never the same one.
unsafe impl<T: Send> Send for RingInner<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for RingInner<T> {}

/// The writing half of a single-producer/single-consumer ring.
pub struct RingProducer<T> {
    inner: Arc<RingInner<T>>,
}

/// The reading half of a single-producer/single-consumer ring.
pub struct RingConsumer<T> {
    inner: Arc<RingInner<T>>,
}

/// Creates a bounded lock-free ring.
///
/// `capacity` is rounded up to a power of two, so the wrap is a mask rather
/// than a modulo — a division on the audio thread is avoidable and therefore
/// should be avoided. One slot is reserved to distinguish full from empty, so
/// the usable capacity is one less than the allocation.
pub fn ring<T: Send>(capacity: usize) -> (RingProducer<T>, RingConsumer<T>) {
    let capacity = capacity.max(2).next_power_of_two();
    let mut slots = Vec::with_capacity(capacity);
    slots.resize_with(capacity, || UnsafeCell::new(None));
    let inner = Arc::new(RingInner {
        slots: slots.into_boxed_slice(),
        head: AtomicUsize::new(0),
        tail: AtomicUsize::new(0),
        mask: capacity - 1,
        dropped: AtomicUsize::new(0),
    });
    (RingProducer { inner: Arc::clone(&inner) }, RingConsumer { inner })
}

impl<T> RingProducer<T> {
    /// Pushes a value, returning `false` when the ring is full.
    ///
    /// Never blocks, never allocates, never grows. A full ring means the UI
    /// thread has fallen behind; dropping is the only real-time-safe response,
    /// and [`RingProducer::dropped`] makes it visible rather than silent.
    pub fn push(&mut self, value: T) -> bool {
        let head = self.inner.head.load(Ordering::Relaxed);
        let next = (head + 1) & self.inner.mask;
        // Acquire: pairs with the consumer's Release store to `tail`, so a slot
        // it has finished reading is safe for us to overwrite.
        if next == self.inner.tail.load(Ordering::Acquire) {
            self.inner.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        // SAFETY: `head` is not equal to `tail` (checked above), so the
        // consumer is not touching this slot, and only this producer stores to
        // `head`.
        unsafe {
            *self.inner.slots[head].get() = Some(value);
        }
        // Release: the slot write above must be visible to a consumer that
        // acquires this `head`.
        self.inner.head.store(next, Ordering::Release);
        true
    }

    /// Pushes as many values as fit, returning how many were accepted.
    pub fn push_slice(&mut self, values: &[T]) -> usize
    where
        T: Copy,
    {
        let mut n = 0;
        for v in values {
            if !self.push(*v) {
                break;
            }
            n += 1;
        }
        n
    }

    /// Values rejected because the ring was full.
    #[inline]
    pub fn dropped(&self) -> usize {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// True when a push would currently fail.
    #[inline]
    pub fn is_full(&self) -> bool {
        let head = self.inner.head.load(Ordering::Relaxed);
        ((head + 1) & self.inner.mask) == self.inner.tail.load(Ordering::Acquire)
    }
}

impl<T> RingConsumer<T> {
    /// Pops the oldest value, or `None` when empty.
    pub fn pop(&mut self) -> Option<T> {
        let tail = self.inner.tail.load(Ordering::Relaxed);
        // Acquire: pairs with the producer's Release store to `head`, so the
        // payload it wrote is visible before we read the slot.
        if tail == self.inner.head.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: `tail != head`, so the producer is not writing this slot, and
        // only this consumer stores to `tail`.
        let value = unsafe { (*self.inner.slots[tail].get()).take() };
        // Release: the slot is free; a producer that acquires this `tail` may
        // reuse it, and must not do so before our read above has happened.
        self.inner.tail.store((tail + 1) & self.inner.mask, Ordering::Release);
        value
    }

    /// Pops up to `out.len()` values, returning how many were written.
    pub fn pop_slice(&mut self, out: &mut [T]) -> usize
    where
        T: Copy,
    {
        let mut n = 0;
        while n < out.len() {
            match self.pop() {
                Some(v) => {
                    out[n] = v;
                    n += 1;
                }
                None => break,
            }
        }
        n
    }

    /// Drains everything, keeping only the most recent `keep` values.
    ///
    /// What a scrolling display wants after a stall: catching up sample by
    /// sample would draw a burst of stale data at the wrong time, so it is
    /// better to skip to the present and say so.
    pub fn drain_keeping_last(&mut self, keep: usize, out: &mut Vec<T>) -> usize {
        let mut skipped = 0;
        while let Some(v) = self.pop() {
            if out.len() == keep && keep > 0 {
                out.remove(0);
                skipped += 1;
            }
            if keep > 0 {
                out.push(v);
            } else {
                skipped += 1;
            }
        }
        skipped
    }

    /// Values currently queued.
    pub fn len(&self) -> usize {
        let head = self.inner.head.load(Ordering::Acquire);
        let tail = self.inner.tail.load(Ordering::Relaxed);
        head.wrapping_sub(tail) & self.inner.mask
    }

    /// True when nothing is queued.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Values the producer had to drop because this side fell behind.
    #[inline]
    pub fn dropped(&self) -> usize {
        self.inner.dropped.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Seqlock
// ---------------------------------------------------------------------------

/// A sequence-locked cell for one large value.
///
/// The writer is wait-free; the reader retries while a write is in flight.
/// Chosen over [`AtomicSnapshot`] when `T` is big enough that three copies of
/// it is a real memory cost — an impulse response or a full spectrum frame —
/// and over a mutex because the writer never blocks.
///
/// The generation counter is even when the value is stable and odd while it is
/// being written, so a reader that sees an odd count, or a different count
/// before and after, knows it may have read a torn value and tries again.
pub struct Seqlock<T> {
    generation: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: all access goes through `write`, which is documented as
// single-writer, and `read`, which retries until it observes a stable
// generation around its copy.
unsafe impl<T: Send> Send for Seqlock<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Sync for Seqlock<T> {}

impl<T: Copy> Seqlock<T> {
    /// Creates a cell holding `value`.
    pub fn new(value: T) -> Self {
        Self { generation: AtomicU32::new(0), value: UnsafeCell::new(value) }
    }

    /// Overwrites the value. Wait-free; safe on the audio thread.
    ///
    /// # Panics
    ///
    /// Never. But calling this from two threads at once breaks the protocol and
    /// will hand readers torn values; the caller is responsible for there being
    /// exactly one writer.
    pub fn write(&self, value: T) {
        let generation = self.generation.load(Ordering::Relaxed);
        // Odd: a write is in progress.
        self.generation.store(generation.wrapping_add(1), Ordering::Relaxed);
        // Release fence so the odd generation is visible before the payload
        // write below, and so the payload is visible before the even one.
        fence(Ordering::Release);
        // SAFETY: the single-writer contract means no other thread is writing;
        // readers that observe the odd generation discard whatever they read.
        unsafe {
            *self.value.get() = value;
        }
        fence(Ordering::Release);
        self.generation.store(generation.wrapping_add(2), Ordering::Relaxed);
    }

    /// Reads the value, retrying while a write is in flight.
    ///
    /// Returns `None` after `max_attempts` failures rather than spinning
    /// forever: a UI thread must never be stuck behind an audio thread, and a
    /// frame drawn with last frame's value is far better than a hang.
    pub fn read(&self, max_attempts: u32) -> Option<T> {
        for _ in 0..max_attempts.max(1) {
            let before = self.generation.load(Ordering::Acquire);
            if before & 1 != 0 {
                // A write is in progress.
                core::hint::spin_loop();
                continue;
            }
            // SAFETY: a torn read is possible here and is exactly what the
            // generation check below detects; `T: Copy` means the bytes are
            // valid for any bit pattern the writer could have left behind.
            let value = unsafe { *self.value.get() };
            fence(Ordering::Acquire);
            if self.generation.load(Ordering::Relaxed) == before {
                return Some(value);
            }
            core::hint::spin_loop();
        }
        None
    }

    /// Reads with a sensible retry budget.
    #[inline]
    pub fn read_relaxed(&self) -> Option<T> {
        // Four attempts is far more than a wait-free writer can lose: it would
        // take four consecutive writes landing inside one read window.
        self.read(4)
    }
}

// ---------------------------------------------------------------------------
// Meter payloads
// ---------------------------------------------------------------------------

/// One channel's level, as an audio callback would report it.
///
/// Linear amplitude rather than dB: the conversion involves a logarithm, and
/// the audio thread should hand over raw numbers and let the UI thread do the
/// maths.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct ChannelLevel {
    /// Peak absolute sample in the last block.
    pub peak: f32,
    /// Root mean square of the last block.
    pub rms: f32,
    /// True when a sample reached or exceeded full scale.
    pub clipped: bool,
}

impl ChannelLevel {
    /// Measures a block of samples.
    ///
    /// Allocation-free and branch-light, so it is safe to call from the audio
    /// callback. Non-finite samples are treated as clipping rather than being
    /// allowed to poison the peak: a NaN that reaches the UI becomes a NaN
    /// vertex position, and one of those corrupts an entire draw call.
    pub fn measure(samples: &[f32]) -> Self {
        let mut peak = 0.0f32;
        let mut sum_squares = 0.0f64;
        let mut clipped = false;

        for &s in samples {
            if !s.is_finite() {
                clipped = true;
                continue;
            }
            let a = s.abs();
            if a > peak {
                peak = a;
            }
            if a >= 1.0 {
                clipped = true;
            }
            sum_squares += (s as f64) * (s as f64);
        }

        let rms = if samples.is_empty() {
            0.0
        } else {
            (sum_squares / samples.len() as f64).sqrt() as f32
        };
        Self { peak, rms, clipped }
    }
}

/// A multi-channel level snapshot.
///
/// Fixed at two channels because that covers the overwhelming majority of meter
/// widgets and keeps the payload `Copy` and small enough to triple-buffer
/// without thought. Surround metering wants its own payload type rather than a
/// `Vec` here, because a `Vec` cannot cross this boundary without allocating.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct StereoLevel {
    /// Left, or mono.
    pub left: ChannelLevel,
    /// Right.
    pub right: ChannelLevel,
    /// Gain reduction currently applied, in linear amplitude. `1.0` is none.
    pub gain_reduction: f32,
}

impl StereoLevel {
    /// A silent snapshot with no gain reduction.
    pub const SILENT: Self = Self {
        left: ChannelLevel { peak: 0.0, rms: 0.0, clipped: false },
        right: ChannelLevel { peak: 0.0, rms: 0.0, clipped: false },
        gain_reduction: 1.0,
    };

    /// The louder of the two channels' peaks.
    #[inline]
    pub fn max_peak(&self) -> f32 {
        self.left.peak.max(self.right.peak)
    }

    /// True when either channel clipped.
    #[inline]
    pub fn clipped(&self) -> bool {
        self.left.clipped || self.right.clipped
    }
}

/// A triple-buffered stereo level channel, the common case.
pub type LevelProducer = SnapshotProducer<StereoLevel>;
/// The reading half of [`LevelProducer`].
pub type LevelConsumer = SnapshotConsumer<StereoLevel>;

/// Creates a level channel initialised to silence.
pub fn level_channel() -> (LevelProducer, LevelConsumer) {
    snapshot(StereoLevel::SILENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::thread;

    #[test]
    fn a_snapshot_starts_with_its_initial_value() {
        let (_producer, mut consumer) = snapshot(42u32);
        assert_eq!(
            *consumer.read(),
            42,
            "the consumer must have something before the first publish"
        );
        assert!(!consumer.has_new_data());
    }

    #[test]
    fn publishing_makes_the_new_value_visible() {
        let (mut producer, mut consumer) = snapshot(0u32);
        producer.set(7);
        assert!(consumer.has_new_data());
        assert_eq!(*consumer.read(), 7);
        assert_eq!(consumer.fetched(), 1);
    }

    #[test]
    fn reading_twice_without_a_publish_returns_the_same_value() {
        // A meter between audio buffers must keep drawing the last level, not
        // fall back to zero.
        let (mut producer, mut consumer) = snapshot(0u32);
        producer.set(5);
        assert_eq!(*consumer.read(), 5);
        assert_eq!(*consumer.read(), 5);
        assert_eq!(consumer.fetched(), 1, "the second read should not have fetched");
    }

    #[test]
    fn only_the_newest_value_survives_a_burst() {
        // Latest-wins is the correct semantic for a snapshot: a meter has no
        // use for the levels it missed.
        let (mut producer, mut consumer) = snapshot(0u32);
        for i in 1..=100 {
            producer.set(i);
        }
        assert_eq!(*consumer.read(), 100);
    }

    #[test]
    fn producer_and_consumer_never_hold_the_same_slot() {
        // The core soundness invariant of the triple buffer.
        let (mut producer, mut consumer) = snapshot(0u32);
        for i in 0..1000 {
            producer.set(i);
            assert_ne!(producer.index, consumer.index, "producer and consumer aliased a slot");
            consumer.read();
            assert_ne!(producer.index, consumer.index, "producer and consumer aliased a slot");
        }
    }

    #[test]
    fn a_snapshot_never_tears_under_real_thread_contention() {
        // A torn read would show a payload whose two halves came from different
        // publishes. Encoding the invariant in the data makes that detectable.
        #[derive(Copy, Clone, Default)]
        struct Pair {
            a: u64,
            b: u64,
        }
        let (mut producer, mut consumer) = snapshot(Pair::default());
        let stop = Arc::new(AtomicBool::new(false));
        let stop_writer = Arc::clone(&stop);

        let writer = thread::spawn(move || {
            let mut i = 0u64;
            while !stop_writer.load(Ordering::Relaxed) {
                i = i.wrapping_add(1);
                producer.publish(|p| {
                    p.a = i;
                    p.b = i.wrapping_mul(3);
                });
            }
        });

        let mut observed = 0usize;
        for _ in 0..200_000 {
            let p = consumer.read();
            assert_eq!(p.b, p.a.wrapping_mul(3), "torn read: a={} b={}", p.a, p.b);
            observed += 1;
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert_eq!(observed, 200_000);
    }

    #[test]
    fn a_snapshot_eventually_sees_the_producers_progress() {
        let (mut producer, mut consumer) = snapshot(0u64);
        let writer = thread::spawn(move || {
            for i in 1..=50_000u64 {
                producer.set(i);
            }
        });
        let mut last = 0;
        for _ in 0..200_000 {
            last = *consumer.read();
            if last >= 40_000 {
                break;
            }
        }
        writer.join().unwrap();
        // Drain whatever the writer left behind.
        let final_value = *consumer.read();
        assert!(final_value >= last);
        assert!(final_value > 0, "the consumer never observed any progress");
    }

    #[test]
    fn a_ring_rounds_its_capacity_up_to_a_power_of_two() {
        let (producer, consumer) = ring::<u32>(5);
        assert_eq!(producer.inner.mask + 1, 8);
        assert!(consumer.is_empty());
    }

    #[test]
    fn a_ring_preserves_order() {
        let (mut producer, mut consumer) = ring(16);
        for i in 0..10 {
            assert!(producer.push(i));
        }
        for i in 0..10 {
            assert_eq!(consumer.pop(), Some(i));
        }
        assert_eq!(consumer.pop(), None);
    }

    #[test]
    fn a_full_ring_rejects_instead_of_blocking_or_growing() {
        // Growing would allocate on the audio thread; blocking would miss the
        // deadline. Dropping is the only real-time-safe answer.
        let (mut producer, consumer) = ring(4);
        let mut accepted = 0;
        for i in 0..100 {
            if producer.push(i) {
                accepted += 1;
            }
        }
        // One slot is reserved to tell full from empty.
        assert_eq!(accepted, 3);
        assert_eq!(producer.dropped(), 97);
        assert_eq!(consumer.len(), 3);
    }

    #[test]
    fn a_ring_wraps_correctly_over_many_cycles() {
        let (mut producer, mut consumer) = ring(4);
        for round in 0..1000u32 {
            assert!(producer.push(round));
            assert_eq!(consumer.pop(), Some(round));
        }
        assert!(consumer.is_empty());
        assert_eq!(producer.dropped(), 0, "a correctly wrapping ring should never drop here");
    }

    #[test]
    fn ring_length_is_accurate_at_every_fill_level() {
        let (mut producer, consumer) = ring::<u32>(8);
        assert_eq!(consumer.len(), 0);
        for i in 0..7 {
            producer.push(i);
            assert_eq!(consumer.len(), (i + 1) as usize);
        }
        assert!(producer.is_full());
    }

    #[test]
    fn a_ring_loses_nothing_under_real_thread_contention() {
        // The property that distinguishes a ring from a snapshot: a waveform
        // that skips samples draws a lie, so nothing accepted may be lost.
        const COUNT: u32 = 200_000;
        let (mut producer, mut consumer) = ring(1024);

        let writer = thread::spawn(move || {
            let mut sent = 0u32;
            let mut value = 0u32;
            while value < COUNT {
                if producer.push(value) {
                    value += 1;
                    sent += 1;
                } else {
                    std::hint::spin_loop();
                }
            }
            sent
        });

        let mut expected = 0u32;
        while expected < COUNT {
            if let Some(v) = consumer.pop() {
                assert_eq!(v, expected, "the ring reordered or dropped a value");
                expected += 1;
            } else {
                std::hint::spin_loop();
            }
        }
        assert_eq!(writer.join().unwrap(), COUNT);
    }

    #[test]
    fn drain_keeping_last_skips_to_the_present() {
        let (mut producer, mut consumer) = ring(64);
        for i in 0..40 {
            producer.push(i);
        }
        let mut out = Vec::new();
        let skipped = consumer.drain_keeping_last(10, &mut out);
        assert_eq!(out.len(), 10);
        assert_eq!(out.first(), Some(&30));
        assert_eq!(out.last(), Some(&39));
        assert_eq!(skipped, 30);
    }

    #[test]
    fn push_and_pop_slice_move_bulk_data() {
        let (mut producer, mut consumer) = ring(64);
        let source: Vec<f32> = (0..50).map(|i| i as f32).collect();
        assert_eq!(producer.push_slice(&source), 50);
        let mut out = vec![0.0f32; 50];
        assert_eq!(consumer.pop_slice(&mut out), 50);
        assert_eq!(out, source);
    }

    #[test]
    fn a_seqlock_round_trips_a_stable_value() {
        let lock = Seqlock::new([0u32; 64]);
        let mut payload = [0u32; 64];
        for (i, v) in payload.iter_mut().enumerate() {
            *v = i as u32;
        }
        lock.write(payload);
        assert_eq!(lock.read_relaxed(), Some(payload));
    }

    #[test]
    fn a_seqlock_never_returns_a_torn_value_under_contention() {
        // Every element of the payload carries the same value, so any mixture
        // of two writes is detectable.
        const LEN: usize = 128;
        let lock = Arc::new(Seqlock::new([0u64; LEN]));
        let writer_lock = Arc::clone(&lock);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_writer = Arc::clone(&stop);

        let writer = thread::spawn(move || {
            let mut i = 0u64;
            while !stop_writer.load(Ordering::Relaxed) {
                i = i.wrapping_add(1);
                writer_lock.write([i; LEN]);
            }
        });

        let mut successes = 0;
        for _ in 0..100_000 {
            if let Some(v) = lock.read(64) {
                let first = v[0];
                assert!(v.iter().all(|x| *x == first), "torn seqlock read");
                successes += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        assert!(successes > 0, "every read failed, which suggests a protocol bug");
    }

    #[test]
    fn a_seqlock_read_gives_up_rather_than_spinning_forever() {
        // The UI thread must never be stuck behind the audio thread.
        let lock = Seqlock::new(0u32);
        // Force the odd, mid-write generation and leave it there.
        lock.generation.store(1, Ordering::Relaxed);
        assert_eq!(lock.read(4), None);
    }

    #[test]
    fn channel_level_measures_peak_and_rms() {
        let level = ChannelLevel::measure(&[0.0, 0.5, -0.75, 0.25]);
        assert!((level.peak - 0.75).abs() < 1e-6);
        // sqrt((0 + 0.25 + 0.5625 + 0.0625) / 4) = sqrt(0.21875)
        assert!((level.rms - 0.4677).abs() < 1e-3, "{}", level.rms);
        assert!(!level.clipped);
    }

    #[test]
    fn full_scale_counts_as_clipping() {
        assert!(ChannelLevel::measure(&[0.5, 1.0]).clipped);
        assert!(ChannelLevel::measure(&[0.5, -1.2]).clipped);
        assert!(!ChannelLevel::measure(&[0.5, 0.999]).clipped);
    }

    #[test]
    fn a_non_finite_sample_is_treated_as_clipping_and_never_reaches_the_peak() {
        // A plug-in will eventually feed NaN. If it becomes a vertex position
        // the whole draw call is corrupted, so it must be stopped here.
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let level = ChannelLevel::measure(&[0.25, bad, 0.5]);
            assert!(level.clipped, "{bad} was not reported as clipping");
            assert!(level.peak.is_finite(), "{bad} poisoned the peak");
            assert!(level.rms.is_finite(), "{bad} poisoned the rms");
            assert!((level.peak - 0.5).abs() < 1e-6);
        }
    }

    #[test]
    fn an_empty_block_measures_silence_without_dividing_by_zero() {
        let level = ChannelLevel::measure(&[]);
        assert_eq!(level.peak, 0.0);
        assert_eq!(level.rms, 0.0);
        assert!(!level.clipped);
    }

    #[test]
    fn rms_of_a_full_scale_square_wave_is_one() {
        let square: Vec<f32> = (0..1000).map(|i| if i % 2 == 0 { 1.0 } else { -1.0 }).collect();
        let level = ChannelLevel::measure(&square);
        assert!((level.rms - 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_level_channel_starts_silent() {
        let (_p, mut c) = level_channel();
        let level = *c.read();
        assert_eq!(level, StereoLevel::SILENT);
        assert_eq!(level.gain_reduction, 1.0, "no reduction must be 1.0, not 0.0");
        assert!(!level.clipped());
    }

    #[test]
    fn stereo_level_reports_the_louder_channel() {
        let level = StereoLevel {
            left: ChannelLevel { peak: 0.2, rms: 0.1, clipped: false },
            right: ChannelLevel { peak: 0.9, rms: 0.4, clipped: true },
            gain_reduction: 1.0,
        };
        assert!((level.max_peak() - 0.9).abs() < 1e-6);
        assert!(level.clipped(), "clipping on either channel must be reported");
    }
}
