//! Keyboard input: logical keys, physical key positions and modifier state.
//!
//! SphereKit separates the three things a keystroke actually carries, because
//! plug-in UIs need all three and conflating them is the classic source of
//! "works on my QWERTY keyboard" bugs:
//!
//! * [`Key`] is the *logical* key: what the current layout says the user typed.
//!   `Key::Character("q")` on AZERTY comes from the physical `KeyA` position.
//!   Bind menu accelerators to this.
//! * [`PhysicalKey`] is *where the key sits*, independent of layout. Bind
//!   transport controls, piano-roll keys and WASD-style navigation to this so
//!   the shape of the binding survives a layout change.
//! * [`Modifiers`] is the chord state. [`Modifiers::COMMAND`] resolves to the
//!   platform's primary accelerator modifier, which is the one thing every
//!   cross-platform audio plug-in gets wrong at least once.
//!
//! The variant names of [`NamedKey`] and [`KeyCode`] follow the W3C UI Events
//! `key`/`code` value names, so a binding can be serialised as a string and
//! still mean the same thing on another platform.

use core::fmt;
use core::ops::Deref;

// ---------------------------------------------------------------------------
// Modifiers
// ---------------------------------------------------------------------------

bitflags::bitflags! {
    /// The set of modifier keys held when an input event was produced.
    ///
    /// Left and right modifiers are deliberately collapsed: no shipping audio
    /// UI distinguishes them for accelerators, and the platforms disagree about
    /// how to report the difference.
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Hash)]
    pub struct Modifiers: u8 {
        /// Either <kbd>Shift</kbd> key.
        const SHIFT = 1 << 0;
        /// Either <kbd>Ctrl</kbd> key.
        const CTRL = 1 << 1;
        /// Either <kbd>Alt</kbd> key, which is <kbd>Option</kbd> on macOS.
        const ALT = 1 << 2;
        /// Either <kbd>Super</kbd> key: <kbd>Windows</kbd> on Windows,
        /// <kbd>Command</kbd> on macOS, <kbd>Meta</kbd> on X11 and Wayland.
        const SUPER = 1 << 3;
    }
}

/// Which physical modifier acts as the accelerator modifier on a given target.
///
/// Kept as a free function taking a flag rather than reading `cfg!` inline so
/// that both branches can be unit-tested from any host platform.
const fn command_flag(target_is_macos: bool) -> Modifiers {
    if target_is_macos { Modifiers::SUPER } else { Modifiers::CTRL }
}

impl Modifiers {
    /// The platform's primary accelerator modifier.
    ///
    /// <kbd>Command</kbd> (Super) on macOS, <kbd>Ctrl</kbd> everywhere else.
    /// Write shortcuts against this rather than hard-coding `CTRL`: a DAW user
    /// on macOS expects <kbd>Cmd</kbd>+<kbd>Z</kbd>, and `CTRL`+<kbd>Z</kbd>
    /// there collides with the system's emacs-style editing bindings.
    pub const COMMAND: Self = command_flag(cfg!(target_os = "macos"));

    /// True when either <kbd>Shift</kbd> is held.
    #[inline]
    pub const fn shift(self) -> bool {
        self.bits() & Self::SHIFT.bits() != 0
    }

    /// True when either <kbd>Ctrl</kbd> is held.
    #[inline]
    pub const fn ctrl(self) -> bool {
        self.bits() & Self::CTRL.bits() != 0
    }

    /// True when either <kbd>Alt</kbd> / <kbd>Option</kbd> is held.
    #[inline]
    pub const fn alt(self) -> bool {
        self.bits() & Self::ALT.bits() != 0
    }

    /// True when either <kbd>Super</kbd> / <kbd>Command</kbd> / <kbd>Win</kbd>
    /// is held.
    ///
    /// Named `super_key` because `super` is a reserved word.
    #[inline]
    pub const fn super_key(self) -> bool {
        self.bits() & Self::SUPER.bits() != 0
    }

    /// True when the platform's accelerator modifier ([`Modifiers::COMMAND`])
    /// is held.
    #[inline]
    pub const fn command(self) -> bool {
        self.bits() & Self::COMMAND.bits() != 0
    }

    /// True when exactly the given modifiers are held and nothing else.
    ///
    /// Accelerator dispatch must use this rather than [`Modifiers::contains`],
    /// or <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>S</kbd> would also fire the
    /// <kbd>Cmd</kbd>+<kbd>S</kbd> handler.
    #[inline]
    pub const fn is_exactly(self, other: Self) -> bool {
        self.bits() == other.bits()
    }
}

