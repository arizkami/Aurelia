//! # spherekit-ui
//!
//! The UI layer: a declarative element API over a retained node tree, with
//! event dispatch, focus, theming and widgets.
//!
//! ## The shape of it
//!
//! ```ignore
//! div()
//!     .flex_col()
//!     .gap(px(8.0))
//!     .p(px(12.0))
//!     .bg(theme.colors.surface)
//!     .rounded(theme.radii.lg)
//!     .child(label("Threshold").text_size(theme.typography.sm))
//!     .child(
//!         div()
//!             .id("threshold")
//!             .focusable()
//!             .size(px(64.0))
//!             .on_mouse_down(|cx| cx.capture())
//!             .on_mouse_move(|cx| { /* drag */ cx.notify(); }),
//!     )
//! ```
//!
//! The code reads immediate. The machinery is retained: [`UiTree::build`]
//! reconciles the produced elements against last frame's layout nodes by
//! identity, so an unchanged tree reuses every node, marks nothing
//! layout-dirty, and costs zero layout work. There is a test that asserts
//! exactly that, because it is the property the whole design exists to deliver.
//!
//! ## Invalidation
//!
//! Two levels, and the difference matters more than anything else here:
//!
//! | Call | Marks | Cost |
//! |---|---|---|
//! | [`EventContext::notify`] | `PAINT` | Repaint this node. No layout, no reshaping. |
//! | [`EventContext::notify_layout`] | `LAYOUT` | Relayout the ancestor chain and this subtree. |
//!
//! A meter, a knob drag, a hover highlight and a theme change all take the
//! first path. Only a structural or size change takes the second.
//!
//! ## The frame
//!
//! ```ignore
//! tree.build(view.render());                 // reconcile
//! tree.compute_layout(viewport)?;            // skipped entirely if clean
//! tree.paint(&mut canvas, viewport, time);   // walk once, cull, record
//! tree.end_frame();
//! ```
//!
//! and, on input:
//!
//! ```ignore
//! let result = tree.dispatch(&event);
//! if result.relayout { /* schedule layout */ }
//! if result.repaint  { window.request_redraw(); }
//! ```

#![deny(missing_docs)]

pub mod color;
pub mod date;
pub mod edit;
pub mod element;
pub mod event;
pub mod field;
pub mod focus;
pub mod input;
pub mod keyboard;
pub mod overlay;
pub mod semantics;
pub mod style;
pub mod text;
pub mod theme;
pub mod tree;
pub mod widgets;

pub use color::{
    ColorArea, ColorChannel, ColorPicker, ColorSlider, ColorSwatch, Hsva, alpha_slider, color_area,
    color_picker, color_swatch, hex_string, hsva, hue_slider, parse_hex,
};
pub use date::{Calendar, Date, Weekday, calendar};
pub use edit::{Motion, Preedit, TextEdit};
pub use element::{
    AnyElement, Div, Element, Empty, EventContext, HandlerKind, Handlers, ImeArea,
    InteractionState, Interactive, IntoElement, PaintContext, ParentElement, ScrollMetrics, Styled,
    div,
};
pub use event::{
    ClickTracker, ElementState, EventFlow, GestureState, HitChain, HitTarget, ImeEvent, Key,
    KeyEvent, LongPressEvent, Modifiers, MouseButton, MouseButtonEvent, MouseMoveEvent, Phase,
    PinchEvent, PointerCancelEvent, PointerSource, ScrollDelta, ScrollEvent, ScrollPhase,
    SwipeDirection, SwipeEvent, TextInputEvent, TouchEvent, TouchPoint, UiEvent,
};
pub use field::{TextField, text_field};
pub use focus::{FocusDirection, FocusHandle, FocusRegistry, Focusable, Scope, ScopeId};
pub use input::{InputTranslator, TouchConfig};
pub use keyboard::{
    KeyPress, KeyboardLayer, VirtualKey, VirtualKeyboard, numeric_keyboard, virtual_keyboard,
};
pub use overlay::{
    OVERLAY_Z, Overlay, Popover, PopoverAlign, PopoverSide, TOAST_Z, Toast, ToastVariant, overlay,
    popover, toast, toast_layer,
};
pub use semantics::{Action, Live, Role, SemanticNode, Semantics, ValueRange};
pub use style::{Cursor, FocusRing, PaintStyle, StyledInteraction};
pub use text::{Label, draw_layout, label};
pub use theme::{Palette, Radii, Shadows, Spacing, TextRole, Theme, TypeScale, Typography};
pub use tree::{DispatchResult, TreeStats, UiTree};
pub use widgets::{
    Avatar, Badge, BadgeVariant, Button, ButtonVariant, ContextMenu, Dropdown, DropdownSide,
    MenuItem, Presence, Progress, Radio, ScrollView, Scrollbar, ScrollbarPolicy, SegmentedControl,
    Spinner, Stepper, Toggle, Tooltip, ValueControl, ValueShape, avatar, badge, button, checkbox,
    context_menu, dropdown, fader, knob, menu_item, panel, progress, progress_indeterminate, radio,
    scroll_area, scroll_view, segmented, separator, slider, spinner, stepper, toggle, tooltip,
};

/// Everything a typical consumer needs, in one import.
pub mod prelude {
    pub use crate::color::{Hsva, color_area, color_picker, color_swatch, hue_slider};
    pub use crate::date::{Calendar, Date, Weekday, calendar};
    pub use crate::element::{
        Element, EventContext, Interactive, IntoElement, PaintContext, ParentElement, Styled, div,
    };
    pub use crate::event::{
        EventFlow, Key, Modifiers, MouseButton, PointerSource, SwipeDirection, UiEvent,
    };
    pub use crate::focus::FocusDirection;
    pub use crate::input::{InputTranslator, TouchConfig};
    pub use crate::keyboard::{KeyPress, virtual_keyboard};
    pub use crate::overlay::{overlay, popover, toast, toast_layer};
    pub use crate::semantics::{Role, Semantics};
    pub use crate::style::{Cursor, PaintStyle, StyledInteraction};
    pub use crate::text::label;
    pub use crate::theme::Theme;
    pub use crate::tree::UiTree;
    pub use crate::widgets::prelude::*;
    pub use spherekit_core::prelude::*;
    pub use spherekit_layout::prelude::*;
}
