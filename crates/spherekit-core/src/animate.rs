//! Springs, tweens, and the state machine both drive.
//!
//! ## Why springs first
//!
//! A duration-plus-easing tween is described by `(from, to, t)`. Interrupt one
//! — hover in, then hover out 40 ms later — and there is no honest answer:
//! restarting `t` jumps, and keeping it finishes the wrong animation. A mixer
//! strip the pointer sweeps across generates dozens of overlapping transitions
//! a second, so this is the common case, not the edge case.
//!
//! A spring is described by `(position, velocity, target)`. [`Motion::retarget`]
//! writes the target and touches neither of the other two, so the next step
//! solves from wherever the value actually is with whatever momentum it
//! actually has. There is structurally nothing that can jump.
//!
//! Tweens are still here, as the degenerate case sharing the same state
//! machine, because a progress readout that must finish exactly when the
//! operation does is a real requirement. What a tween genuinely adds over a
//! critically damped spring is a *hard end time* — which is precisely the
//! property that makes it uninterruptible, and precisely why it is not the
//! default.
//!
//! ## Why the spring step is analytic
//!
//! Semi-implicit Euler accumulates `O(dt²)` error per step, so a spring
//! settles visibly differently at 60 Hz and at 144 Hz. Dragging a plug-in
//! window from a laptop panel to an external display mid-gesture is enough to
//! expose it. The closed form makes frame-rate independence exact rather than
//! approximate, and there is a test asserting one 32 ms step equals two 16 ms
//! steps.
//!
//! It is also unconditionally stable: at a huge `dt`, `exp(-zwt)` underflows to
//! zero and the value lands exactly on target, where Euler gains energy and
//! diverges. The closed form is valid because the target is constant *within* a
//! step, which holds because targets are set once per frame.

use crate::color::{Color, linear_to_srgb, srgb_to_linear};
use crate::geometry::{Corners, Edges, Point, Rect, Size};
use crate::unit::Px;
use core::marker::PhantomData;
use core::time::Duration;

/// The widest animatable value, in `f32` lanes.
///
/// Four is not arbitrary: [`Color`], [`Rect`], [`Corners`] and [`Edges`] are
/// exactly four components, [`Point`] and [`Size`] two, [`Px`] and `f32` one.
/// [`crate::transform::Affine`] would need six and is out of scope regardless,
/// because it has no translation/rotation/scale decomposition and interpolating
/// its six raw coefficients is simply wrong through a rotation.
pub const MAX_CHANNELS: usize = 4;

/// The window a settling test looks ahead over, in seconds.
///
/// A thirtieth of a second: velocity below `REST / SETTLE_WINDOW` cannot move a
/// value a visible distance before the next frame even on a 30 Hz display.
pub const SETTLE_WINDOW: f32 = 1.0 / 30.0;

/// A value an animation can carry.
pub trait Animatable: Copy + 'static {
    /// How many lanes this type occupies. Must be at most [`MAX_CHANNELS`].
    const CHANNELS: usize;

    /// Per-lane displacement below which two values are indistinguishable on
    /// screen, in this type's own interpolation space.
    ///
    /// The type declares what "arrived" means for it, because only the type
    /// knows. Half a hundredth of a logical pixel is invisible; half a
    /// hundredth of a colour lane is four sRGB steps and is not.
    const REST: [f32; MAX_CHANNELS];

    /// Writes the value into interpolation space.
    ///
    /// Lanes past `CHANNELS` must be left alone.
    fn write_channels(self, out: &mut [f32; MAX_CHANNELS]);

    /// Reads a value back out of interpolation space.
    ///
    /// Implementations **must** tolerate lanes outside their natural range: an
    /// underdamped spring overshoots by construction, so this is handed `1.015`
    /// before a [`Spring::SNAPPY`] fade settles.
    fn read_channels(src: &[f32; MAX_CHANNELS]) -> Self;

    /// Clamped linear interpolation, in the same space the spring works in.
    ///
    /// One contract for every type. `Point::lerp` does not clamp and
    /// `Color::lerp` does; reconciling those two inherent methods would change
    /// dash tessellation, so this trait method is the single contract animation
    /// uses and the inherent ones are left alone.
    fn lerp(self, other: Self, t: f32) -> Self;
}

// ------------------------------------------------------------- scalar types

impl Animatable for f32 {
    const CHANNELS: usize = 1;
    // A thousandth: below the quantisation of anything this drives.
    const REST: [f32; MAX_CHANNELS] = [1.0 / 1024.0; MAX_CHANNELS];

    #[inline]
    fn write_channels(self, out: &mut [f32; MAX_CHANNELS]) {
        out[0] = self;
    }
    #[inline]
    fn read_channels(src: &[f32; MAX_CHANNELS]) -> Self {
        src[0]
    }
    #[inline]
    fn lerp(self, other: Self, t: f32) -> Self {
        self + (other - self) * t.clamp(0.0, 1.0)
    }
}

impl Animatable for Px {
    const CHANNELS: usize = 1;
    // A fiftieth of a logical pixel: under half a device pixel even at 2x.
    const REST: [f32; MAX_CHANNELS] = [0.02; MAX_CHANNELS];

    #[inline]
    fn write_channels(self, out: &mut [f32; MAX_CHANNELS]) {
        out[0] = self.get();
    }
    #[inline]
    fn read_channels(src: &[f32; MAX_CHANNELS]) -> Self {
        Px(src[0])
    }
    #[inline]
    fn lerp(self, other: Self, t: f32) -> Self {
        Px(self.get() + (other.get() - self.get()) * t.clamp(0.0, 1.0))
    }
}

// ---------------------------------------------------------- geometry types