// ---------------------------------------------------------------------------
// KeyText
// ---------------------------------------------------------------------------

/// Inline capacity of [`KeyText`], in bytes.
///
/// Every key that produces text produces one grapheme cluster; 15 bytes covers
/// every single-scalar cluster and almost every combining sequence, so the heap
/// path is effectively unreachable during normal typing.
const KEY_TEXT_INLINE: usize = 15;

/// The text a character key produced, stored inline when it is short.
///
/// Key events are hot: on a held key they arrive at the OS repeat rate, and an
/// allocation per repeat is exactly the kind of jitter that shows up as a
/// stutter in a plug-in editor sharing a thread with the host's UI. This is a
/// small-string type in the shape of `SmolStr`, without the dependency.
#[derive(Clone)]
pub struct KeyText(KeyTextRepr);

#[derive(Clone)]
enum KeyTextRepr {
    /// Text short enough to live in the struct itself.
    Inline {
        /// UTF-8 bytes; only the first `len` are meaningful.
        bytes: [u8; KEY_TEXT_INLINE],
        /// Number of meaningful bytes in `bytes`.
        len: u8,
    },
    /// Text too long to inline. Reachable only for exotic grapheme clusters.
    Heap(Box<str>),
}

impl KeyText {
    /// An empty key text.
    pub const EMPTY: Self = Self(KeyTextRepr::Inline { bytes: [0; KEY_TEXT_INLINE], len: 0 });

    /// Builds a key text, inlining it when it fits.
    pub fn new(s: &str) -> Self {
        if s.len() <= KEY_TEXT_INLINE {
            let mut bytes = [0u8; KEY_TEXT_INLINE];
            bytes[..s.len()].copy_from_slice(s.as_bytes());
            Self(KeyTextRepr::Inline { bytes, len: s.len() as u8 })
        } else {
            Self(KeyTextRepr::Heap(s.into()))
        }
    }

    /// The text as a string slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        match &self.0 {
            KeyTextRepr::Inline { bytes, len } => {
                // SAFETY-free path: the only constructor copies from a `&str`,
                // so the prefix is always valid UTF-8. `from_utf8` is checked
                // anyway; the branch is never taken and costs a length scan on
                // at most 15 bytes.
                core::str::from_utf8(&bytes[..*len as usize]).unwrap_or("")
            }
            KeyTextRepr::Heap(s) => s,
        }
    }

    /// Length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.as_str().len()
    }

    /// True when no text was produced.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// True when the text is held inline rather than on the heap.
    ///
    /// Exposed so a test can prove the allocation-free path is actually taken.
    #[inline]
    pub fn is_inline(&self) -> bool {
        matches!(self.0, KeyTextRepr::Inline { .. })
    }

    /// The single `char` this text represents, when it is exactly one.
    ///
    /// Shortcut matching wants a `char`; combining sequences yield `None`.
    #[inline]
    pub fn as_char(&self) -> Option<char> {
        let mut it = self.as_str().chars();
        match (it.next(), it.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    }
}

