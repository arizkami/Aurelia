//! Semantic information for assistive technology.
//!
//! SphereKit does not ship an accessibility bridge yet — there is no AT-SPI,
//! UI Automation or NSAccessibility integration in v0.1. What exists is the
//! *shape* of the information such a bridge needs, attached to elements that
//! already know it.
//!
//! That ordering is deliberate. Retrofitting semantics onto a widget layer that
//! never modelled roles, values or states means rewriting every widget. Carrying
//! the information from the start makes the bridge a wiring job later, and costs
//! almost nothing now: an element with no semantics returns `None` and the field
//! is one `Option` on a struct that is rebuilt per frame anyway.

use smallvec::SmallVec;
use spherekit_core::Px;

/// What kind of thing an element is.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Default)]
pub enum Role {
    /// No semantic meaning; purely visual.
    #[default]
    Presentation,
    /// A grouping container.
    Group,
    /// Static text.
    Label,
    /// A push button.
    Button,
    /// A toggle button with two states.
    Toggle,
    /// A checkbox.
    Checkbox,
    /// A radio button within a group.
    Radio,
    /// A continuous value control.
    Slider,
    /// A rotary continuous value control.
    Knob,
    /// A linear level control.
    Fader,
    /// A single-line or multi-line editable field.
    TextField,
    /// A numeric entry field.
    NumberField,
    /// A collapsed list of choices.
    ComboBox,
    /// A menu.
    Menu,
    /// One item within a menu.
    MenuItem,
    /// A tab strip.
    TabList,
    /// One tab.
    Tab,
    /// A scrollable region.
    ScrollArea,
    /// A list.
    List,
    /// One item in a list.
    ListItem,
    /// A hierarchical tree.
    Tree,
    /// One node in a tree.
    TreeItem,
    /// A determinate or indeterminate progress indicator.
    Progress,
    /// A read-only level indicator, such as a VU meter.
    Meter,
    /// A dialog.
    Dialog,
    /// A tooltip.
    Tooltip,
    /// A visual separator.
    Separator,
    /// A graphic whose content is described by its label.
    Image,
    /// A canvas whose content is described by its label, such as a waveform.
    Canvas,
}

impl Role {
    /// True when the role implies a numeric value that should be announced.
    #[inline]
    pub fn has_value(self) -> bool {
        matches!(
            self,
            Role::Slider
                | Role::Knob
                | Role::Fader
                | Role::Progress
                | Role::Meter
                | Role::NumberField
        )
    }

    /// True when the role implies a boolean checked state.
    #[inline]
    pub fn has_checked(self) -> bool {
        matches!(self, Role::Toggle | Role::Checkbox | Role::Radio)
    }

    /// True when the role is normally reachable by keyboard.
    #[inline]
    pub fn is_interactive(self) -> bool {
        !matches!(
            self,
            Role::Presentation
                | Role::Group
                | Role::Label
                | Role::Separator
                | Role::Image
                | Role::Canvas
                | Role::Meter
                | Role::Progress
                | Role::Tooltip
        )
    }
}

/// A continuous value with a range, as an assistive technology would announce it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ValueRange {
    /// Current value, in the control's own units.
    pub value: f32,
    /// Lowest value.
    pub min: f32,
    /// Highest value.
    pub max: f32,
    /// Step used by keyboard adjustment, or `None` for continuous.
    pub step: Option<f32>,
}

impl ValueRange {
    /// The value as a fraction of the range, clamped to `0..=1`.
    #[inline]
    pub fn normalized(&self) -> f32 {
        let span = self.max - self.min;
        if span.abs() < f32::EPSILON {
            0.0
        } else {
            ((self.value - self.min) / span).clamp(0.0, 1.0)
        }
    }
}

