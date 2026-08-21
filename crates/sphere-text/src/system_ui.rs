//! The platform's own interface font.
//!
//! `fontdb` resolves an unnamed request through its generic sans-serif family,
//! whose default is `Arial` — a font Windows has not used for its interface
//! since Windows 95, and which has no Thai, no CJK and no Cyrillic to speak of.
//! Text with no explicit family therefore came out in the wrong face on every
//! platform, and on a non-Latin system it came out in a fallback chosen by
//! accident.
//!
//! ## Why this is queried and not hard-coded
//!
//! On Windows the interface font is not a constant. It is
//! `NONCLIENTMETRICSW::lfMessageFont`, and the shell picks it per locale: Segoe
//! UI on a Latin system, **Yu Gothic UI** on Japanese, **Malgun Gothic** on
//! Korean, **Microsoft JhengHei UI** on Traditional Chinese, **Leelawadee UI**
//! on Thai. A user can also change it. Hard-coding `Segoe UI` would put Latin
//! metrics on a Japanese desktop and make every interface in this engine look
//! subtly foreign to the person using it.
//!
//! The query is also what a user's own accessibility setting travels through,
//! which is a reason to read it rather than guess it.

/// The family the platform uses for its own interface text.
///
/// Falls back to a per-platform constant when the query fails or the platform
/// has no such notion. Never returns an empty string.
pub fn family() -> String {
    #[cfg(windows)]
    {
        windows_ui_family().unwrap_or_else(|| DEFAULT_UI.to_string())
    }
    #[cfg(not(windows))]
    {
        DEFAULT_UI.to_string()
    }
}

/// The family the platform uses for monospaced interface text.
///
/// Not queried: no platform exposes a "system monospace" setting the way it
/// exposes a UI font, so this is the best-known default per platform rather
/// than a lookup pretending to be one.
pub fn monospace_family() -> &'static str {
    #[cfg(windows)]
    {
        // Consolas has shipped since Vista. `fontdb`'s default here is
        // `Courier New`, which is a typewriter face and hints badly at UI sizes.
        "Consolas"
    }
    #[cfg(target_os = "macos")]
    {
        "SF Mono"
    }
    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        "DejaVu Sans Mono"
    }
}

/// What to use when the platform cannot be asked.
#[cfg(windows)]
const DEFAULT_UI: &str = "Segoe UI";
#[cfg(target_os = "macos")]
const DEFAULT_UI: &str = "Helvetica Neue";
#[cfg(all(not(windows), not(target_os = "macos")))]
const DEFAULT_UI: &str = "DejaVu Sans";

/// Reads `lfMessageFont` out of the shell's non-client metrics.
#[cfg(windows)]
fn windows_ui_family() -> Option<String> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SystemParametersInfoW,
    };

    let mut metrics = NONCLIENTMETRICSW {
        cbSize: core::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..unsafe { core::mem::zeroed() }
    };
    // SAFETY: `SystemParametersInfoW` fills the struct whose size it is told in
    // `cbSize`, which is set above from the type itself. It writes nothing on
    // failure and returns zero, which is checked. No handle is created and
    // nothing is retained.
    let ok = unsafe {
        SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, metrics.cbSize, (&raw mut metrics).cast(), 0)
    };
    if ok == 0 {
        return None;
    }
    utf16_face_name(&metrics.lfMessageFont.lfFaceName)
}

/// Turns a `LOGFONTW` face name into a `String`.
///
/// The array is NUL-padded rather than NUL-terminated when the name fills it,
/// so the terminator search has to tolerate its absence.
#[cfg(windows)]
fn utf16_face_name(raw: &[u16]) -> Option<String> {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    if end == 0 {
        return None;
    }
    let name = String::from_utf16_lossy(&raw[..end]);
    let trimmed = name.trim();
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ui_family_is_always_reported() {
        // The whole point is that an unnamed request resolves to something the
        // platform actually uses. An empty answer would put it back on Arial.
        let f = family();
        assert!(!f.trim().is_empty(), "reported an empty family");
        assert!(!monospace_family().is_empty());
    }

    #[test]
    fn the_reported_family_is_not_the_fontdb_default() {
        // Arial is `fontdb`'s generic sans-serif and is the bug this module
        // exists to fix, so reporting it would mean the query silently failed.
        assert_ne!(family(), "Arial");
    }

    #[cfg(windows)]
    #[test]
    fn a_face_name_stops_at_its_terminator() {
        let mut raw = [0u16; 32];
        for (slot, c) in raw.iter_mut().zip("Segoe UI".encode_utf16()) {
            *slot = c;
        }
        assert_eq!(utf16_face_name(&raw).as_deref(), Some("Segoe UI"));
    }

    #[cfg(windows)]
    #[test]
    fn a_face_name_filling_the_array_has_no_terminator_to_find() {
        // `LOGFONTW::lfFaceName` is NUL-*padded*, so a name that fills all 32
        // slots has no terminator and a naive scan runs off the end.
        let raw = [b'A' as u16; 32];
        assert_eq!(utf16_face_name(&raw).map(|s| s.len()), Some(32));
    }

    #[cfg(windows)]
    #[test]
    fn an_empty_face_name_is_refused_rather_than_returned_blank() {
        assert_eq!(utf16_face_name(&[0u16; 32]), None);
        let mut spaces = [0u16; 32];
        spaces[0] = b' ' as u16;
        assert_eq!(utf16_face_name(&spaces), None);
    }
}