macro_rules! animatable_px_pair {
    ($ty:ident, $a:ident, $b:ident) => {
        impl Animatable for $ty<Px> {
            const CHANNELS: usize = 2;
            const REST: [f32; MAX_CHANNELS] = [0.02; MAX_CHANNELS];

            #[inline]
            fn write_channels(self, out: &mut [f32; MAX_CHANNELS]) {
                out[0] = self.$a.get();
                out[1] = self.$b.get();
            }
            #[inline]
            fn read_channels(src: &[f32; MAX_CHANNELS]) -> Self {
                $ty { $a: Px(src[0]), $b: Px(src[1]) }
            }
            #[inline]
            fn lerp(self, other: Self, t: f32) -> Self {
                let t = t.clamp(0.0, 1.0);
                $ty {
                    $a: Px(self.$a.get() + (other.$a.get() - self.$a.get()) * t),
                    $b: Px(self.$b.get() + (other.$b.get() - self.$b.get()) * t),
                }
            }
        }
    };
}

animatable_px_pair!(Point, x, y);
animatable_px_pair!(Size, width, height);

macro_rules! animatable_px_quad {
    ($ty:ident, $a:ident, $b:ident, $c:ident, $d:ident) => {
        impl Animatable for $ty<Px> {
            const CHANNELS: usize = 4;
            const REST: [f32; MAX_CHANNELS] = [0.02; MAX_CHANNELS];

            #[inline]
            fn write_channels(self, out: &mut [f32; MAX_CHANNELS]) {
                out[0] = self.$a.get();
                out[1] = self.$b.get();
                out[2] = self.$c.get();
                out[3] = self.$d.get();
            }
            #[inline]
            fn read_channels(src: &[f32; MAX_CHANNELS]) -> Self {
                $ty { $a: Px(src[0]), $b: Px(src[1]), $c: Px(src[2]), $d: Px(src[3]) }
            }
            #[inline]
            fn lerp(self, other: Self, t: f32) -> Self {
                let t = t.clamp(0.0, 1.0);
                $ty {
                    $a: Px(self.$a.get() + (other.$a.get() - self.$a.get()) * t),
                    $b: Px(self.$b.get() + (other.$b.get() - self.$b.get()) * t),
                    $c: Px(self.$c.get() + (other.$c.get() - self.$c.get()) * t),
                    $d: Px(self.$d.get() + (other.$d.get() - self.$d.get()) * t),
                }
            }
        }
    };
}

animatable_px_quad!(Corners, top_left, top_right, bottom_right, bottom_left);
animatable_px_quad!(Edges, top, right, bottom, left);

/// Interpolated as **origin plus size**, not as min plus max.
///
/// Origin+size is what layout produces, and it keeps a rectangle collapsing to
/// zero from inverting partway through the interpolation, which min+max does
/// when the two edges cross.
impl Animatable for Rect<Px> {
    const CHANNELS: usize = 4;
    const REST: [f32; MAX_CHANNELS] = [0.02; MAX_CHANNELS];

    #[inline]
    fn write_channels(self, out: &mut [f32; MAX_CHANNELS]) {
        out[0] = self.origin.x.get();
        out[1] = self.origin.y.get();
        out[2] = self.size.width.get();
        out[3] = self.size.height.get();
    }
    #[inline]
    fn read_channels(src: &[f32; MAX_CHANNELS]) -> Self {
        Rect { origin: Point::new(Px(src[0]), Px(src[1])), size: Size::new(Px(src[2]), Px(src[3])) }
    }
    #[inline]
    fn lerp(self, other: Self, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mut a = [0.0; MAX_CHANNELS];
        let mut b = [0.0; MAX_CHANNELS];
        self.write_channels(&mut a);
        other.write_channels(&mut b);
        for i in 0..MAX_CHANNELS {
            a[i] += (b[i] - a[i]) * t;
        }
        Self::read_channels(&a)
    }
}

// ------------------------------------------------------------------- colour

/// Interpolated in unpremultiplied linear light — exactly the space
/// [`Color::lerp`] mixes in — so a lane-wise spring reproduces `Color::lerp`
/// rather than passing a red-to-green fade through brown. There is a test
/// asserting the two agree, because two colour paths that can drift apart is
/// worse than one that is slower.
///
/// `read_channels` clamps each lane before converting back. Not for safety:
/// [`linear_to_srgb`] already clamps its own input, so an overshooting lane
/// cannot produce a `NaN`. It is so that the value a caller reads is the value
/// the settle test compares against — without it, `position` could sit at
/// `1.01` while the colour it round-trips to is pinned at `1.0`, and the motion
/// would report movement that is not visible. The consequence is stated
/// plainly: a bouncy colour transition saturates at its endpoints instead of
/// overshooting them.
///
/// `REST` is a 4096th per lane: one 8-bit sRGB step near black is about 0.0003
/// in linear light, so this stays under quantisation even in shadow.
///
/// Known gap, inherited from `Color::lerp` deliberately: an endpoint of
/// [`Color::TRANSPARENT`] loses its hue, because unpremultiplying at zero alpha
/// returns all zeros. Author such an endpoint as `colour.with_alpha(0.0)`.
impl Animatable for Color {
    const CHANNELS: usize = 4;
    const REST: [f32; MAX_CHANNELS] = [1.0 / 4096.0; MAX_CHANNELS];

    #[inline]
    fn write_channels(self, out: &mut [f32; MAX_CHANNELS]) {
        let a = self.a.clamp(0.0, 1.0);
        out[0] = srgb_to_linear(self.r);
        out[1] = srgb_to_linear(self.g);
        out[2] = srgb_to_linear(self.b);
        out[3] = a;
    }