impl Deref for KeyText {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl PartialEq for KeyText {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}
impl Eq for KeyText {}

impl PartialOrd for KeyText {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for KeyText {
    #[inline]
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl core::hash::Hash for KeyText {
    // Hashing the string rather than the representation is what keeps `Hash`
    // consistent with `Eq` across the inline/heap split.
    #[inline]
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialEq<str> for KeyText {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}
impl PartialEq<&str> for KeyText {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl From<&str> for KeyText {
    #[inline]
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}
impl From<char> for KeyText {
    fn from(c: char) -> Self {
        let mut buf = [0u8; 4];
        Self::new(c.encode_utf8(&mut buf))
    }
}

impl fmt::Debug for KeyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}
impl fmt::Display for KeyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// NamedKey
// ---------------------------------------------------------------------------

/// Declares [`NamedKey`] from a bare list of W3C `key` value names.
macro_rules! declare_named_keys {
    ($($name:ident),* $(,)?) => {
        /// A key with a name rather than a character: everything the user can
        /// press that does not produce text.
        ///
        /// Variant names match the W3C UI Events `key` values, so
        /// `NamedKey::ArrowUp` prints and parses as `"ArrowUp"`.
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        #[non_exhaustive]
        pub enum NamedKey {
            $(
                #[doc = concat!("The `", stringify!($name), "` key (W3C UI Events `key` value).")]
                $name,
            )*
            /// A function key, numbered from 1. Values above 24 exist only on
            /// exotic hardware but are passed through rather than dropped.
            F(u8),
        }

        impl fmt::Display for NamedKey {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(NamedKey::$name => f.write_str(stringify!($name)),)*
                    NamedKey::F(n) => write!(f, "F{n}"),
                }
            }
        }
    };
}

/// The single source of truth for [`NamedKey`]'s variant list.
///
/// Invoked once to declare the enum and once by the backend to build its
/// translation table, so the two can never drift apart.
macro_rules! named_key_names {
    ($cb:ident) => {
        $cb! {
            Alt, AltGraph, CapsLock, Control, Fn, FnLock, NumLock, ScrollLock, Shift, Symbol,
            Meta, Hyper, Super,
            Enter, Tab, Space, Backspace, Delete, Insert, Clear,
            ArrowDown, ArrowLeft, ArrowRight, ArrowUp, End, Home, PageDown, PageUp,
            Copy, Cut, Paste, Undo, Redo, Select, Find, Again,
            Escape, ContextMenu, Help, Pause, Play, Cancel, Execute, PrintScreen, Props, Attn,
            ZoomIn, ZoomOut,
            MediaPlayPause, MediaStop, MediaTrackNext, MediaTrackPrevious, MediaRecord,
            MediaRewind, MediaFastForward, MediaPause, MediaPlay,
            AudioVolumeUp, AudioVolumeDown, AudioVolumeMute,
            Convert, NonConvert, ModeChange, Process, Compose, CodeInput, GroupNext, GroupPrevious,
            HangulMode, KanaMode, KanjiMode, Katakana, Hiragana, Romaji, Alphanumeric,
            Zenkaku, Hankaku, ZenkakuHankaku, Eisu,
        }
    };
}
#[cfg(feature = "winit-backend")]
pub(crate) use named_key_names;

named_key_names!(declare_named_keys);

impl NamedKey {
    /// True when this key is itself a modifier, and so should not be treated as
    /// the "main" key of a chord.
    pub const fn is_modifier(self) -> bool {
        matches!(
            self,
            NamedKey::Alt
                | NamedKey::AltGraph
                | NamedKey::CapsLock
                | NamedKey::Control
                | NamedKey::Fn
                | NamedKey::FnLock
                | NamedKey::NumLock
                | NamedKey::ScrollLock
                | NamedKey::Shift
                | NamedKey::Symbol
                | NamedKey::Meta
                | NamedKey::Hyper
                | NamedKey::Super
        )
    }

    /// True when this key moves the caret or selection.
    pub const fn is_navigation(self) -> bool {
        matches!(
            self,
            NamedKey::ArrowDown
                | NamedKey::ArrowLeft
                | NamedKey::ArrowRight
                | NamedKey::ArrowUp
                | NamedKey::End
                | NamedKey::Home
                | NamedKey::PageDown
                | NamedKey::PageUp
        )
    }
}

// ---------------------------------------------------------------------------
// Key
// ---------------------------------------------------------------------------

/// A logical key: what the active keyboard layout says was pressed.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[non_exhaustive]
pub enum Key {
    /// A key with a name rather than a character.
    Named(NamedKey),
    /// A key that maps to text under the current layout, such as `"a"` or
    /// `"@"`. Note this is the *layout* interpretation: <kbd>Shift</kbd>+`2`
    /// on a US layout arrives as `"@"`.
    Character(KeyText),
    /// A dead key, carrying its combining character when the platform reports
    /// one. Dead keys must not be treated as text: the composed result arrives
    /// later, through the IME path.
    Dead(Option<char>),
    /// The platform reported a key SphereKit has no name for. The physical
    /// position is still available on the event, so rebinding still works.
    Unidentified,
}

impl Key {
    /// Builds a character key from a string slice.
    #[inline]
    pub fn character(s: &str) -> Self {
        Key::Character(KeyText::new(s))
    }

    /// The character text of this key, if it has any.
    #[inline]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Key::Character(t) => Some(t.as_str()),
            _ => None,
        }
    }

    /// The named key, if this is one.
    #[inline]
    pub const fn named(&self) -> Option<NamedKey> {
        match self {
            Key::Named(n) => Some(*n),
            _ => None,
        }
    }

    /// True when this key is a modifier and therefore never the main key of an
    /// accelerator.
    #[inline]
    pub const fn is_modifier(&self) -> bool {
        matches!(self, Key::Named(n) if n.is_modifier())
    }
}

