//! Mouse cursor shapes.

use core::fmt;

/// The shape the mouse cursor takes over a window.
///
/// The names follow the CSS `cursor` property, which is the vocabulary every
/// platform's cursor set is already mapped onto, so a designer's spec
/// translates directly.
///
/// [`Cursor::Hidden`] is part of the same enum rather than a separate
/// visibility flag because hiding is what a knob or fader does while dragging:
/// it is a *shape* decision made at the same place as every other shape
/// decision, and splitting it into a second call is how the cursor ends up
/// stuck invisible after a drag is cancelled.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, Default)]
#[non_exhaustive]
pub enum Cursor {
    /// The platform's default arrow.
    #[default]
    Default,
    /// A context menu is available.
    ContextMenu,
    /// Help is available.
    Help,
    /// A link or button: the pointing hand.
    Pointer,
    /// Busy, but the UI still responds.
    Progress,
    /// Busy and unresponsive.
    Wait,
    /// A table cell, for range selection.
    Cell,
    /// Precise selection.
    Crosshair,
    /// Editable text.
    Text,
    /// Editable vertical text.
    VerticalText,
    /// An alias or shortcut will be created.
    Alias,
    /// A copy will be made.
    Copy,
    /// The item will be moved.
    Move,
    /// The item cannot be dropped here.
    NoDrop,
    /// The action is not allowed.
    NotAllowed,
    /// The item can be grabbed.
    Grab,
    /// The item is being dragged.
    Grabbing,
    /// Resize the east edge.
    EResize,
    /// Resize the north edge.
    NResize,
    /// Resize the north-east corner.
    NeResize,
    /// Resize the north-west corner.
    NwResize,
    /// Resize the south edge.
    SResize,
    /// Resize the south-east corner.
    SeResize,
    /// Resize the south-west corner.
    SwResize,
    /// Resize the west edge.
    WResize,
    /// Resize horizontally, both directions.
    EwResize,
    /// Resize vertically, both directions.
    NsResize,
    /// Resize along the north-east / south-west diagonal.
    NeswResize,
    /// Resize along the north-west / south-east diagonal.
    NwseResize,
    /// Resize a column divider. The shape a mixer channel-width splitter wants.
    ColResize,
    /// Resize a row divider. The shape a track-height splitter wants.
    RowResize,
    /// Scrolling in any direction.
    AllScroll,
    /// Zoom in.
    ZoomIn,
    /// Zoom out.
    ZoomOut,
    /// No cursor at all. Used while dragging a continuous control so the
    /// pointer does not distract from the value being changed; the application
    /// is responsible for restoring a visible shape when the drag ends.
    Hidden,
}

impl Cursor {
    /// True when the cursor should be drawn at all.
    #[inline]
    pub const fn is_visible(self) -> bool {
        !matches!(self, Cursor::Hidden)
    }

    /// The CSS `cursor` keyword for this shape, or `None` for [`Cursor::Hidden`]
    /// which has no shape.
    ///
    /// Useful for logging and for a future web backend, where the value can be
    /// written straight into a style.
    pub const fn css_name(self) -> Option<&'static str> {
        Some(match self {
            Cursor::Default => "default",
            Cursor::ContextMenu => "context-menu",
            Cursor::Help => "help",
            Cursor::Pointer => "pointer",
            Cursor::Progress => "progress",
            Cursor::Wait => "wait",
            Cursor::Cell => "cell",
            Cursor::Crosshair => "crosshair",
            Cursor::Text => "text",
            Cursor::VerticalText => "vertical-text",
            Cursor::Alias => "alias",
            Cursor::Copy => "copy",
            Cursor::Move => "move",
            Cursor::NoDrop => "no-drop",
            Cursor::NotAllowed => "not-allowed",
            Cursor::Grab => "grab",
            Cursor::Grabbing => "grabbing",
            Cursor::EResize => "e-resize",
            Cursor::NResize => "n-resize",
            Cursor::NeResize => "ne-resize",
            Cursor::NwResize => "nw-resize",
            Cursor::SResize => "s-resize",
            Cursor::SeResize => "se-resize",
            Cursor::SwResize => "sw-resize",
            Cursor::WResize => "w-resize",
            Cursor::EwResize => "ew-resize",
            Cursor::NsResize => "ns-resize",
            Cursor::NeswResize => "nesw-resize",
            Cursor::NwseResize => "nwse-resize",
            Cursor::ColResize => "col-resize",
            Cursor::RowResize => "row-resize",
            Cursor::AllScroll => "all-scroll",
            Cursor::ZoomIn => "zoom-in",
            Cursor::ZoomOut => "zoom-out",
            Cursor::Hidden => return None,
        })
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.css_name().unwrap_or("none"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_is_the_only_shapeless_cursor() {
        assert!(!Cursor::Hidden.is_visible());
        assert_eq!(Cursor::Hidden.css_name(), None);
        assert!(Cursor::Default.is_visible());
        assert_eq!(Cursor::Default.css_name(), Some("default"));
    }

    #[test]
    fn css_names_are_kebab_case_and_unique() {
        // A duplicate name would mean two shapes collapse to one on any backend
        // that maps through the CSS keyword.
        let all = [
            Cursor::Default,
            Cursor::ContextMenu,
            Cursor::Help,
            Cursor::Pointer,
            Cursor::Progress,
            Cursor::Wait,
            Cursor::Cell,
            Cursor::Crosshair,
            Cursor::Text,
            Cursor::VerticalText,
            Cursor::Alias,
            Cursor::Copy,
            Cursor::Move,
            Cursor::NoDrop,
            Cursor::NotAllowed,
            Cursor::Grab,
            Cursor::Grabbing,
            Cursor::EResize,
            Cursor::NResize,
            Cursor::NeResize,
            Cursor::NwResize,
            Cursor::SResize,
            Cursor::SeResize,
            Cursor::SwResize,
            Cursor::WResize,
            Cursor::EwResize,
            Cursor::NsResize,
            Cursor::NeswResize,
            Cursor::NwseResize,
            Cursor::ColResize,
            Cursor::RowResize,
            Cursor::AllScroll,
            Cursor::ZoomIn,
            Cursor::ZoomOut,
        ];
        let mut names: Vec<&str> = all.iter().map(|c| c.css_name().unwrap()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate CSS cursor keyword");
        for n in names {
            assert!(
                n.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "`{n}` is not a CSS keyword"
            );
        }
    }

    #[test]
    fn default_cursor_is_the_arrow() {
        assert_eq!(Cursor::default(), Cursor::Default);
        assert_eq!(Cursor::ColResize.to_string(), "col-resize");
        assert_eq!(Cursor::Hidden.to_string(), "none");
    }
}
