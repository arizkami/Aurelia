# Realtime audio UI

The rule this document exists to state, and the machinery that enforces it.

## The rule

**Realtime audio data never renders from the audio thread.**

The audio callback has a hard deadline. Miss it and the user hears a click — not a dropped frame,
not a stutter, an audible defect in the output. So on that thread there must be no:

| Forbidden | Why |
|---|---|
| Allocation | The allocator can take a lock, and the lock can be held by a thread that just page-faulted |
| A contended lock | It is fast until the one frame it is not |
| GPU work | Submission can block on the driver |
| Text shaping | Unbounded, allocating, and cache-dependent |
| Filesystem access | Unbounded, and on Windows can block on a virus scanner |
| An unbounded loop | Any loop whose iteration count depends on data |

A `Mutex` is the usual mistake. It looks fine in testing because contention is rare, and it fails
in production because rare is not never.

## What the callback does instead

```text
Audio thread                        UI / render thread
------------                        ------------------
measure the block
      |
      v
  AtomicSnapshot  ───────────────▶  read newest level
  SpscRing        ───────────────▶  drain the sample stream
  Seqlock         ───────────────▶  copy the spectrum frame
                                          |
                                          v
                                    RealtimePaintNode marked PAINT-dirty
                                          |
                                          v
                                    mesh generated → GPU instance → drawn
```

And what must never happen:

```text
DSP callback ──▶ mutex ──▶ UI tree update ──▶ GPU call
```

## The three transfer primitives

All three live in `sphere_audio_ui::transfer`. Which one to use is decided by the *shape* of the
data, not by preference.

| Type | Data shape | Use for |
|---|---|---|
| [`AtomicSnapshot`] | One small `T`, latest wins | Meter levels, gain reduction, transport position |
| [`SpscRing`] | A stream, none may be dropped | Waveform samples, FFT frames, MIDI events |
| [`Seqlock`] | One large `T`, latest wins | A whole spectrum frame, an impulse response |

The distinction that matters:

A **meter** only wants the newest value. Dropping the levels it missed between frames is not a
loss — nobody can see 200 Hz of meter updates — so latest-wins is correct and a queue would just
build a backlog.

A **scrolling waveform** must not skip samples. Skipping draws a lie: a transient that fell in a
dropped block simply is not in the picture. So it needs a queue, and the queue has to be bounded
because growing it would allocate on the audio thread.

### `AtomicSnapshot` — triple buffered

Three slots, because two is not enough: a producer that wants to publish while the consumer is
mid-read has nowhere to go. The producer and consumer each own an index, and a single atomic
control word holds the third plus a dirty bit.

```rust
let (mut producer, mut consumer) = level_channel();

// Audio thread — wait-free, one closure call and one atomic swap.
producer.set(StereoLevel { left, right, gain_reduction });

// UI thread — never blocks; returns the previous value if nothing is new,
// which is exactly right for a meter between audio buffers.
let level = *consumer.read();
```

The producer's publish is a `Release` swap and the consumer's fetch is an `AcqRel` swap. Without
the `Release` the consumer can observe the new index while still seeing stale payload; that is the
whole bug class the ordering exists to prevent, and there is a two-thread test that hammers it and
asserts no torn read.

### `SpscRing` — bounded, lock-free

Power-of-two capacity so the wrap is a mask rather than a modulo; a division on the audio thread is
avoidable and therefore avoided. One slot is reserved to distinguish full from empty.

```rust
// Audio thread. Returns false when the UI has fallen behind.
if !producer.push(sample) {
    // Dropping is the only real-time-safe response. `producer.dropped()`
    // makes it visible rather than silent.
}
```

The producer only ever stores to `head`, the consumer only ever to `tail`, and the emptiness and
fullness checks guarantee they never touch the same slot. Producer and consumer are separate,
non-`Clone` handles, so "single producer, single consumer" is a compile-time property rather than a
comment.

`drain_keeping_last` exists for the case after a stall: catching up sample by sample would draw a
burst of stale data at the wrong time, so a display would rather skip to the present.

### `Seqlock` — for large payloads

An even generation means stable, odd means a write is in flight. A reader that sees an odd count,
or a different count before and after its copy, retries. The writer never blocks.

`read` takes a retry budget and returns `None` when it runs out rather than spinning forever: a UI
thread must never be stuck behind an audio thread, and a frame drawn with last frame's spectrum is
far better than a hang.

## Why `unsafe` appears in this module and nowhere else

A lock-free buffer hands out `&T` to memory another thread may be writing. That is exactly what
`UnsafeCell` is for and it cannot be expressed safely. Every block carries a `// SAFETY:` comment
naming the invariant that makes it sound, and the invariants are enforced by the types rather than
requested in prose.

## Realtime paint nodes

A [`RealtimeCanvas`] is an element whose contents are regenerated every frame from a data snapshot.
It is not a widget with an occasionally-changing value; it is a surface that redraws continuously
while the transport rolls.

```rust
realtime_canvas(move |frame| {
    frame.draw_meter(&meter_state, &MeterStyle::default());
})
.id("meter-l")
.w(px(18.0))
.h(relative(1.0))
```

Two properties follow:

**It invalidates PAINT only.** Its box never changes size, so relayout would be pure waste at 60 or
120 Hz. `crates/sphere-audio-ui/src/realtime.rs` contains a test that builds a real tree, repaints
a meter sixty times, and asserts `nodes_laid_out == 0` on every one of them.

**It builds meshes, not paths.** A spectrum is already a list of points; a waveform is already a
list of vertical spans. Turning either into a `Path` so a tessellator can turn it back into
triangles is work with no product. `Canvas::draw_mesh` exists for this, and the waveform test
asserts the scene contains a `Mesh` command and *no* `FillPath` command.

**It culls.** A mixer scrolled past 400 channels must not generate geometry for the 360 that are
offscreen. `RealtimeCanvas::paint` checks `cx.is_culled()` before running the closure at all, and
there is a test that lays out 40 strips in a 400 px viewport and asserts at most 5 closures ran.

## The primitives

| Function | What it draws |
|---|---|
| `draw_meter` | Peak + RMS level with peak-hold and a clip indicator |
| `draw_gain_reduction` | An inverted meter, hanging from unity |
| `draw_waveform` | Min/max envelope from raw samples |
| `draw_envelope` | The same from a precomputed envelope |
| `draw_spectrum` | Log-frequency, dB-magnitude, filled or outlined |
| `draw_eq_curve` | Magnitude response over log frequency |
| `draw_compressor_curve` | Input/output transfer with soft knee and a unity reference |
| `draw_oscilloscope` | A time-domain trace |
| `draw_vectorscope` | A rotated Lissajous stereo field |
| `draw_piano` | A keyboard with correct black-key geometry |
| `draw_frequency_grid` | The 1-2-5 decade gridlines an analyser sits on |

## The measurement maths

Kept in `sphere_audio_ui::dsp`, separate from the drawing, because it is the part that is easy to
get subtly and invisibly wrong.

### Meters are not linear

A meter whose scale is linear in amplitude looks plausible and is useless: everything below −20 dB
collapses into the bottom two per cent of the widget, which is exactly the range a mix engineer
works in.

`MeterScale` maps dB to a position and additionally applies a **skew**, because meters devote more
space to the top of the range — the difference between −3 and 0 dB matters far more than between
−60 and −57. The default `0.6` is close to a hardware meter face. A test asserts that a skewed
scale puts −20 dB higher up the meter than a linear-in-dB one does.

### Ballistics

A meter showing the current block's peak flickers unreadably. Real meters rise instantly and fall
at a rate, and hold the highest recent peak so the eye can catch it.

```rust
pub struct MeterBallistics {
    pub release_db_per_second: f32,   // 20 dB/s, the IEC PPM figure
    pub peak_hold_seconds: f32,
    pub peak_fall_db_per_second: f32,
    pub clip_hold_seconds: f32,
}
```

A rate rather than an exponential decay, because dB-per-second is what meter specifications are
written in and it makes the fall time predictable regardless of how far it has to travel. There is
a test that runs one second of silence at 20 dB/s from 0 dB and asserts the meter reads −20.

Ballistics run on the **UI thread**, not the audio thread: they depend on frame timing, and the
audio thread has no idea when a frame happened.

### Frequency is logarithmic

On a linear axis, half the width of a spectrum goes to 10–20 kHz where almost nothing musically
interesting happens, and the two octaves carrying the bass are squeezed into a few pixels.
`LogFrequencyScale` maps 20 Hz to 20 kHz across a width, and a test asserts every octave gets equal
width — the defining property, and the reason the scale is used.

### Waveforms use min/max, never decimation

For N samples into W columns, take the extremes of each column, not every Nth sample.

Decimation makes a waveform look thinner than it is and drops transients entirely — the click at
the start of a snare simply vanishes. `min_max_envelope` computes per-column extremes with integer
boundary arithmetic so every sample lands in exactly one column and none falls between. There are
three tests: one asserting the envelope brackets every sample, one asserting a lone transient
survives, and one asserting the column boundaries tile with no gaps.

## NaN is not hypothetical

A plug-in will eventually feed the UI a NaN. If it becomes a vertex position, it does not produce a
wrong pixel — it corrupts the entire draw call.

So every entry point clamps:

- `linear_to_db` maps silence, negatives, NaN and both infinities to `MIN_DB`, never to `-inf`.
- `ChannelLevel::measure` treats a non-finite sample as clipping and excludes it from the peak and
  the RMS.
- `min_max_envelope` skips non-finite samples, and a column containing only garbage collapses to
  zero rather than staying at ±infinity.
- `LogFrequencyScale::new` clamps both bounds above zero, because `log10(0)` is `-inf`.

Each of those has a test that feeds it NaN and both infinities and asserts the output is finite.

## Frame scheduling

`sphere_platform::RedrawPolicy` has four modes:

| Mode | Loop behaviour |
|---|---|
| `Idle` | Blocks. A static window costs zero CPU. |
| `Dirty` | One frame is owed. |
| `Animating` | Paced against the display refresh. |
| `Realtime` | Continuous, unpaced. |

`Realtime` is the most expensive and should be entered only while the transport is actually
rolling. `cx.scheduler_mut().begin_realtime()` turns it on; `end_realtime()` turns it off. The rest
of the UI stays cached throughout — the meters repaint, and nothing else does any work at all.