impl From<NamedKey> for Key {
    #[inline]
    fn from(n: NamedKey) -> Self {
        Key::Named(n)
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Key::Named(n) => fmt::Display::fmt(n, f),
            Key::Character(t) => f.write_str(t.as_str()),
            Key::Dead(Some(c)) => write!(f, "Dead({c})"),
            Key::Dead(None) => f.write_str("Dead"),
            Key::Unidentified => f.write_str("Unidentified"),
        }
    }
}

// ---------------------------------------------------------------------------
// Physical keys
// ---------------------------------------------------------------------------

/// A raw platform key identifier, kept for keys SphereKit cannot name.
///
/// A rebindable control surface must be able to bind *any* key, including ones
/// no abstraction knows about, so the raw value is preserved rather than
/// discarded. The value is only comparable against other scancodes from the
/// same platform.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
#[repr(transparent)]
pub struct Scancode(u32);

impl Scancode {
    /// The platform reported no usable raw value.
    ///
    /// Zero is used as the sentinel because no platform SphereKit targets reports
    /// scancode 0 for a real key.
    pub const UNIDENTIFIED: Self = Self(0);

    /// Wraps a raw platform value.
    #[inline]
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw platform value.
    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// True when a real raw value is present.
    #[inline]
    pub const fn is_identified(self) -> bool {
        self.0 != 0
    }
}

/// Declares [`KeyCode`] from a bare list of W3C `code` value names.
macro_rules! declare_key_codes {
    ($($name:ident),* $(,)?) => {
        /// A physical key position, independent of the active layout.
        ///
        /// Variant names match the W3C UI Events `code` values. `KeyA` means
        /// "the key where <kbd>A</kbd> sits on a US layout", which is
        /// <kbd>Q</kbd> on AZERTY.
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
        #[non_exhaustive]
        pub enum KeyCode {
            $(
                #[doc = concat!("The `", stringify!($name), "` key position (W3C UI Events `code` value).")]
                $name,
            )*
            /// A function key position, numbered from 1.
            F(u8),
        }

        impl fmt::Display for KeyCode {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                match self {
                    $(KeyCode::$name => f.write_str(stringify!($name)),)*
                    KeyCode::F(n) => write!(f, "F{n}"),
                }
            }
        }
    };
}

/// The single source of truth for [`KeyCode`]'s variant list.
macro_rules! key_code_names {
    ($cb:ident) => {
        $cb! {
            Backquote, Backslash, BracketLeft, BracketRight, Comma,
            Digit0, Digit1, Digit2, Digit3, Digit4, Digit5, Digit6, Digit7, Digit8, Digit9,
            Equal, IntlBackslash, IntlRo, IntlYen,
            KeyA, KeyB, KeyC, KeyD, KeyE, KeyF, KeyG, KeyH, KeyI, KeyJ, KeyK, KeyL, KeyM,
            KeyN, KeyO, KeyP, KeyQ, KeyR, KeyS, KeyT, KeyU, KeyV, KeyW, KeyX, KeyY, KeyZ,
            Minus, Period, Quote, Semicolon, Slash,
            AltLeft, AltRight, Backspace, CapsLock, ContextMenu, ControlLeft, ControlRight,
            Enter, SuperLeft, SuperRight, ShiftLeft, ShiftRight, Space, Tab,
            Convert, KanaMode, NonConvert,
            Delete, End, Help, Home, Insert, PageDown, PageUp,
            ArrowDown, ArrowLeft, ArrowRight, ArrowUp,
            NumLock, Numpad0, Numpad1, Numpad2, Numpad3, Numpad4, Numpad5, Numpad6, Numpad7,
            Numpad8, Numpad9, NumpadAdd, NumpadComma, NumpadDecimal, NumpadDivide, NumpadEnter,
            NumpadEqual, NumpadMultiply, NumpadParenLeft, NumpadParenRight, NumpadSubtract,
            Escape, Fn, FnLock, PrintScreen, ScrollLock, Pause,
            AudioVolumeDown, AudioVolumeMute, AudioVolumeUp,
            MediaPlayPause, MediaStop, MediaTrackNext, MediaTrackPrevious,
            Again, Copy, Cut, Find, Open, Paste, Props, Select, Undo,
        }
    };
}
#[cfg(feature = "winit-backend")]
pub(crate) use key_code_names;