    #[inline]
    fn read_channels(src: &[f32; MAX_CHANNELS]) -> Self {
        Color {
            r: linear_to_srgb(src[0].clamp(0.0, 1.0)),
            g: linear_to_srgb(src[1].clamp(0.0, 1.0)),
            b: linear_to_srgb(src[2].clamp(0.0, 1.0)),
            a: src[3].clamp(0.0, 1.0),
        }
    }

    #[inline]
    fn lerp(self, other: Self, t: f32) -> Self {
        Color::lerp(self, other, t)
    }
}

// ------------------------------------------------------------------ springs

/// A damped harmonic oscillator.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Spring {
    /// Mass. Rarely worth changing; stiffness and damping cover the space.
    pub mass: f32,
    /// Restoring force per unit displacement.
    pub stiffness: f32,
    /// Resistive force per unit velocity.
    pub damping: f32,
}

impl Spring {
    /// Critically damped, ω = 32 rad/s: 99 % of any displacement in ~208 ms.
    ///
    /// The default, because a control that overshoots its own hover colour
    /// reads as a bug rather than as motion.
    pub const SMOOTH: Self = Self { mass: 1.0, stiffness: 1024.0, damping: 64.0 };
    /// Critically damped, ω = 64 rad/s: ~104 ms. For things that should feel
    /// immediate but not instant.
    pub const STIFF: Self = Self { mass: 1.0, stiffness: 4096.0, damping: 128.0 };
    /// ζ = 0.8: one overshoot of roughly 1.5 % of the displacement.
    pub const SNAPPY: Self = Self { mass: 1.0, stiffness: 1024.0, damping: 51.2 };
    /// ζ = 0.55: one overshoot of roughly 13 %. Not for anything a user reads.
    pub const BOUNCY: Self = Self { mass: 1.0, stiffness: 1024.0, damping: 35.2 };

    /// Unit mass with `damping = 2·√stiffness`: exactly critical.
    pub fn critical(stiffness: f32) -> Self {
        let k = stiffness.max(f32::MIN_POSITIVE);
        Self { mass: 1.0, stiffness: k, damping: 2.0 * k.sqrt() }
    }

    /// Rebuilds `damping` for a target damping ratio.
    ///
    /// Clamped to `[0.05, 4.0]`: below that the settling time diverges and the
    /// loop would not return to idle in any reasonable time.
    #[must_use]
    pub fn with_damping_ratio(self, zeta: f32) -> Self {
        let zeta = zeta.clamp(0.05, 4.0);
        Self { damping: 2.0 * zeta * (self.stiffness * self.mass).sqrt(), ..self }
    }

    /// Critically damped, sized so any displacement is 99 % gone in `settle`.
    ///
    /// From solving `(1 + u)·e^(−u) = 0.01`, which gives `u ≈ 6.64`, so
    /// `ω = 6.64 / settle`. This gives the ergonomics of a duration back
    /// without giving up interruptibility.
    ///
    /// It is a duration for the eye, not a guaranteed stop time: the last one
    /// per cent takes longer or shorter depending on the displacement and the
    /// type's [`Animatable::REST`].
    pub fn with_settling_time(settle: Duration) -> Self {
        let secs = settle.as_secs_f32().max(1.0e-4);
        let w = 6.64 / secs;
        Self { mass: 1.0, stiffness: w * w, damping: 2.0 * w }
    }

    /// Undamped angular frequency, `√(k/m)`.
    #[inline]
    pub fn angular_frequency(self) -> f32 {
        (self.stiffness.max(0.0) / self.mass.max(f32::MIN_POSITIVE)).sqrt()
    }

    /// Damping ratio, `c / (2·√(k·m))`. 1 is critical.
    #[inline]
    pub fn damping_ratio(self) -> f32 {
        let denom = 2.0 * (self.stiffness.max(0.0) * self.mass.max(f32::MIN_POSITIVE)).sqrt();
        if denom <= 0.0 { 0.0 } else { self.damping / denom }
    }

    /// Roughly how long until 99 % of a displacement is gone.
    ///
    /// An estimate, for choosing between springs and for documentation. The
    /// real stop is decided by [`Motion::step`]'s settle test.
    pub fn settling_time(self) -> Duration {
        let w = self.angular_frequency();
        if w <= 0.0 {
            return Duration::ZERO;
        }
        let z = self.damping_ratio().max(0.05);
        // Underdamped decays with the envelope e^(−ζωt); critical and above
        // carry the extra (1 + ωt) term that the 6.64 constant accounts for.
        let secs = if z < 1.0 { 4.6 / (z * w) } else { 6.64 / w };
        Duration::from_secs_f32(secs.min(3600.0))
    }

    /// Advances one lane analytically.
    ///
    /// Public because it is the whole of the physics and has to be testable
    /// with no [`Motion`], no [`Animatable`] and no store around it.
    pub fn step_channel(self, position: &mut f32, velocity: &mut f32, target: f32, dt: f32) {
        if dt <= 0.0 || !dt.is_finite() {
            return;
        }
        let w = self.angular_frequency();
        if w <= 0.0 {
            // A spring with no stiffness cannot restore. Coast, so the value is
            // still continuous rather than frozen or NaN.
            *position += *velocity * dt;
            return;
        }
        let z = self.damping_ratio();
        let d0 = *position - target;
        let v0 = *velocity;
        let e = (-z * w * dt).exp();

        let (d, v) = if (z - 1.0).abs() < 1.0e-3 {
            // Critical. Both other branches divide by a quantity that goes to
            // zero here, which is the whole reason this band exists.
            let b = v0 + w * d0;
            ((d0 + b * dt) * e, (v0 - w * b * dt) * e)
        } else if z < 1.0 {
            let wd = w * (1.0 - z * z).sqrt();
            let (sin, cos) = (wd * dt).sin_cos();
            (
                e * (d0 * cos + ((v0 + z * w * d0) / wd) * sin),
                e * (v0 * cos - ((w * w * d0 + z * w * v0) / wd) * sin),
            )
        } else {
            let s = (z * z - 1.0).sqrt();
            let (r1, r2) = (-w * (z - s), -w * (z + s));
            let c2 = (v0 - r1 * d0) / (r2 - r1);
            let c1 = d0 - c2;
            let (e1, e2) = ((r1 * dt).exp(), (r2 * dt).exp());
            (c1 * e1 + c2 * e2, c1 * r1 * e1 + c2 * r2 * e2)
        };

        // A `dt` large enough to underflow the exponentials lands exactly on
        // target rather than producing a NaN from `inf * 0`.
        if d.is_finite() && v.is_finite() {
            *position = target + d;
            *velocity = v;
        } else {
            *position = target;
            *velocity = 0.0;
        }
    }
}