/// Something a user can do to an element.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// Activate: click, press, or confirm.
    Activate,
    /// Increase the value.
    Increment,
    /// Decrease the value.
    Decrement,
    /// Return to the default value.
    Reset,
    /// Take keyboard focus.
    Focus,
    /// Expand a collapsed region.
    Expand,
    /// Collapse an expanded region.
    Collapse,
    /// Show a context menu.
    ShowContextMenu,
    /// Scroll the element into view.
    ScrollIntoView,
}

/// Everything an assistive technology would want to know about one element.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Semantics {
    /// What kind of control this is.
    pub role: Role,
    /// The accessible name. Usually the visible label.
    pub label: Option<String>,
    /// A longer description, for a tooltip or help text.
    pub description: Option<String>,
    /// The current value, for controls that have one.
    pub value: Option<ValueRange>,
    /// A human-readable rendering of the value.
    ///
    /// A fader at `0.79` should be announced as `-6.0 dB`, not as a fraction.
    /// Only the widget knows its own unit mapping, so it supplies the string.
    pub value_text: Option<String>,
    /// Checked state, for toggles and checkboxes.
    pub checked: Option<bool>,
    /// True when the element is present but does not accept input.
    pub disabled: bool,
    /// True when the element is selected within its container.
    pub selected: bool,
    /// True when the element is expanded.
    pub expanded: Option<bool>,
    /// Actions this element supports.
    pub actions: SmallVec<[Action; 4]>,
    /// Keyboard traversal order. Lower comes first; `None` uses tree order.
    pub tab_index: Option<i32>,
    /// Live-region politeness, for values that change without user action.
    pub live: Option<Live>,
}

/// How urgently a changing value should be announced.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Live {
    /// Announce at the next natural pause.
    Polite,
    /// Interrupt to announce.
    Assertive,
}

impl Semantics {
    /// A labelled element of the given role.
    pub fn new(role: Role, label: impl Into<String>) -> Self {
        Self { role, label: Some(label.into()), ..Default::default() }
    }

    /// An element with a role but no name of its own.
    pub fn role(role: Role) -> Self {
        Self { role, ..Default::default() }
    }

    /// Sets the description.
    pub fn description(mut self, text: impl Into<String>) -> Self {
        self.description = Some(text.into());
        self
    }

    /// Sets a value range.
    pub fn value(mut self, value: ValueRange) -> Self {
        self.value = Some(value);
        self
    }

    /// Sets the human-readable value string.
    pub fn value_text(mut self, text: impl Into<String>) -> Self {
        self.value_text = Some(text.into());
        self
    }

    /// Sets the checked state.
    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }

    /// Marks the element disabled.
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Declares a supported action.
    pub fn action(mut self, action: Action) -> Self {
        if !self.actions.contains(&action) {
            self.actions.push(action);
        }
        self
    }

    /// Sets the keyboard traversal index.
    pub fn tab_index(mut self, index: i32) -> Self {
        self.tab_index = Some(index);
        self
    }

    /// Fills in the actions a role implies but the caller did not spell out.
    ///
    /// A slider always supports increment, decrement and focus; making every
    /// widget author list them is how they end up inconsistent.
    pub fn with_implied_actions(mut self) -> Self {
        if self.disabled {
            return self;
        }
        if self.role.is_interactive() {
            self = self.action(Action::Focus);
        }
        if self.role.has_value() && self.role.is_interactive() {
            self = self.action(Action::Increment).action(Action::Decrement);
        }
        if matches!(self.role, Role::Button | Role::MenuItem | Role::Tab) || self.role.has_checked()
        {
            self = self.action(Action::Activate);
        }
        self
    }

    /// The string an assistive technology would announce for the value.
    pub fn announced_value(&self) -> Option<String> {
        if let Some(t) = &self.value_text {
            return Some(t.clone());
        }
        if let Some(v) = self.value {
            return Some(format!("{:.0}%", v.normalized() * 100.0));
        }
        self.checked.map(|c| if c { "checked".into() } else { "unchecked".into() })
    }
}