key_code_names!(declare_key_codes);

impl KeyCode {
    /// True when this position is on the numeric keypad.
    ///
    /// Numeric entry fields in a DAW usually want <kbd>Numpad Enter</kbd> to
    /// commit and the main <kbd>Enter</kbd> to do something else, so the
    /// distinction has to survive to the widget layer.
    pub const fn is_numpad(self) -> bool {
        matches!(
            self,
            KeyCode::NumLock
                | KeyCode::Numpad0
                | KeyCode::Numpad1
                | KeyCode::Numpad2
                | KeyCode::Numpad3
                | KeyCode::Numpad4
                | KeyCode::Numpad5
                | KeyCode::Numpad6
                | KeyCode::Numpad7
                | KeyCode::Numpad8
                | KeyCode::Numpad9
                | KeyCode::NumpadAdd
                | KeyCode::NumpadComma
                | KeyCode::NumpadDecimal
                | KeyCode::NumpadDivide
                | KeyCode::NumpadEnter
                | KeyCode::NumpadEqual
                | KeyCode::NumpadMultiply
                | KeyCode::NumpadParenLeft
                | KeyCode::NumpadParenRight
                | KeyCode::NumpadSubtract
        )
    }
}

/// A physical key, by position.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum PhysicalKey {
    /// A position SphereKit has a name for.
    Code(KeyCode),
    /// A position SphereKit cannot name, carrying the raw platform value so that
    /// press and release still match and the key stays rebindable.
    Unidentified(Scancode),
}

impl PhysicalKey {
    /// The named position, if there is one.
    #[inline]
    pub const fn code(self) -> Option<KeyCode> {
        match self {
            PhysicalKey::Code(c) => Some(c),
            PhysicalKey::Unidentified(_) => None,
        }
    }
}

impl From<KeyCode> for PhysicalKey {
    #[inline]
    fn from(c: KeyCode) -> Self {
        PhysicalKey::Code(c)
    }
}

impl fmt::Display for PhysicalKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PhysicalKey::Code(c) => fmt::Display::fmt(c, f),
            PhysicalKey::Unidentified(s) => write!(f, "Scancode({})", s.get()),
        }
    }
}