impl Default for Spring {
    fn default() -> Self {
        Self::SMOOTH
    }
}

// ------------------------------------------------------------------- tweens

/// The shape of a tween's progress over its duration.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Curve {
    /// Constant rate.
    Linear,
    /// Starts slow. `t²`.
    EaseIn,
    /// Ends slow. `1 − (1 − t)²`.
    EaseOut,
    /// Slow at both ends. The default, and what most UI motion wants.
    #[default]
    EaseInOut,
    /// Starts slower than [`Curve::EaseIn`]. `t³`.
    EaseInCubic,
    /// Ends slower than [`Curve::EaseOut`]. `1 − (1 − t)³`.
    ///
    /// The one to reach for when something is *arriving* — a menu, a scroll,
    /// a panel. The steep start reads as a response to the input and the long
    /// tail as the thing settling, which is why every shell uses it for
    /// exactly those.
    EaseOutCubic,
    /// Slow at both ends, more pronounced than [`Curve::EaseInOut`].
    EaseInOutCubic,
}

impl Curve {
    /// Maps linear progress onto eased progress. Both are in `[0, 1]`.
    pub fn eval(self, t: f32) -> f32 {
        let t = t.clamp(0.0, 1.0);
        match self {
            Curve::Linear => t,
            Curve::EaseIn => t * t,
            Curve::EaseOut => {
                let u = 1.0 - t;
                1.0 - u * u
            }
            Curve::EaseInOut => {
                if t < 0.5 {
                    2.0 * t * t
                } else {
                    let u = 1.0 - t;
                    1.0 - 2.0 * u * u
                }
            }
            Curve::EaseInCubic => t * t * t,
            Curve::EaseOutCubic => {
                let u = 1.0 - t;
                1.0 - u * u * u
            }
            Curve::EaseInOutCubic => {
                if t < 0.5 {
                    4.0 * t * t * t
                } else {
                    let u = 1.0 - t;
                    1.0 - 4.0 * u * u * u
                }
            }
        }
    }
}

/// A fixed-duration transition.
///
/// Use one only when the end time genuinely matters. Everything else wants a
/// [`Spring`], because a tween cannot be interrupted without either jumping or
/// finishing the wrong animation.
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct Tween {
    /// How long a full traversal takes.
    pub duration: Duration,
    /// The easing shape.
    pub curve: Curve,
}

impl Tween {
    /// A tween of `duration` with the default curve.
    pub const fn new(duration: Duration) -> Self {
        Self { duration, curve: Curve::EaseInOut }
    }

    /// Replaces the curve.
    #[must_use]
    pub const fn with_curve(mut self, curve: Curve) -> Self {
        self.curve = curve;
        self
    }
}

/// What advances a [`Motion`].
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum Drive {
    /// Physical, interruptible, no fixed end.
    Spring(Spring),
    /// Fixed duration, uninterruptible without a discontinuity in velocity.
    Tween(Tween),
}

impl Drive {
    /// The default spring.
    pub const SMOOTH: Self = Self::Spring(Spring::SMOOTH);
    /// A faster spring.
    pub const STIFF: Self = Self::Spring(Spring::STIFF);

    /// A tween of `duration` with an explicit curve.
    ///
    /// Prefer a [`Spring`] for anything a user can interrupt — a hover, a
    /// drag, a toggle. A tween is right when the *duration* is the point: a
    /// page transition that has to match a sibling animation, or a shuttle
    /// that has to loop on a known beat.
    pub const fn tween(duration: Duration, curve: Curve) -> Self {
        Self::Tween(Tween::new(duration).with_curve(curve))
    }

    /// A tween that starts fast and settles. What arriving things want.
    pub const fn ease_out(duration: Duration) -> Self {
        Self::tween(duration, Curve::EaseOutCubic)
    }

    /// A tween that starts slow and accelerates. What leaving things want.
    pub const fn ease_in(duration: Duration) -> Self {
        Self::tween(duration, Curve::EaseInCubic)
    }

    /// A tween that is slow at both ends.
    pub const fn ease_in_out(duration: Duration) -> Self {
        Self::tween(duration, Curve::EaseInOutCubic)
    }

    /// A tween with no easing at all, for something mechanical.
    pub const fn linear(duration: Duration) -> Self {
        Self::tween(duration, Curve::Linear)
    }
}

impl Default for Drive {
    fn default() -> Self {
        Self::SMOOTH
    }
}

impl From<Spring> for Drive {
    fn from(s: Spring) -> Self {
        Drive::Spring(s)
    }
}

impl From<Tween> for Drive {
    fn from(t: Tween) -> Self {
        Drive::Tween(t)
    }
}

// ------------------------------------------------------------------- motion