/// A resolved semantic node, as a platform bridge would consume it.
///
/// Produced by walking the element tree after layout, so bounds are known.
#[derive(Clone, Debug, PartialEq)]
pub struct SemanticNode {
    /// The node this describes.
    pub node: spherekit_core::NodeId,
    /// The semantics the element declared.
    pub semantics: Semantics,
    /// Absolute bounds in window-logical pixels, which a screen reader needs to
    /// place its highlight.
    pub bounds: spherekit_core::Rect<Px>,
    /// Indices of child nodes within the containing tree.
    pub children: SmallVec<[usize; 4]>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_roles_are_classified() {
        assert!(Role::Slider.has_value());
        assert!(Role::Fader.has_value());
        assert!(Role::Meter.has_value());
        assert!(!Role::Button.has_value());
    }

    #[test]
    fn a_meter_is_not_keyboard_interactive() {
        // A VU meter reports a value but a user cannot change it, so it must
        // not appear in the tab order.
        assert!(Role::Meter.has_value());
        assert!(!Role::Meter.is_interactive());
        assert!(Role::Fader.is_interactive());
    }

    #[test]
    fn implied_actions_fill_in_what_a_role_guarantees() {
        let s = Semantics::new(Role::Fader, "Input").with_implied_actions();
        assert!(s.actions.contains(&Action::Increment));
        assert!(s.actions.contains(&Action::Decrement));
        assert!(s.actions.contains(&Action::Focus));
    }

    #[test]
    fn a_disabled_element_advertises_no_actions() {
        let s = Semantics::new(Role::Button, "Bypass").disabled(true).with_implied_actions();
        assert!(s.actions.is_empty());
    }

    #[test]
    fn implied_actions_do_not_duplicate_explicit_ones() {
        let s =
            Semantics::new(Role::Button, "Play").action(Action::Activate).with_implied_actions();
        assert_eq!(s.actions.iter().filter(|a| **a == Action::Activate).count(), 1);
    }

    #[test]
    fn normalized_value_clamps_and_handles_a_zero_span() {
        let v = ValueRange { value: 5.0, min: 0.0, max: 10.0, step: None };
        assert!((v.normalized() - 0.5).abs() < 1e-6);

        let over = ValueRange { value: 50.0, min: 0.0, max: 10.0, step: None };
        assert_eq!(over.normalized(), 1.0);

        let degenerate = ValueRange { value: 5.0, min: 3.0, max: 3.0, step: None };
        assert_eq!(degenerate.normalized(), 0.0, "a zero span must not divide by zero");
    }

    #[test]
    fn a_widget_supplied_value_string_wins_over_the_percentage() {
        // A fader must announce "-6.0 dB", not "79%".
        let s = Semantics::new(Role::Fader, "Input")
            .value(ValueRange { value: 0.79, min: 0.0, max: 1.0, step: None })
            .value_text("-6.0 dB");
        assert_eq!(s.announced_value().as_deref(), Some("-6.0 dB"));
    }

    #[test]
    fn a_value_with_no_string_falls_back_to_a_percentage() {
        let s = Semantics::role(Role::Progress).value(ValueRange {
            value: 0.25,
            min: 0.0,
            max: 1.0,
            step: None,
        });
        assert_eq!(s.announced_value().as_deref(), Some("25%"));
    }

    #[test]
    fn a_checkbox_announces_its_state() {
        assert_eq!(
            Semantics::new(Role::Checkbox, "Mute").checked(true).announced_value().as_deref(),
            Some("checked")
        );
        assert_eq!(
            Semantics::new(Role::Checkbox, "Mute").checked(false).announced_value().as_deref(),
            Some("unchecked")
        );
    }

    #[test]
    fn a_presentational_element_announces_nothing() {
        assert_eq!(Semantics::default().announced_value(), None);
        assert!(!Role::Presentation.is_interactive());
    }
}