/// Where on the keyboard a key with a duplicated name lives.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub enum KeyLocation {
    /// The key has only one position.
    #[default]
    Standard,
    /// The left-hand instance of a paired key.
    Left,
    /// The right-hand instance of a paired key.
    Right,
    /// The numeric keypad instance.
    Numpad,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn hash_of<T: Hash>(v: &T) -> u64 {
        let mut h = DefaultHasher::new();
        v.hash(&mut h);
        h.finish()
    }

    #[test]
    fn command_maps_to_super_on_macos_and_ctrl_elsewhere() {
        // The whole point of the helper: both branches are checked from any host.
        assert_eq!(command_flag(true), Modifiers::SUPER);
        assert_eq!(command_flag(false), Modifiers::CTRL);
        assert_eq!(Modifiers::COMMAND, command_flag(cfg!(target_os = "macos")));
    }

    #[test]
    fn command_predicate_follows_the_platform() {
        let sup = Modifiers::SUPER;
        let ctrl = Modifiers::CTRL;
        if cfg!(target_os = "macos") {
            assert!(sup.command());
            assert!(!ctrl.command());
        } else {
            assert!(ctrl.command());
            assert!(!sup.command());
        }
    }

    #[test]
    fn modifier_predicates_are_independent() {
        let m = Modifiers::SHIFT | Modifiers::ALT;
        assert!(m.shift());
        assert!(m.alt());
        assert!(!m.ctrl());
        assert!(!m.super_key());
        assert!(Modifiers::empty().is_empty());
    }

    #[test]
    fn exact_match_rejects_supersets() {
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        // Cmd+Shift+S must not fire a Cmd+S accelerator.
        assert!(cmd_shift.contains(Modifiers::COMMAND));
        assert!(!cmd_shift.is_exactly(Modifiers::COMMAND));
        assert!(Modifiers::COMMAND.is_exactly(Modifiers::COMMAND));
    }

    #[test]
    fn key_text_inlines_short_strings() {
        let t = KeyText::new("a");
        assert!(t.is_inline());
        assert_eq!(t.as_str(), "a");
        assert_eq!(t.len(), 1);
        assert_eq!(t.as_char(), Some('a'));
    }

    #[test]
    fn key_text_handles_empty_and_boundary_lengths() {
        let empty = KeyText::new("");
        assert!(empty.is_empty());
        assert_eq!(empty.as_str(), "");
        assert_eq!(empty.as_char(), None);
        assert_eq!(KeyText::EMPTY, empty);

        let exact = "0123456789abcde"; // exactly KEY_TEXT_INLINE bytes
        assert_eq!(exact.len(), KEY_TEXT_INLINE);
        let t = KeyText::new(exact);
        assert!(t.is_inline(), "a string of exactly the inline capacity must stay inline");
        assert_eq!(t.as_str(), exact);

        let over = "0123456789abcdef"; // one byte too many
        let t = KeyText::new(over);
        assert!(!t.is_inline());
        assert_eq!(t.as_str(), over);
    }

    #[test]
    fn key_text_preserves_multibyte_utf8() {
        // A 4-byte scalar plus a combining mark: still inline, still valid.
        let s = "\u{1F3B9}\u{0301}";
        let t = KeyText::new(s);
        assert_eq!(t.as_str(), s);
        assert_eq!(t.as_char(), None, "two scalars is not a single char");
        let single = KeyText::from('\u{1F3B9}');
        assert_eq!(single.as_char(), Some('\u{1F3B9}'));
    }

    #[test]
    fn key_text_equality_and_hash_agree_across_representations() {
        // Same text, forced through both representations, must compare and hash
        // the same or a keybinding map would miss.
        let long = "aaaaaaaaaaaaaaaaaaaaaa";
        let heap = KeyText::new(long);
        let heap2 = KeyText::new(long);
        assert!(!heap.is_inline());
        assert_eq!(heap, heap2);
        assert_eq!(hash_of(&heap), hash_of(&heap2));

        let a = KeyText::new("x");
        let b = KeyText::new("x");
        assert_eq!(hash_of(&a), hash_of(&b));
        assert_ne!(a, KeyText::new("y"));
        assert_eq!(a, "x");
    }

    #[test]
    fn named_key_display_round_trips_the_w3c_names() {
        assert_eq!(NamedKey::ArrowUp.to_string(), "ArrowUp");
        assert_eq!(NamedKey::MediaPlayPause.to_string(), "MediaPlayPause");
        assert_eq!(NamedKey::F(12).to_string(), "F12");
        assert_eq!(KeyCode::KeyA.to_string(), "KeyA");
        assert_eq!(KeyCode::F(1).to_string(), "F1");
        assert_eq!(PhysicalKey::Unidentified(Scancode::new(42)).to_string(), "Scancode(42)");
    }

    #[test]
    fn modifier_and_navigation_classification() {
        assert!(NamedKey::Shift.is_modifier());
        assert!(NamedKey::Super.is_modifier());
        assert!(!NamedKey::Enter.is_modifier());
        assert!(!NamedKey::F(4).is_modifier());
        assert!(NamedKey::ArrowLeft.is_navigation());
        assert!(!NamedKey::Enter.is_navigation());
        assert!(Key::Named(NamedKey::Control).is_modifier());
        assert!(!Key::character("a").is_modifier());
    }

    #[test]
    fn numpad_classification_separates_the_two_enter_keys() {
        assert!(KeyCode::NumpadEnter.is_numpad());
        assert!(!KeyCode::Enter.is_numpad());
        assert!(KeyCode::Numpad0.is_numpad());
        assert!(!KeyCode::Digit0.is_numpad());
        assert!(!KeyCode::F(5).is_numpad());
    }

    #[test]
    fn scancode_sentinel() {
        assert!(!Scancode::UNIDENTIFIED.is_identified());
        assert!(Scancode::new(1).is_identified());
        assert_eq!(Scancode::default(), Scancode::UNIDENTIFIED);
        assert_eq!(PhysicalKey::Unidentified(Scancode::new(7)).code(), None);
        assert_eq!(PhysicalKey::from(KeyCode::KeyQ).code(), Some(KeyCode::KeyQ));
    }

    #[test]
    fn key_accessors() {
        let k = Key::character("q");
        assert_eq!(k.as_str(), Some("q"));
        assert_eq!(k.named(), None);
        assert_eq!(Key::from(NamedKey::Tab).named(), Some(NamedKey::Tab));
        assert_eq!(Key::Named(NamedKey::Tab).as_str(), None);
        assert_eq!(Key::Dead(Some('\u{0301}')).to_string(), "Dead(\u{0301})");
        assert_eq!(Key::Dead(None).to_string(), "Dead");
    }
}