/// One animated value: where it is, how fast, and where it is going.
///
/// `(position, velocity, target)` rather than `(from, to, t)`. That single
/// choice is what makes [`Motion::retarget`] free of jumps — there is no `t` to
/// reset — and what lets a tween and a spring share one state machine, one
/// settle rule and one idea of when the loop may sleep.
///
/// `Copy`, no allocation, no `Drop`, and no handle back to the engine. It
/// cannot mark anything dirty because it does not know about anything; whoever
/// owns it decides what a change means.
#[derive(Copy, Clone, Debug)]
pub struct Motion<T: Animatable> {
    position: [f32; MAX_CHANNELS],
    velocity: [f32; MAX_CHANNELS],
    target: [f32; MAX_CHANNELS],
    /// Where the current tween traversal started. Unused by springs.
    origin: [f32; MAX_CHANNELS],
    /// How far into the current tween traversal. Unused by springs.
    elapsed: Duration,
    drive: Drive,
    settled: bool,
    _value: PhantomData<T>,
}

impl<T: Animatable> Motion<T> {
    /// At rest on `value`. Nothing moves until something retargets it.
    pub fn at(value: T, drive: Drive) -> Self {
        let mut channels = [0.0; MAX_CHANNELS];
        value.write_channels(&mut channels);
        Self {
            position: channels,
            velocity: [0.0; MAX_CHANNELS],
            target: channels,
            origin: channels,
            elapsed: Duration::ZERO,
            drive,
            settled: true,
            _value: PhantomData,
        }
    }

    /// Starts at `from` and immediately heads for `to`: an enter animation.
    pub fn from_to(from: T, to: T, drive: Drive) -> Self {
        let mut m = Self::at(from, drive);
        m.retarget(to);
        m
    }

    /// The current value.
    #[inline]
    pub fn value(&self) -> T {
        T::read_channels(&self.position)
    }

    /// Where it is heading.
    #[inline]
    pub fn target(&self) -> T {
        T::read_channels(&self.target)
    }

    /// Raw per-lane velocity, in interpolation space.
    ///
    /// Exposed for diagnostics and for handing momentum between motions; the
    /// lanes are this type's own space and are not meaningful without knowing
    /// its [`Animatable`] impl.
    #[inline]
    pub fn velocity_channels(&self) -> [f32; MAX_CHANNELS] {
        self.velocity
    }

    /// True when the value has arrived and is no longer moving.
    #[inline]
    pub const fn is_settled(&self) -> bool {
        self.settled
    }

    /// What advances it.
    #[inline]
    pub const fn drive(&self) -> Drive {
        self.drive
    }

    /// Swaps the drive mid-flight.
    ///
    /// Position and velocity are kept, so a spring taking over from a tween
    /// starts with the momentum the tween had built up.
    pub fn set_drive(&mut self, drive: Drive) {
        self.drive = drive;
        self.origin = self.position;
        self.elapsed = Duration::ZERO;
    }

    /// Points the motion somewhere new without touching where it is or how
    /// fast it is going.
    ///
    /// A no-op when the target is unchanged, which is what makes it safe — and
    /// free — to call on every frame from a rebuild.
    ///
    /// For a tween this also snapshots the origin from the current position and
    /// resets the traversal, so an interrupted tween restarts its curve from
    /// where the value actually is. Position stays continuous; velocity does
    /// not, which is the honest cost of a hard end time.
    pub fn retarget(&mut self, target: T) {
        let mut next = [0.0; MAX_CHANNELS];
        target.write_channels(&mut next);
        if next == self.target && !self.settled {
            return;
        }
        if next == self.target && self.settled && next == self.position {
            return;
        }
        self.target = next;
        self.origin = self.position;
        self.elapsed = Duration::ZERO;
        self.settled = self.arrived();
    }

    /// Teleports: position becomes `value`, velocity zero, settled.
    ///
    /// What a fader calls on mouse-down, so the cap does not lag the pointer
    /// through a spring it has no reason to travel.
    pub fn jump_to(&mut self, value: T) {
        let mut channels = [0.0; MAX_CHANNELS];
        value.write_channels(&mut channels);
        self.position = channels;
        self.target = channels;
        self.origin = channels;
        self.velocity = [0.0; MAX_CHANNELS];
        self.elapsed = Duration::ZERO;
        self.settled = true;
    }

    /// Integrates `dt` and returns the new value.
    ///
    /// Settles when, for **every** lane at once:
    ///
    /// 1. `|position − target| ≤ REST[lane]`, and
    /// 2. `|velocity| · SETTLE_WINDOW ≤ REST[lane]`.
    ///
    /// The second condition is not optional. An underdamped spring crosses its
    /// target at maximum velocity, so position alone would declare it settled
    /// at the exact instant it is moving fastest. Read together: the value is
    /// where it is going, and it is moving too slowly to travel a visible
    /// distance before the next frame even at 30 Hz.
    ///
    /// On settling it snaps *exactly* onto the target. The exact terminal value
    /// is load-bearing for the idle guarantee: without it there is always a
    /// residual few thousandths and nothing ever declares itself finished.
    pub fn step(&mut self, dt: Duration) -> T {
        if self.settled || dt.is_zero() {
            return self.value();
        }
        let dt_secs = dt.as_secs_f32();

        match self.drive {
            Drive::Spring(spring) => {
                for lane in 0..T::CHANNELS {
                    spring.step_channel(
                        &mut self.position[lane],
                        &mut self.velocity[lane],
                        self.target[lane],
                        dt_secs,
                    );
                }
            }
            Drive::Tween(tween) => {
                self.elapsed = self.elapsed.saturating_add(dt);
                let total = tween.duration.as_secs_f32();
                let t = if total <= 0.0 {
                    1.0
                } else {
                    (self.elapsed.as_secs_f32() / total).clamp(0.0, 1.0)
                };
                let eased = tween.curve.eval(t);
                for lane in 0..T::CHANNELS {
                    let next = self.origin[lane] + (self.target[lane] - self.origin[lane]) * eased;
                    // Velocity from the actual displacement rather than from
                    // the curve's derivative: it costs one subtraction, needs
                    // no closed form per curve, and is exactly what a spring
                    // taking over needs.
                    self.velocity[lane] = (next - self.position[lane]) / dt_secs;
                    self.position[lane] = next;
                }
                if t >= 1.0 {
                    self.snap();
                    return self.value();
                }
            }
        }

        if self.arrived() {
            self.snap();
        }
        self.value()
    }

