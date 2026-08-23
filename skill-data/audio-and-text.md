# Realtime audio, and text

## The audio-thread boundary

The rule the whole `spherekit-audio-ui` crate exists to enforce: **realtime
audio data never renders from the audio thread.**

The audio callback must never block, allocate, or take a lock. It does exactly
one thing for the display — pushes samples it has already produced into a
lock-free structure. Everything expensive (FFT, meter ballistics, drawing)
happens on the UI thread, where being late costs a dropped frame rather than an
audible dropout.

Three transports, in `spherekit::audio::transfer`:

| | Use for |
|---|---|
| `ring(capacity)` | A stream of samples. SPSC. `push_slice` from audio, `drain_keeping_last` from the UI |
| `snapshot(initial)` | One value replaced wholesale. Triple-buffered |
| `level_channel()` | Meter levels specifically |
| `Seqlock` | A small `Copy` value read far more often than written |

**The ring is allowed to overflow.** If the UI stalls, the producer drops what
it cannot fit and carries on, because the alternative — blocking audio until the
UI catches up — is something the listener hears. A visualiser missing a few
milliseconds during a stall is invisible.

Batch the pushes. One `push_slice` per few hundred samples is one
acquire/release pair instead of hundreds.

## Drawing

`realtime_canvas(|frame: &mut RealtimeFrame| …)` gives a `RealtimeFrame` with
the node's bounds, the theme, the time, and:

`draw_meter` `draw_gain_reduction` `draw_waveform` `draw_envelope`
`draw_spectrum` `draw_frequency_grid` `draw_eq_curve` `draw_compressor_curve`
`draw_oscilloscope` `draw_vectorscope` `draw_piano`

`draw_spectrum` takes **linear magnitudes** and does the dB conversion itself.

`MeterState::advance(target_db, clipped, dt, &ballistics)` applies attack and
release. Feed it **RMS**, not peak: on the default −60…0 dB scale a modern
master peaks within a decibel of full scale almost continuously, so a peak-fed
bar sits pinned at the top and stops saying anything about the music. Keep peak
for the clip indicator.

If your tap sits before a volume fader, apply the gain yourself — otherwise the
meters ignore the volume control entirely.

## Text

Glyphs are MTSDF distance fields, rasterised once and sampled at any size, so
zooming a panel re-rasterises nothing. Below a physical-size threshold an
analytic-coverage bitmap rasteriser takes over.

Shaping is rustybuzz. Runs are split by script, bidi level **and font**, and a
Common/Inherited character stays on the run's current font — which is what stops
a single space from cutting a Thai or CJK run in half and handing the remainder
back to the Latin primary.

### Marks carry their own placement

A combining mark's position is frequently **baked into its outline**, not
supplied as a GPOS offset. A Thai tone mark in Leelawadee UI looks like this:

```text
bbox = { x_min: -280, y_min: 1110, x_max: -180, y_max: 1440 }, advance = 0
```

Negative x pulls it back over the consonant; y 1110–1440 puts it high above the
baseline. A shaper `y_offset` of `0` for such a mark is **correct**, not a bug.

The consequence: anything that clamps a negative left bearing, or assumes ink
starts at or below the baseline, drops the mark beside its base at baseline
height. `GlyphImage::bounds_em` is y-down and signed precisely so it can carry
this. There are tests in `raster.rs` and `shape.rs` pinning it — read them
before touching glyph placement.

### Wrapping

UAX #14 line breaking, with a grapheme-boundary mode for Thai and CJK, which
have no spaces between words and would otherwise produce one enormous line.