    /// True when every lane is within `REST` and moving slower than `REST` per
    /// settle window.
    fn arrived(&self) -> bool {
        for lane in 0..T::CHANNELS {
            let rest = T::REST[lane];
            if (self.position[lane] - self.target[lane]).abs() > rest {
                return false;
            }
            if self.velocity[lane].abs() * SETTLE_WINDOW > rest {
                return false;
            }
        }
        true
    }

    /// Lands exactly on the target and stops.
    fn snap(&mut self) {
        self.position = self.target;
        self.velocity = [0.0; MAX_CHANNELS];
        self.settled = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::rect;
    use crate::unit::px;

    const FRAME: Duration = Duration::from_micros(16_667);

    /// Runs until settled or the budget runs out. Returns frames taken.
    fn run<T: Animatable>(m: &mut Motion<T>, budget: usize) -> usize {
        for i in 1..=budget {
            m.step(FRAME);
            if m.is_settled() {
                return i;
            }
        }
        budget + 1
    }

    // ------------------------------------------------------------- physics

    #[test]
    fn a_spring_reaches_its_target_and_declares_itself_settled() {
        let mut m = Motion::at(0.0f32, Drive::SMOOTH);
        m.retarget(1.0);
        let frames = run(&mut m, 240);
        assert!(frames < 60, "SMOOTH should settle well under a second, took {frames} frames");
        assert_eq!(m.value(), 1.0, "must land exactly on target, not near it");
        assert!(m.is_settled());
    }

    #[test]
    fn a_settled_motion_costs_nothing_and_stays_put() {
        // The idle guarantee depends on this: a settled motion must not move,
        // must not unsettle itself, and must not need another frame.
        let mut m = Motion::at(0.0f32, Drive::SMOOTH);
        m.retarget(1.0);
        run(&mut m, 240);
        for _ in 0..100 {
            assert_eq!(m.step(FRAME), 1.0);
            assert!(m.is_settled());
        }
    }

    #[test]
    fn the_analytic_step_is_frame_rate_independent() {
        // The whole reason the step is closed-form. One 32 ms step must give
        // the same answer as two 16 ms steps; Euler would not.
        let spring = Spring::SMOOTH;
        let (mut p1, mut v1) = (0.0f32, 0.0f32);
        spring.step_channel(&mut p1, &mut v1, 1.0, 0.032);

        let (mut p2, mut v2) = (0.0f32, 0.0f32);
        spring.step_channel(&mut p2, &mut v2, 1.0, 0.016);
        spring.step_channel(&mut p2, &mut v2, 1.0, 0.016);

        assert!((p1 - p2).abs() < 1.0e-6, "position {p1} vs {p2}");
        assert!((v1 - v2).abs() < 1.0e-4, "velocity {v1} vs {v2}");
    }

    #[test]
    fn all_three_damping_regimes_converge() {
        // Under-, critically and over-damped are three different closed forms,
        // and the critical band exists because the other two divide by zero
        // there. Each must still arrive.
        for (name, zeta) in [("under", 0.4f32), ("critical", 1.0), ("over", 2.5)] {
            let spring = Spring::critical(1024.0).with_damping_ratio(zeta);
            let mut m = Motion::at(0.0f32, Drive::Spring(spring));
            m.retarget(1.0);
            let frames = run(&mut m, 600);
            assert!(frames <= 600, "{name} damping never settled");
            assert_eq!(m.value(), 1.0, "{name} damping did not land on target");
        }
    }

    #[test]
    fn an_underdamped_spring_overshoots_and_a_critical_one_does_not() {
        let mut over = Motion::at(0.0f32, Drive::Spring(Spring::BOUNCY));
        over.retarget(1.0);
        let mut peak = 0.0f32;
        for _ in 0..120 {
            peak = peak.max(over.step(FRAME));
        }
        assert!(peak > 1.02, "BOUNCY should overshoot, peaked at {peak}");

        let mut crit = Motion::at(0.0f32, Drive::SMOOTH);
        crit.retarget(1.0);
        let mut peak = 0.0f32;
        for _ in 0..120 {
            peak = peak.max(crit.step(FRAME));
        }
        assert!(peak <= 1.0 + 1.0e-4, "SMOOTH must not overshoot, peaked at {peak}");
    }

    #[test]
    fn a_huge_step_lands_on_target_rather_than_exploding() {
        // Euler gains energy here. The closed form underflows to zero and
        // arrives, which is what makes the timeline's clamp a UX choice rather
        // than a numerical necessity.
        let mut m = Motion::at(0.0f32, Drive::SMOOTH);
        m.retarget(1.0);
        let v = m.step(Duration::from_secs(600));
        assert_eq!(v, 1.0);
        assert!(m.is_settled());
    }

    #[test]
    fn settling_needs_low_velocity_not_just_proximity() {
        // An underdamped spring crosses its target at maximum speed. A settle
        // test on position alone would stop it at that exact instant.
        let spring = Spring::critical(1024.0).with_damping_ratio(0.35);
        let mut m = Motion::at(0.0f32, Drive::Spring(spring));
        m.retarget(1.0);
        let mut crossed_while_moving = false;
        for _ in 0..600 {
            let before = m.value();
            let after = m.step(FRAME);
            if before < 1.0 && after >= 1.0 && !m.is_settled() {
                crossed_while_moving = true;
            }
            if m.is_settled() {
                break;
            }
        }
        assert!(crossed_while_moving, "the test never exercised a fast crossing");
        assert!(m.is_settled());
        assert_eq!(m.value(), 1.0);
    }

    // -------------------------------------------------------- interruption

    #[test]
    fn retargeting_keeps_position_and_velocity() {
        // The property the whole design exists for: no jump, ever.
        let mut m = Motion::at(0.0f32, Drive::SMOOTH);
        m.retarget(1.0);
        for _ in 0..6 {
            m.step(FRAME);
        }
        let (p, v) = (m.value(), m.velocity_channels());
        m.retarget(0.0);
        assert_eq!(m.value(), p, "position moved on retarget");
        assert_eq!(m.velocity_channels(), v, "velocity was reset on retarget");
    }

    #[test]
    fn an_interrupted_spring_reverses_rather_than_restarting() {
        let mut m = Motion::at(0.0f32, Drive::SMOOTH);
        m.retarget(1.0);
        for _ in 0..6 {
            m.step(FRAME);
        }
        let midway = m.value();
        assert!(midway > 0.0 && midway < 1.0, "not actually midway: {midway}");
        m.retarget(0.0);
        // It carries momentum, so it may still creep forward for a frame or
        // two before turning round. What it must never do is teleport.
        let next = m.step(FRAME);
        assert!((next - midway).abs() < 0.2, "jumped from {midway} to {next}");
        run(&mut m, 240);
        assert_eq!(m.value(), 0.0);
    }

    #[test]
    fn retargeting_to_the_same_value_does_not_restart_anything() {
        // Called every frame from a rebuild, so this must be free.
        let mut m = Motion::at(0.0f32, Drive::SMOOTH);
        m.retarget(1.0);
        for _ in 0..6 {
            m.step(FRAME);
        }
        let before = (m.value(), m.velocity_channels());
        for _ in 0..10 {
            m.retarget(1.0);
        }
        assert_eq!((m.value(), m.velocity_channels()), before);
    }

    #[test]
    fn retargeting_a_settled_motion_wakes_it_up() {
        let mut m = Motion::at(0.0f32, Drive::SMOOTH);
        assert!(m.is_settled());
        m.retarget(1.0);
        assert!(!m.is_settled(), "a new target must un-settle the motion");
    }

    #[test]
    fn jumping_lands_immediately_and_stops() {
        let mut m = Motion::at(0.0f32, Drive::SMOOTH);
        m.retarget(1.0);
        m.step(FRAME);
        m.jump_to(0.25);
        assert_eq!(m.value(), 0.25);
        assert!(m.is_settled());
        assert_eq!(m.velocity_channels(), [0.0; MAX_CHANNELS]);
    }

    #[test]
    fn swapping_a_tween_for_a_spring_keeps_the_momentum() {
        let mut m = Motion::at(0.0f32, Drive::Tween(Tween::new(Duration::from_millis(400))));
        m.retarget(1.0);
        for _ in 0..8 {
            m.step(FRAME);
        }
        let (p, v) = (m.value(), m.velocity_channels());
        m.set_drive(Drive::SMOOTH);
        assert_eq!(m.value(), p);
        assert_eq!(m.velocity_channels(), v, "handover must not drop the momentum");
    }

    // -------------------------------------------------------------- tweens

    #[test]
    fn a_tween_finishes_at_its_duration() {
        // The one thing a tween buys over a spring: a hard end time.
        let mut m = Motion::at(0.0f32, Drive::Tween(Tween::new(Duration::from_millis(100))));
        m.retarget(1.0);
        for _ in 0..5 {
            m.step(Duration::from_millis(20));
        }
        assert!(m.is_settled());
        assert_eq!(m.value(), 1.0);
    }

    #[test]
    fn a_zero_duration_tween_arrives_on_its_first_step() {
        let mut m = Motion::at(0.0f32, Drive::Tween(Tween::new(Duration::ZERO)));
        m.retarget(1.0);
        assert_eq!(m.step(FRAME), 1.0);
        assert!(m.is_settled());
    }

    #[test]
    fn every_curve_runs_from_zero_to_one_monotonically() {
        for curve in [Curve::Linear, Curve::EaseIn, Curve::EaseOut, Curve::EaseInOut] {
            assert_eq!(curve.eval(0.0), 0.0, "{curve:?}");
            assert!((curve.eval(1.0) - 1.0).abs() < 1.0e-6, "{curve:?}");
            let mut previous = -1.0;
            for i in 0..=64 {
                let v = curve.eval(i as f32 / 64.0);
                assert!(v >= previous - 1.0e-6, "{curve:?} went backwards at {i}");
                previous = v;
            }
        }
        // Out of range clamps rather than extrapolating.
        assert_eq!(Curve::EaseInOut.eval(-1.0), 0.0);
        assert_eq!(Curve::EaseInOut.eval(2.0), 1.0);
    }

    // ---------------------------------------------------------- value types

    #[test]
    fn a_colour_spring_agrees_with_color_lerp() {
        // Two colour paths that can drift apart is worse than one that is
        // slower, so the spring's interpolation space must match `Color::lerp`.
        let (a, b) = (Color::hex(0xFF0000), Color::hex(0x00FF00));
        let mut ch_a = [0.0; MAX_CHANNELS];
        let mut ch_b = [0.0; MAX_CHANNELS];
        a.write_channels(&mut ch_a);
        b.write_channels(&mut ch_b);

        for t in [0.0f32, 0.25, 0.5, 0.75, 1.0] {
            let mut mixed = [0.0; MAX_CHANNELS];
            for i in 0..MAX_CHANNELS {
                mixed[i] = ch_a[i] + (ch_b[i] - ch_a[i]) * t;
            }
            let through_channels = Color::read_channels(&mixed);
            let through_lerp = Color::lerp(a, b, t);
            for (x, y) in [
                (through_channels.r, through_lerp.r),
                (through_channels.g, through_lerp.g),
                (through_channels.b, through_lerp.b),
                (through_channels.a, through_lerp.a),
            ] {
                assert!((x - y).abs() < 1.0e-5, "{through_channels:?} vs {through_lerp:?} at {t}");
            }
        }
    }

    #[test]
    fn an_overshooting_colour_is_clamped_into_the_gamut_on_read() {
        // Stated consequence: a bouncy colour saturates rather than
        // overshooting, so the value read back is the value the settle test
        // compares against.
        let lanes = [1.4f32, -0.3, 0.5, 1.2];
        let c = Color::read_channels(&lanes);
        for v in [c.r, c.g, c.b, c.a] {
            assert!((0.0..=1.0).contains(&v), "{c:?} left the gamut");
            assert!(v.is_finite());
        }
    }

    #[test]
    fn a_colour_spring_settles_exactly_on_its_target() {
        let mut m = Motion::at(Color::hex(0x101418), Drive::SMOOTH);
        let target = Color::hex(0x3D8BFD);
        m.retarget(target);
        let frames = run(&mut m, 600);
        assert!(frames <= 600, "colour spring never settled");
        let v = m.value();
        assert!((v.r - target.r).abs() < 1.0e-4, "{v:?} vs {target:?}");
        assert!((v.g - target.g).abs() < 1.0e-4);
        assert!((v.b - target.b).abs() < 1.0e-4);
    }

    #[test]
    fn every_animatable_type_round_trips_through_its_channels() {
        fn check<T: Animatable + core::fmt::Debug + PartialEq>(value: T) {
            let mut ch = [0.0; MAX_CHANNELS];
            value.write_channels(&mut ch);
            assert_eq!(T::read_channels(&ch), value, "round trip lost information");
            assert!(T::CHANNELS <= MAX_CHANNELS);
        }
        check(1.5f32);
        check(px(12.5));
        check(Point::new(px(1.0), px(2.0)));
        check(Size::new(px(3.0), px(4.0)));
        check(rect(px(1.0), px(2.0), px(3.0), px(4.0)));
        check(Corners::all(px(5.0)));
        check(Edges::all(px(6.0)));
    }

    #[test]
    fn a_rect_interpolates_as_origin_and_size_so_it_cannot_invert() {
        // min+max crosses over when a rect collapses; origin+size does not.
        let mut m = Motion::at(rect(px(0.0), px(0.0), px(100.0), px(40.0)), Drive::SMOOTH);
        m.retarget(rect(px(200.0), px(0.0), px(0.0), px(40.0)));
        for _ in 0..600 {
            let r = m.step(FRAME);
            assert!(r.size.width.get() >= -0.05, "width went negative: {r:?}");
            if m.is_settled() {
                break;
            }
        }
        assert!(m.is_settled());
    }

    // ------------------------------------------------------------- springs

    #[test]
    fn the_named_springs_have_the_damping_ratios_they_claim() {
        assert!((Spring::SMOOTH.damping_ratio() - 1.0).abs() < 1.0e-3, "SMOOTH must be critical");
        assert!((Spring::STIFF.damping_ratio() - 1.0).abs() < 1.0e-3, "STIFF must be critical");
        assert!((Spring::SNAPPY.damping_ratio() - 0.8).abs() < 1.0e-2);
        assert!((Spring::BOUNCY.damping_ratio() - 0.55).abs() < 1.0e-2);
        assert!(Spring::STIFF.angular_frequency() > Spring::SMOOTH.angular_frequency());
    }

    #[test]
    fn a_settling_time_spring_settles_near_the_time_it_was_asked_for() {
        let want = Duration::from_millis(200);
        let spring = Spring::with_settling_time(want);
        assert!((spring.damping_ratio() - 1.0).abs() < 1.0e-3, "must be critical");
        let mut m = Motion::at(0.0f32, Drive::Spring(spring));
        m.retarget(1.0);
        let frames = run(&mut m, 600);
        let elapsed = FRAME.as_secs_f32() * frames as f32;
        // A duration for the eye, not a guarantee: the last one per cent
        // depends on the displacement and the type's REST.
        assert!(elapsed > 0.1 && elapsed < 0.5, "settled in {elapsed}s, asked for 0.2s");
    }

    #[test]
    fn the_damping_ratio_is_clamped_away_from_never_settling() {
        let barely = Spring::critical(1024.0).with_damping_ratio(0.0);
        assert!(barely.damping_ratio() >= 0.05, "an undamped spring would never return to idle");
        let heavy = Spring::critical(1024.0).with_damping_ratio(100.0);
        assert!(heavy.damping_ratio() <= 4.0);
    }

    #[test]
    fn a_spring_with_no_stiffness_coasts_instead_of_producing_nan() {
        let dead = Spring { mass: 1.0, stiffness: 0.0, damping: 0.0 };
        let (mut p, mut v) = (0.0f32, 2.0f32);
        dead.step_channel(&mut p, &mut v, 1.0, 0.5);
        assert!(p.is_finite() && v.is_finite());
        assert_eq!(p, 1.0, "coasted at constant velocity");
    }

    #[test]
    fn a_zero_or_negative_step_changes_nothing() {
        let spring = Spring::SMOOTH;
        let (mut p, mut v) = (0.5f32, 1.0f32);
        spring.step_channel(&mut p, &mut v, 1.0, 0.0);
        assert_eq!((p, v), (0.5, 1.0));
        spring.step_channel(&mut p, &mut v, 1.0, -1.0);
        assert_eq!((p, v), (0.5, 1.0));
    }
}
