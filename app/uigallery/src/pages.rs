//! The gallery itself: one page per widget family.
//!
//! Every control on every page is **live**. Nothing here is a picture of a
//! button — the toggles toggle, the sliders drag, the fields take an input
//! method, and the state they write is the state the next frame reads. A
//! gallery that showed static mock-ups would be a worse document than the
//! source it is documenting.
//!
//! Each page follows the same shape: a page heading, then a stack of
//! [`specimen`] blocks. A specimen is a titled row of variants with a one-line
//! note saying what the variant is *for* — the part an API listing cannot tell
//! you.

use std::rc::Rc;

use spherekit::core::{Color, Px, px, relative};
use spherekit::ui::{
    AnyElement, BadgeVariant, ButtonVariant, Cursor, Date, EventContext, Hsva, Interactive,
    IntoElement, ParentElement, PopoverSide, Presence, Role, Semantics, Styled, StyledInteraction,
    TextRole, Theme, ToastVariant, TypeScale, Weekday, alpha_slider, avatar, badge, button,
    calendar, checkbox, color_area, color_picker, color_swatch, div, dropdown, hue_slider, label,
    popover, progress, progress_indeterminate, radio, scroll_area, segmented, separator, slider,
    spinner, stepper, text_field, toggle, tooltip,
};

use crate::{State, USER_EMAIL, USER_NAME};

/// The pages the sidebar navigates between.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Home,
    Buttons,
    Selection,
    Values,
    Text,
    Colour,
    Dates,
    Identity,
    Containers,
    Overlays,
    Palette,
}

impl Page {
    pub(crate) const ALL: [Page; 11] = [
        Page::Home,
        Page::Buttons,
        Page::Selection,
        Page::Values,
        Page::Text,
        Page::Colour,
        Page::Dates,
        Page::Identity,
        Page::Containers,
        Page::Overlays,
        Page::Palette,
    ];

    /// The pages the Home page offers as cards: everything but itself.
    pub(crate) fn tour() -> impl Iterator<Item = Page> {
        Page::ALL.into_iter().filter(|p| *p != Page::Home)
    }

    /// Matches a page by its title, case-insensitively.
    ///
    /// For `SPHEREKIT_GALLERY_PAGE`, which opens the window straight onto one
    /// page. A gallery is documentation, and documentation needs a way to take
    /// the same screenshot twice.
    pub(crate) fn from_name(name: &str) -> Option<Page> {
        let name = name.trim();
        Page::ALL.into_iter().find(|p| p.title().eq_ignore_ascii_case(name))
    }

    pub(crate) fn title(self) -> &'static str {
        match self {
            Page::Buttons => "Buttons",
            Page::Selection => "Selection",
            Page::Values => "Values",
            Page::Text => "Text",
            Page::Colour => "Colour",
            Page::Dates => "Dates",
            Page::Identity => "Identity",
            Page::Containers => "Containers",
            Page::Overlays => "Overlays",
            Page::Palette => "Palette",
            Page::Home => "Home",
        }
    }

    /// The one-line summary under the page title.
    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Page::Buttons => "Push buttons: four weights, and what each one is for.",
            Page::Selection => "Switches, checkboxes, radios and segments: one answer, or many.",
            Page::Values => "Sliders, faders and knobs over one continuous value.",
            Page::Text => "Editable fields and the type scale they sit in.",
            Page::Colour => "The square, the ramps, and the panel that composes them.",
            Page::Dates => "A month grid that owns neither the month nor the day.",
            Page::Identity => "Avatars, presence, and the menu they hang off.",
            Page::Containers => "Panels, scrolling, separators and progress.",
            Page::Overlays => "A scrim, a popover and a toast — none of which owns a timer.",
            Page::Home => "Every built-in widget, live, in one window.",
            Page::Palette => "Every semantic token in the active theme.",
        }
    }

    /// A monochrome icon, tinted by the theme at draw time.
    ///
    /// Stroked rather than filled, like the rest of the shell's icons: a
    /// stroked path is the shape most likely to look jagged without
    /// multisampling, so it is the honest thing to put in a gallery.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            Page::Buttons => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round">
                     <rect x="3" y="7.5" width="18" height="9" rx="4.5"/>
                     <path d="M8.5 12h7"/></svg>"##
            }
            Page::Selection => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round"
                     stroke-linejoin="round">
                     <rect x="3" y="3.5" width="17" height="17" rx="4"/>
                     <path d="M7.5 12.2l3 3 6-6.4"/></svg>"##
            }
            Page::Values => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round">
                     <path d="M3 8h18M3 16h18"/>
                     <circle cx="9" cy="8" r="2.6"/><circle cx="16" cy="16" r="2.6"/></svg>"##
            }
            Page::Text => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round">
                     <path d="M5 6.5h14M12 6.5V19M9 19h6"/></svg>"##
            }
            Page::Colour => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round">
                     <path d="M4 15.5 15.2 4.3a2.1 2.1 0 0 1 3 0l1.5 1.5a2.1 2.1 0 0 1 0 3L8.5 20"/>
                     <path d="M4 15.5 8.5 20H4z"/><path d="M12.4 7.1l4.5 4.5"/></svg>"##
            }
            Page::Dates => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round"
                     stroke-linejoin="round">
                     <rect x="3.2" y="5" width="17.6" height="15.5" rx="3"/>
                     <path d="M3.2 10h17.6M8 3v4M16 3v4"/></svg>"##
            }
            Page::Identity => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round">
                     <circle cx="12" cy="8.4" r="3.9"/>
                     <path d="M4.6 20c1.3-3.7 4-5.6 7.4-5.6s6.1 1.9 7.4 5.6"/></svg>"##
            }
            Page::Containers => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round">
                     <rect x="3" y="4" width="18" height="16" rx="3"/>
                     <path d="M3 9h18"/></svg>"##
            }
            Page::Overlays => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round">
                     <rect x="3" y="3.5" width="13" height="13" rx="3"/>
                     <path d="M8 20.5h9.5a3 3 0 0 0 3-3V8"/></svg>"##
            }
            Page::Home => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linecap="round"
                     stroke-linejoin="round">
                     <path d="M3.5 10.5 12 3.5l8.5 7"/>
                     <path d="M5.5 12v8h13v-8"/><path d="M10 20v-5h4v5"/></svg>"##
            }
            Page::Palette => {
                r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none"
                     stroke="black" stroke-width="1.8" stroke-linejoin="round">
                     <path d="M12 3a9 9 0 1 0 0 18c1.4 0 2.2-.9 2.2-2 0-1.4-1.2-1.7-1.2-2.7
                              0-.8.7-1.5 1.6-1.5H16a5 5 0 0 0 5-5c0-3.9-4-6.8-9-6.8z"/>
                     <circle cx="7.6" cy="11.5" r="1.3"/>
                     <circle cx="12" cy="7.6" r="1.3"/>
                     <circle cx="16.4" cy="10.4" r="1.3"/></svg>"##
            }
        }
    }
}

/// Renders the body of a page.
pub(crate) fn render(
    page: Page,
    state: &Rc<State>,
    theme: &Theme,
    adapter: &str,
    stats: &spherekit::SurfaceStats,
    icons: &[(Page, spherekit::core::SvgId)],
) -> AnyElement {
    match page {
        Page::Home => home(state, theme, adapter, stats, icons),
        Page::Overlays => overlays(state, theme),
        Page::Buttons => buttons(state, theme),
        Page::Selection => selection(state, theme),
        Page::Values => values(state, theme),
        Page::Text => text(state, theme),
        Page::Colour => colour(state, theme),
        Page::Dates => dates(state, theme),
        Page::Identity => identity(state, theme),
        Page::Containers => containers(state, theme, adapter, stats),
        Page::Palette => palette(theme),
    }
}

// ---------------------------------------------------------------------------
// Pages
// ---------------------------------------------------------------------------

fn buttons(state: &Rc<State>, theme: &Theme) -> AnyElement {
    use spherekit::ui::ButtonVariant as V;
    let c = theme.colors;
    let pressed = state.presses.get();

    let one = |text: &str, variant: V, key: &'static str, state: &Rc<State>| {
        let s = Rc::clone(state);
        let name = text.to_string();
        button(text)
            .id(key)
            .variant(variant)
            .on_press(move || {
                s.presses.set(s.presses.get() + 1);
                s.say(format!("{name} pressed."));
            })
            .into_element()
    };

    page(theme, Page::Buttons)
        .child(specimen(
            theme,
            "Variants",
            "Weight is a statement about consequence, not decoration. One primary per view.",
            row(theme)
                .child(one("Primary", V::Primary, "b.primary", state))
                .child(one("Secondary", V::Secondary, "b.secondary", state))
                .child(one("Outline", V::Outline, "b.outline", state))
                .child(one("Ghost", V::Ghost, "b.ghost", state))
                .child(one("Danger", V::Danger, "b.danger", state))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Outline is never the default",
            "An outline reads as lighter than a fill on a dark surface and heavier on a light one, so a default outline would change a screen's hierarchy the moment the theme flipped. It is here for the case that wants it: a button with no surface of its own to sit on.",
            div()
                .flex_row()
                .items_center()
                .gap(theme.spacing.md)
                .h(px(72.0))
                .px_(theme.spacing.md)
                .rounded(theme.radii.lg)
                .bg(c.accent.with_alpha(0.35))
                .child(button("Outline").id("b.o1").variant(V::Outline))
                .child(button("Secondary").id("b.o2").variant(V::Secondary))
                .child(button("Ghost").id("b.o3").variant(V::Ghost))
                .child(
                    label("Over a tint, only the outline keeps its shape.")
                        .text_size(theme.typography.sm)
                        .text_color(c.text),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Disabled",
            "A disabled button also leaves the tab order — it is not merely greyed out.",
            row(theme)
                .child(button("Primary").id("b.dp").variant(V::Primary).disabled(true))
                .child(button("Secondary").id("b.ds").disabled(true))
                .child(button("Outline").id("b.do").variant(V::Outline).disabled(true))
                .child(button("Ghost").id("b.dg").variant(V::Ghost).disabled(true))
                .child(button("Danger").id("b.dd").variant(V::Danger).disabled(true))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Sizing",
            "A button hugs its label until told otherwise; width and height take any Length.",
            row(theme)
                .child(button("Auto").id("b.auto"))
                .child(button("Fixed 140").id("b.fixed").width(px(140.0)))
                .child(button("Tall").id("b.tall").height(px(40.0)))
                .child(
                    button("Half the row").id("b.half").width(relative(0.4)).variant(V::Secondary),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Icon buttons",
            "The same widget with an icon font and a glyph for a label.",
            row(theme)
                .child(
                    button("\u{E72B}")
                        .id("b.back")
                        .font(crate::ICON_FONT)
                        .text_size(px(12.0))
                        .width(px(36.0)),
                )
                .child(
                    button("\u{E72A}")
                        .id("b.fwd")
                        .font(crate::ICON_FONT)
                        .text_size(px(12.0))
                        .width(px(36.0)),
                )
                .child(
                    button("\u{E713}")
                        .id("b.cog")
                        .font(crate::ICON_FONT)
                        .text_size(px(12.0))
                        .width(px(36.0))
                        .variant(V::Ghost),
                )
                .child(
                    button("\u{E74D}")
                        .id("b.bin")
                        .font(crate::ICON_FONT)
                        .text_size(px(12.0))
                        .width(px(36.0))
                        .variant(V::Danger),
                )
                .into_element(),
        ))
        .child(
            label(format!(
                "{pressed} press{} so far this session.",
                if pressed == 1 { "" } else { "es" }
            ))
            .text_size(theme.typography.sm)
            .text_color(c.text_muted)
            .into_element(),
        )
        .into_element()
}

fn selection(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;

    page(theme, Page::Selection)
        .child(specimen(
            theme,
            "Switch",
            "For a setting that takes effect immediately. No confirm step, so no Apply button.",
            row(theme)
                .child({
                    let s = Rc::clone(state);
                    let on = state.wifi.get();
                    toggle(on).id("s.wifi").label("Wi-Fi").on_change(move |v| {
                        s.wifi.set(v);
                        s.say(if v { "Wi-Fi on." } else { "Wi-Fi off." });
                    })
                })
                .child(
                    label(if state.wifi.get() { "Wi-Fi" } else { "Wi-Fi (off)" })
                        .text_size(theme.typography.sm)
                        .text_color(c.text),
                )
                .child(div().w(px(24.0)))
                .child(toggle(true).id("s.on").label("Always on").disabled(true))
                .child(label("disabled").text_size(theme.typography.sm).text_color(c.text_muted))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Checkbox",
            "For a choice that is part of a set, or that only applies once something is submitted.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .child(check_row(state, theme, "c.a", "Ship release notes", &state.opt_a))
                .child(check_row(state, theme, "c.b", "Notify the mailing list", &state.opt_b))
                .child(check_row(state, theme, "c.c", "Tag the commit", &state.opt_c))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Radio",
            "For one answer out of a set. A radio cannot be un-chosen by clicking it again, because the set must always have an answer.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .child(radio_row(state, theme, 0, "Lossless", "FLAC, the whole file"))
                .child(radio_row(state, theme, 1, "High", "256 kbps, transparent for most material"))
                .child(radio_row(state, theme, 2, "Data saver", "96 kbps, for a metered connection"))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Segmented",
            "The same choice as a dropdown, with the options already on screen. Worth the space up to about five; past that, use a menu.",
            div()
                .flex_col()
                .items(spherekit::layout::Align::Start)
                .gap(theme.spacing.md)
                .child({
                    let s = Rc::clone(state);
                    segmented(state.density.get())
                        .id("s.density")
                        .name("Density")
                        .item("Compact")
                        .item("Cosy")
                        .item("Roomy")
                        .on_select(move |i| {
                            s.density.set(i);
                            s.say(format!("Density: {}.", ["compact", "cosy", "roomy"][i]));
                        })
                })
                .child(
                    segmented(1).id("s.seg.dis").item("Off").item("Auto").item("On").disabled(true),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "State readout",
            "Widgets own no value. What you see here is the same state the widgets above wrote.",
            label(format!(
                "wifi={}   notes={}   list={}   tag={}   quality={}   density={}",
                state.wifi.get(),
                state.opt_a.get(),
                state.opt_b.get(),
                state.opt_c.get(),
                state.quality.get(),
                state.density.get(),
            ))
            .text_size(theme.typography.sm)
            .text_color(c.text_muted)
            .into_element(),
        ))
        .into_element()
}

fn values(state: &Rc<State>, theme: &Theme) -> AnyElement {
    use spherekit::ui::{fader, knob};
    let c = theme.colors;

    page(theme, Page::Values)
        .child(specimen(
            theme,
            "Slider",
            "The horizontal default. Drag it, or focus it and use the arrow keys.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .child({
                    let s = Rc::clone(state);
                    slider(state.gain.get())
                        .id("v.gain")
                        .range(-60.0, 6.0)
                        .default_value(0.0)
                        .name("Gain")
                        .unit("dB")
                        .format(|x| format!("{x:+.1} dB"))
                        .on_change(move |x| s.gain.set(x))
                })
                .child(
                    label(format!("{:+.1} dB", state.gain.get()))
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Stepped",
            "A step quantises the drag, so the value lands on something the product can honour.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .child({
                    let s = Rc::clone(state);
                    slider(state.scale.get())
                        .id("v.scale")
                        .range(75.0, 200.0)
                        .step(25.0)
                        .default_value(100.0)
                        .name("Interface scale")
                        .format(|x| format!("{x:.0}%"))
                        .on_change(move |x| s.scale.set(x))
                })
                .child(
                    label(format!("{:.0}% — snaps to 25% stops", state.scale.get()))
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Bipolar",
            "Fills outward from the centre, which is what a pan or a balance actually means.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .child({
                    let s = Rc::clone(state);
                    slider(state.pan.get())
                        .id("v.pan")
                        .range(-1.0, 1.0)
                        .default_value(0.0)
                        .bipolar(true)
                        .name("Pan")
                        .format(|x| format!("{x:+.2}"))
                        .on_change(move |x| s.pan.set(x))
                })
                .child(
                    label(pan_text(state.pan.get()))
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Stepper",
            "For a small, meaningful step where a slider would offer a continuum the product cannot honour. Click the signs, drag the middle to scrub, or use the arrow keys.",
            row(theme)
                .child({
                    let s = Rc::clone(state);
                    stepper(state.takes.get())
                        .id("v.takes")
                        .range(1.0, 32.0)
                        .name("Takes")
                        .format(|v| format!("{v:.0} takes"))
                        .on_change(move |v| s.takes.set(v))
                })
                .child({
                    let s = Rc::clone(state);
                    stepper(state.bpm.get())
                        .id("v.bpm")
                        .range(40.0, 240.0)
                        .step(5.0)
                        .name("Tempo")
                        .format(|v| format!("{v:.0} BPM"))
                        .on_change(move |v| s.bpm.set(v))
                })
                .child(stepper(4.0).id("v.step.dis").range(1.0, 8.0).disabled(true))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Fader and knob",
            "The same control, two shapes. Both drag vertically — a wrist is bad at arcs.",
            row(theme)
                .child({
                    let s = Rc::clone(state);
                    fader(state.level.get())
                        .id("v.fader")
                        .range(0.0, 100.0)
                        .name("Level")
                        .on_change(move |x| s.level.set(x))
                })
                .child({
                    let s = Rc::clone(state);
                    knob(state.tone.get())
                        .id("v.knob")
                        .range(0.0, 100.0)
                        .name("Tone")
                        .on_change(move |x| s.tone.set(x))
                })
                .child({
                    let s = Rc::clone(state);
                    knob(state.width.get())
                        .id("v.width")
                        .range(-100.0, 100.0)
                        .default_value(0.0)
                        .bipolar(true)
                        .name("Width")
                        .on_change(move |x| s.width.set(x))
                })
                .child(knob(40.0).id("v.dis").range(0.0, 100.0).name("Locked").disabled(true))
                .child(
                    div()
                        .flex_col()
                        .gap(theme.spacing.xs)
                        .child(readout(theme, "Level", &format!("{:.0}", state.level.get())))
                        .child(readout(theme, "Tone", &format!("{:.0}", state.tone.get())))
                        .child(readout(theme, "Width", &format!("{:+.0}", state.width.get())))
                        .child(readout(theme, "Locked", "disabled")),
                )
                .into_element(),
        ))
        .into_element()
}

fn text(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;

    page(theme, Page::Text)
        .child(specimen(
            theme,
            "Fields",
            "Real editing: selection, an input method, and a submit that commits.",
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .child({
                    let s = Rc::clone(state);
                    let commit = Rc::clone(state);
                    text_field(state.name_field.borrow().clone())
                        .id("t.name")
                        .placeholder("your name")
                        .on_change(move |e| *s.name_field.borrow_mut() = e.clone())
                        .on_submit(move |t| commit.say(format!("Name set to {t}.")))
                        .on_context_menu({
                            let s = Rc::clone(state);
                            move |at| s.open_edit_menu(1, at)
                        })
                })
                .child({
                    let s = Rc::clone(state);
                    text_field(state.secret_field.borrow().clone())
                        .id("t.secret")
                        .placeholder("passphrase")
                        .mask(true)
                        .on_change(move |e| *s.secret_field.borrow_mut() = e.clone())
                        .on_context_menu({
                            let s = Rc::clone(state);
                            move |at| s.open_edit_menu(2, at)
                        })
                })
                .child(
                    label("Try a Thai or Japanese input method in the first field — the composition stays underlined until it is committed.")
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Type scale",
            "Five sizes, and the two smallest cross into the bitmap fallback on a 1x display.",
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(type_row(theme, "Extra small", theme.typography.xs))
                .child(type_row(theme, "Small", theme.typography.sm))
                .child(type_row(theme, "Medium", theme.typography.md))
                .child(type_row(theme, "Large", theme.typography.lg))
                .child(type_row(theme, "Extra large", theme.typography.xl))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Weights",
            "Semibold rather than bold for emphasis: at interface sizes bold overshoots.",
            row(theme)
                .child(
                    label("Regular 400")
                        .text_size(theme.typography.md)
                        .weight(spherekit::text::FontWeight::NORMAL)
                        .text_color(c.text),
                )
                .child(
                    label("SemiBold 600")
                        .text_size(theme.typography.md)
                        .weight(spherekit::text::FontWeight::SEMI_BOLD)
                        .text_color(c.text),
                )
                .child(
                    label("Bold 700")
                        .text_size(theme.typography.md)
                        .weight(spherekit::text::FontWeight::BOLD)
                        .text_color(c.text),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Scripts",
            "One shaping pass, one atlas. Nothing here is a special case in the renderer.",
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(
                    label("ไทย · 日本語 · 中文 · 한국어 · العربية · Ελληνικά")
                        .text_size(theme.typography.md)
                        .text_color(c.text),
                )
                .child(
                    label("สวัสดีครับ — ทดสอบการเรนเดอร์ข้อความภาษาไทย")
                        .text_size(theme.typography.md)
                        .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .into_element()
}

/// The landing page: what this is, what the renderer is doing, and a way in.
///
/// Deliberately not a specimen page. Every other page here is a catalogue, and
/// a catalogue is a bad first screen: it answers "what is there" before anyone
/// has asked "what is this". So this one is a real composition — a heading, the
/// renderer reporting on itself, and a card per page — built from the same
/// widgets the catalogue documents.
fn home(
    state: &Rc<State>,
    theme: &Theme,
    adapter: &str,
    stats: &spherekit::SurfaceStats,
    icons: &[(Page, spherekit::core::SvgId)],
) -> AnyElement {
    let c = theme.colors;

    div()
        .flex_col()
        .gap(theme.spacing.xl)
        // --- the hero ------------------------------------------------------
        .child(
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .child(
                    div()
                        .flex_row()
                        .items_center()
                        .gap(theme.spacing.md)
                        .child(
                            label("SphereKit UI")
                                .text_size(px(34.0))
                                .weight(theme.typography.strong)
                                .text_color(c.text),
                        )
                        .child(badge("2026.8").variant(BadgeVariant::Accent)),
                )
                .child(
                    label(
                        "A retained element tree with an immediate-looking API, drawn by one \
                         analytic quad pipeline. Everything on these pages is live: the toggles \
                         toggle, the sliders drag, and the state they write is the state the \
                         next frame reads.",
                    )
                    .text_size(theme.typography.md)
                    .text_color(c.text_muted)
                    .max_w(px(620.0)),
                )
                .child(div().h(theme.spacing.xs))
                .child(
                    row(theme)
                        .child({
                            let s = Rc::clone(state);
                            button("Explore the widgets")
                                .id("home.tour")
                                .variant(ButtonVariant::Primary)
                                .on_press(move || {
                                    s.page.set(Page::Buttons);
                                    s.say("Buttons.");
                                })
                        })
                        .child({
                            let s = Rc::clone(state);
                            button("Raise a toast")
                                .id("home.toast")
                                .variant(ButtonVariant::Outline)
                                .on_press(move || {
                                    s.pending_toast.set(Some((ToastVariant::Success, "Welcome")));
                                })
                        }),
                ),
        )
        // --- what the renderer is doing right now --------------------------
        .child(
            div()
                .flex_row()
                .gap(theme.spacing.md)
                .child(stat(theme, "Adapter", adapter.split('(').next().unwrap_or(adapter).trim()))
                .child(stat(theme, "Draw calls", &stats.frame.draw_calls.to_string()))
                .child(stat(theme, "Elements", &stats.tree.elements.to_string()))
                .child(stat(theme, "CPU / frame", &format!("{:.2} ms", stats.cpu_ms))),
        )
        // --- a card per page -----------------------------------------------
        .child(
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .child(
                    label("PAGES")
                        .text_size(theme.typography.xs)
                        .weight(theme.typography.strong)
                        .text_color(c.text_muted),
                )
                .child({
                    let mut grid = wrapping_row(theme);
                    for page in Page::tour() {
                        grid = grid.child(page_card(state, theme, page, icons));
                    }
                    grid
                }),
        )
        .into_element()
}

/// One number the renderer is reporting, in a card.
fn stat(theme: &Theme, name: &str, value: &str) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_col()
        .flex_1()
        .min_w(px(0.0))
        .gap(theme.spacing.xs)
        .p(theme.spacing.md)
        .rounded(theme.radii.lg)
        .bg(c.surface)
        .border(px(1.0), c.border)
        .child(
            label(name.to_string())
                .text_size(theme.typography.xs)
                .text_color(c.text_muted)
                .no_wrap(),
        )
        .child(
            label(value.to_string())
                .text_size(theme.typography.md)
                .weight(theme.typography.strong)
                .text_color(c.text)
                .truncate(),
        )
        .into_element()
}

/// A card that navigates to one page.
fn page_card(
    state: &Rc<State>,
    theme: &Theme,
    page: Page,
    icons: &[(Page, spherekit::core::SvgId)],
) -> AnyElement {
    let c = theme.colors;
    let s = Rc::clone(state);
    let svg = icons.iter().find(|(p, _)| *p == page).map(|(_, id)| *id);
    div()
        .id(("home.card", page.title()))
        .focusable()
        .flex_col()
        .w(px(224.0))
        .gap(theme.spacing.xs)
        .p(theme.spacing.md)
        .rounded(theme.radii.lg)
        .bg(c.surface)
        .border(px(1.0), c.border)
        .hover_bg(c.hover)
        .active_bg(c.pressed)
        .cursor(Cursor::Pointer)
        .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
        .semantics(Semantics::new(Role::Button, page.title()))
        .child(
            div()
                .flex_row()
                .items_center()
                .gap(theme.spacing.sm)
                .child(crate::IconElement { svg, tint: c.accent, size: px(16.0) })
                .child(
                    label(page.title())
                        .text_size(theme.typography.md)
                        .weight(theme.typography.strong)
                        .text_color(c.text)
                        .no_wrap(),
                ),
        )
        .child(label(page.blurb()).text_size(theme.typography.sm).text_color(c.text_muted))
        .on_click(move |cx: &mut EventContext<'_>| {
            s.page.set(page);
            cx.notify_layout();
        })
        .into_element()
}

/// A row whose children wrap onto the next line when they run out of room.
///
/// `Styled` has no `flex_wrap`, so this reaches for the style directly. Wrapping
/// is the one flex property a card grid genuinely needs and the one a fixed
/// gallery layout would otherwise have to fake with a column count.
fn wrapping_row(theme: &Theme) -> spherekit::ui::Div {
    let mut row = div().flex_row().gap(theme.spacing.md);
    row.style_mut().flex_wrap = spherekit::layout::FlexWrap::Wrap;
    row
}

fn overlays(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let open = state.popover.get().value();
    let side = match state.popover_side.get() {
        0 => PopoverSide::Top,
        2 => PopoverSide::Left,
        3 => PopoverSide::Right,
        _ => PopoverSide::Bottom,
    };

    page(theme, Page::Overlays)
        .child(specimen(
            theme,
            "Overlay",
            "A scrim dims what is behind it and swallows every event that reaches it. The second half is the one that is easy to forget: a dialog over a page whose buttons still work is a picture of a modal, not a modal.",
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .items(spherekit::layout::Align::Start)
                .child({
                    let s = Rc::clone(state);
                    button("Delete take\u{2026}")
                        .id("o.modal")
                        .variant(ButtonVariant::Danger)
                        .on_press(move || {
                            s.dialog_open.set(true);
                            s.say("Dialog open \u{2014} click away or press Escape.");
                        })
                })
                .child(
                    label("An overlay fills its *parent*, so this one is built at the root of the tree and covers the window. One built inside a card would cover the card, which is how a local \u{201c}are you sure?\u{201d} is done.")
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Popover",
            "Anchored to its parent, on any of four sides, with an optional beak. There is no centre alignment: centring a box of unknown width over its anchor needs a transform the layout engine does not have, and the widget will not pretend otherwise.",
            div()
                .flex_col()
                .gap(theme.spacing.lg)
                .items(spherekit::layout::Align::Start)
                .child({
                    let s = Rc::clone(state);
                    segmented(state.popover_side.get())
                        .id("o.side")
                        .name("Popover side")
                        .item("Top")
                        .item("Bottom")
                        .item("Left")
                        .item("Right")
                        .on_select(move |i| s.popover_side.set(i))
                })
                // Room around the trigger for the panel to open into, which is
                // the caller's job: a popover does not reserve space, it floats.
                .child(
                    div()
                        .h(px(150.0))
                        .w(relative(1.0))
                        .flex_row()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .flex_col()
                                .child({
                                    let s = Rc::clone(state);
                                    button("Anchor")
                                        .id("o.trigger")
                                        .variant(ButtonVariant::Outline)
                                        .on_press(move || {
                                            s.popover_open.set(!s.popover_open.get());
                                        })
                                })
                                .child(
                                    popover(open)
                                        .id("o.pop")
                                        .side(side)
                                        .arrow(true)
                                        .align(spherekit::ui::PopoverAlign::Start)
                                        .w(px(220.0))
                                        .p(theme.spacing.md)
                                        .gap(theme.spacing.xs)
                                        .child(
                                            label("Analytic shadows")
                                                .scale(TypeScale::Sm)
                                                .weight(theme.typography.strong),
                                        )
                                        .child(
                                            label("This panel casts one. It is a Gaussian solved in the fragment shader, not a blur pass.")
                                                .scale(TypeScale::Sm)
                                                .role(TextRole::Muted),
                                        ),
                                ),
                        ),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Toast",
            "The widget draws one toast. When it appears, how long it stays and how many are on screen are product decisions with no defensible default, so the application owns the list \u{2014} each entry with its own spring, so one leaving never interrupts the two above it.",
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .items(spherekit::layout::Align::Start)
                .child(
                    row(theme)
                        .child(toast_button(state, "o.t1", ToastVariant::Info, "Nothing to do"))
                        .child(toast_button(state, "o.t2", ToastVariant::Success, "Sync complete"))
                        .child(toast_button(state, "o.t3", ToastVariant::Warning, "Partly done"))
                        .child(toast_button(state, "o.t4", ToastVariant::Danger, "Upload failed")),
                )
                .child(
                    label(format!(
                        "{} on screen. They stack in the bottom-right, retire after four and a half seconds, and the oldest leaves early once there are three.",
                        state.toasts.borrow().len()
                    ))
                    .text_size(theme.typography.sm)
                    .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .into_element()
}

/// One button that raises a toast of a given variant.
fn toast_button(
    state: &Rc<State>,
    key: &'static str,
    variant: ToastVariant,
    title: &'static str,
) -> AnyElement {
    let s = Rc::clone(state);
    button(title)
        .id(key)
        .variant(match variant {
            ToastVariant::Danger => ButtonVariant::Danger,
            ToastVariant::Info => ButtonVariant::Outline,
            _ => ButtonVariant::Secondary,
        })
        // Queued rather than pushed: a toast needs the frame's clock reading to
        // know when it was born, and a callback has no clock.
        .on_press(move || s.pending_toast.set(Some((variant, title))))
        .into_element()
}

fn colour(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let tint = state.tint.get();
    let chosen = tint.to_color();

    page(theme, Page::Colour)
        .child(specimen(
            theme,
            "Picker",
            "Square, ramps, readout and presets. Every part reports the whole colour, so the page stores one value rather than four.",
            div()
                .flex_row()
                .gap(theme.spacing.lg)
                .child(
                    div().flex_1().min_w(px(0.0)).child({
                        let s = Rc::clone(state);
                        color_picker(tint).id("col.picker").alpha(true).on_change(move |v| {
                            s.tint.set(v);
                        })
                    }),
                )
                .child(
                    card(theme)
                        .w(px(180.0))
                        .child(
                            div()
                                .h(px(64.0))
                                .w(relative(1.0))
                                .rounded(theme.radii.md)
                                .bg(chosen)
                                .border(px(1.0), c.border),
                        )
                        .child(readout(theme, "Hex", &tint.hex(true)))
                        .child(readout(theme, "Hue", &format!("{:.0}\u{00B0}", tint.h * 360.0)))
                        .child(readout(theme, "Sat", &format!("{:.0}%", tint.s * 100.0)))
                        .child(readout(theme, "Val", &format!("{:.0}%", tint.v * 100.0)))
                        .child(readout(theme, "Alpha", &format!("{:.0}%", tint.a * 100.0))),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "The parts, on their own",
            "A levels panel wants the hue ramp and nothing else. The square, the ramps and the chips are separate widgets for exactly that reason.",
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .child({
                    let s = Rc::clone(state);
                    color_area(tint)
                        .id("col.area")
                        .size(px(260.0), px(120.0))
                        .on_change(move |v| s.tint.set(v))
                })
                .child({
                    let s = Rc::clone(state);
                    hue_slider(tint).id("col.hue").on_change(move |v| s.tint.set(v))
                })
                .child({
                    let s = Rc::clone(state);
                    alpha_slider(tint).id("col.alpha").on_change(move |v| s.tint.set(v))
                })
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Swatches",
            "A chip is a button that happens to be a colour. Transparency is drawn over a checkerboard, because a swatch on the surface colour cannot show it any other way.",
            row(theme)
                .child(swatch_pick(state, "col.s1", Color::hex(0x78A8E8)))
                .child(swatch_pick(state, "col.s2", Color::hex(0x79B88A)))
                .child(swatch_pick(state, "col.s3", Color::hex(0xD5AF68)))
                .child(swatch_pick(state, "col.s4", Color::hex(0xD77880)))
                .child(swatch_pick(state, "col.s5", Color::hex(0xB490E0)))
                .child(div().w(px(12.0)))
                .child(swatch_pick(state, "col.s6", Color::hex(0x78A8E8).with_alpha(0.35)))
                .child(swatch_pick(state, "col.s7", Color::WHITE.with_alpha(0.12)))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Why HSV and not the theme's HSL",
            "Drag to the bottom of the square: the colour is black, and the hue is still whatever you were working in. A picker that stored the resulting colour would have forgotten it.",
            div()
                .flex_row()
                .items_center()
                .gap(theme.spacing.md)
                .child(
                    div()
                        .size(px(40.0))
                        .rounded(theme.radii.md)
                        .bg(tint.pure_hue())
                        .border(px(1.0), c.border),
                )
                .child(
                    label(format!(
                        "hue {:.0}\u{00B0} is kept whatever the square says \u{2014} the swatch on the left never goes black.",
                        tint.h * 360.0
                    ))
                    .text_size(theme.typography.sm)
                    .text_color(c.text_muted),
                )
                .into_element(),
        ))
        .into_element()
}

fn dates(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let month = state.cal_month.get();
    let selected = state.cal_day.get();
    let open = state.date_menu.get().value();

    page(theme, Page::Dates)
        .child(specimen(
            theme,
            "Calendar",
            "Click a day, or focus the grid and use the arrow keys. PageUp and PageDown change month; hold Shift for a year.",
            div()
                .flex_row()
                .gap(theme.spacing.lg)
                .child({
                    let pick = Rc::clone(state);
                    let page_to = Rc::clone(state);
                    calendar(month, selected)
                        .id("d.main")
                        .on_select(move |d| {
                            pick.cal_day.set(Some(d));
                            pick.say(format!("Selected {}.", d.long()));
                        })
                        .on_month(move |m| page_to.cal_month.set(m))
                })
                .child(
                    div()
                        .flex_col()
                        .gap(theme.spacing.xs)
                        .child(readout(
                            theme,
                            "Showing",
                            &format!("{} {}", month.month_name(), month.year()),
                        ))
                        .child(readout(
                            theme,
                            "Selected",
                            &selected.map(|d| d.iso()).unwrap_or_else(|| "none".into()),
                        ))
                        .child(readout(
                            theme,
                            "Weekday",
                            selected.map(|d| d.weekday().name()).unwrap_or("\u{2014}"),
                        ))
                        .child(readout(theme, "Today", &Date::today_utc().iso()))
                        .child(div().h(theme.spacing.sm))
                        .items(spherekit::layout::Align::Start)
                        .child({
                            let s = Rc::clone(state);
                            button("Today").id("d.today").on_press(move || {
                                let today = Date::today_utc();
                                s.cal_month.set(today.first_of_month());
                                s.cal_day.set(Some(today));
                                s.say("Jumped to today.");
                            })
                        })
                        .child({
                            let s = Rc::clone(state);
                            button("Clear").id("d.clear").on_press(move || {
                                s.cal_day.set(None);
                                s.say("Selection cleared.");
                            })
                        }),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Bounded, and a Sunday week",
            "Out-of-range days stay in place and stop responding \u{2014} a grid that hid them would change height, and the controls under it would move.",
            div()
                .flex_row()
                .gap(theme.spacing.lg)
                .child(
                    calendar(month, selected)
                        .id("d.bounded")
                        .week_start(Weekday::Sunday)
                        .min(month.first_of_month().add_days(4))
                        .max(month.last_of_month().add_days(-6))
                        .cell_size(px(30.0))
                        .today(None),
                )
                .child(
                    label("`week_start` is a display choice; `Weekday` itself is ISO and starts on Monday, so the working week is never a wrap-around range.")
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted)
                        .flex_1(),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "A range",
            "Click twice: the first click starts a span, the second closes it. The band is painted across the whole cell, so consecutive days join into one bar.",
            div()
                .flex_row()
                .gap(theme.spacing.lg)
                .child({
                    let s = Rc::clone(state);
                    let page_to = Rc::clone(state);
                    calendar(month, None)
                        .id("d.range")
                        .cell_size(px(30.0))
                        .range(state.range())
                        .on_select(move |d| s.extend_range(d))
                        .on_month(move |m| page_to.cal_month.set(m))
                })
                .child(
                    div()
                        .flex_col()
                        .gap(theme.spacing.xs)
                        .child(readout(
                            theme,
                            "From",
                            &state.range_from.get().map(|d| d.iso()).unwrap_or_else(|| "\u{2014}".into()),
                        ))
                        .child(readout(
                            theme,
                            "To",
                            &state.range_to.get().map(|d| d.iso()).unwrap_or_else(|| "\u{2014}".into()),
                        ))
                        .child(readout(theme, "Nights", &state.nights()))
                        .child(div().h(theme.spacing.sm))
                        .items(spherekit::layout::Align::Start)
                        .child({
                            let s = Rc::clone(state);
                            button("Reset").id("d.reset").on_press(move || {
                                s.range_from.set(None);
                                s.range_to.set(None);
                                s.say("Range cleared.");
                            })
                        }),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "In a dropdown",
            "Nothing about the calendar knows it is in a popover. The dropdown anchors to its parent and the grid is just a child of it.",
            div()
                .flex_col()
                .w(px(260.0))
                .child(
                    div()
                        .id("d.trigger")
                        .focusable()
                        .flex_row()
                        .items_center()
                        .gap(theme.spacing.md)
                        .h(px(34.0))
                        .px_(theme.spacing.sm)
                        .rounded(theme.radii.md)
                        .bg(if state.date_menu_open.get() { c.pressed } else { c.surface })
                        .hover_bg(c.hover)
                        .active_bg(c.pressed)
                        .cursor(Cursor::Pointer)
                        .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
                        .semantics(Semantics::new(Role::Button, "Choose a date"))
                        .child(
                            label(
                                selected
                                    .map(|d| d.long())
                                    .unwrap_or_else(|| "Choose a date".to_string()),
                            )
                            .text_size(theme.typography.sm)
                            .text_color(if selected.is_some() { c.text } else { c.text_muted })
                            .flex_1()
                            .no_wrap(),
                        )
                        .child(crate::ChevronElement { tint: c.text_muted, open })
                        .on_click({
                            let s = Rc::clone(state);
                            move |cx: &mut EventContext<'_>| {
                                s.date_menu_open.set(!s.date_menu_open.get());
                                cx.notify();
                            }
                        }),
                )
                .child(
                    dropdown(open).below().offset(theme.spacing.sm).p(theme.spacing.sm).child({
                        let s = Rc::clone(state);
                        let page_to = Rc::clone(state);
                        calendar(month, selected)
                            .id("d.popover")
                            .cell_size(px(30.0))
                            .on_select(move |d| {
                                s.cal_day.set(Some(d));
                                s.date_menu_open.set(false);
                                s.say(format!("Picked {} from the popover.", d.iso()));
                            })
                            .on_month(move |m| page_to.cal_month.set(m))
                    }),
                )
                .into_element(),
        ))
        .into_element()
}

fn identity(state: &Rc<State>, theme: &Theme) -> AnyElement {
    let c = theme.colors;
    let open = state.demo_menu.get().value();

    page(theme, Page::Identity)
        .child(specimen(
            theme,
            "Derived tints",
            "The tint comes from the name, so one person is one colour everywhere — nothing stored.",
            row(theme)
                .child(avatar("Ada Lovelace").ring(c.mica_surface))
                .child(avatar("Grace Hopper").ring(c.mica_surface))
                .child(avatar("Alan Turing").ring(c.mica_surface))
                .child(avatar("Katherine Johnson").ring(c.mica_surface))
                .child(avatar("Radia Perlman").ring(c.mica_surface))
                .child(avatar("นักพัฒนา ไทย").ring(c.mica_surface))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Size and override",
            "Any diameter; an explicit colour or explicit initials when the derivation reads wrongly.",
            row(theme)
                .items_center()
                .child(avatar(USER_NAME).size(px(20.0)).ring(c.mica_surface))
                .child(avatar(USER_NAME).size(px(28.0)).ring(c.mica_surface))
                .child(avatar(USER_NAME).size(px(40.0)).ring(c.mica_surface))
                .child(avatar(USER_NAME).size(px(56.0)).ring(c.mica_surface))
                .child(div().w(px(16.0)))
                .child(avatar("Prince").size(px(40.0)).color(c.accent).ring(c.mica_surface))
                .child(
                    avatar("山田 太郎")
                        .size(px(40.0))
                        .initials("YT")
                        .ring(c.mica_surface),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Presence",
            "The dot is cut out of whatever is behind it, so `ring` has to name that colour.",
            row(theme)
                .items_center()
                .child(presence_chip(theme, "Online", Presence::Online))
                .child(presence_chip(theme, "Away", Presence::Away))
                .child(presence_chip(theme, "Busy", Presence::Busy))
                .child(presence_chip(theme, "Offline", Presence::Offline))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Dropdown",
            "Driven by one spring in `0..=1`. It opens downward here and upward in the footer.",
            div()
                .flex_col()
                .w(px(260.0))
                .child(
                    div()
                        .id("i.trigger")
                        .focusable()
                        .flex_row()
                        .items_center()
                        .gap(theme.spacing.md)
                        .h(px(44.0))
                        .px_(theme.spacing.sm)
                        .rounded(theme.radii.md)
                        .bg(if state.demo_menu_open.get() { c.pressed } else { c.surface })
                        .hover_bg(c.hover)
                        .active_bg(c.pressed)
                        .cursor(Cursor::Pointer)
                        .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
                        .semantics(Semantics::new(Role::Button, "Demo menu"))
                        .child(avatar(USER_NAME).size(px(28.0)).presence(Presence::Online).ring(c.surface))
                        .child(
                            div()
                                .flex_col()
                                .flex_1()
                                .min_w(px(0.0))
                                .child(
                                    label(USER_NAME)
                                        .text_size(theme.typography.sm)
                                        .weight(theme.typography.strong)
                                        .text_color(c.text)
                                        .no_wrap(),
                                )
                                .child(
                                    label(USER_EMAIL)
                                        .text_size(theme.typography.xs)
                                        .text_color(c.text_muted)
                                        .no_wrap(),
                                ),
                        )
                        // Down when collapsed, up when expanded — the same way
                        // round as the footer's, even though this panel opens
                        // the other way. The chevron reports state, not travel.
                        .child(crate::ChevronElement { tint: c.text_muted, open })
                        .on_click({
                            let s = Rc::clone(state);
                            move |cx: &mut EventContext<'_>| {
                                s.demo_menu_open.set(!s.demo_menu_open.get());
                                cx.notify();
                            }
                        }),
                )
                .child(
                    dropdown(open)
                        .below()
                        .offset(theme.spacing.sm)
                        .p(theme.spacing.xs)
                        .gap(theme.spacing.xs)
                        .child(demo_menu_item(state, theme, "i.one", "Open profile"))
                        .child(demo_menu_item(state, theme, "i.two", "Switch account"))
                        .child(separator(false).bg(c.border).m(theme.spacing.xs))
                        .child(demo_menu_item(state, theme, "i.three", "Sign out")),
                )
                .into_element(),
        ))
        .into_element()
}

fn containers(
    state: &Rc<State>,
    theme: &Theme,
    adapter: &str,
    stats: &spherekit::SurfaceStats,
) -> AnyElement {
    let c = theme.colors;

    page(theme, Page::Containers)
        .child(specimen(
            theme,
            "Panel",
            "A surface with a border and a radius. Nothing privileged — it is a styled div.",
            row(theme)
                .child(
                    card(theme)
                        .w(px(200.0))
                        .child(
                            label("Surface")
                                .text_size(theme.typography.md)
                                .weight(theme.typography.strong)
                                .text_color(c.text),
                        )
                        .child(
                            label("Sits on the window background.")
                                .text_size(theme.typography.sm)
                                .text_color(c.text_muted),
                        ),
                )
                .child(
                    card(theme)
                        .w(px(200.0))
                        .bg(c.elevated)
                        .child(
                            label("Elevated")
                                .text_size(theme.typography.md)
                                .weight(theme.typography.strong)
                                .text_color(c.text),
                        )
                        .child(
                            label("Sits on a surface.")
                                .text_size(theme.typography.sm)
                                .text_color(c.text_muted),
                        ),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Progress",
            "Determinate when the extent is known; indeterminate when it is not. A bar that shows 0% for unknown work is a worse lie than one that admits it does not know.",
            div()
                .flex_col()
                .gap(theme.spacing.sm)
                .child(progress(state.download.get()).w(relative(1.0)))
                .child(
                    div()
                        .flex_row()
                        .items_center()
                        .gap(theme.spacing.md)
                        .child({
                            let s = Rc::clone(state);
                            button("Restart").id("k.restart").on_press(move || {
                                s.download.set(0.0);
                                s.say("Download restarted.");
                            })
                        })
                        .child(
                            label(if state.download.get() >= 1.0 {
                                "Complete.".to_string()
                            } else {
                                format!("{:.0}%", state.download.get() * 100.0)
                            })
                            .text_size(theme.typography.sm)
                            .text_color(c.text_muted),
                        ),
                )
                .child(div().h(theme.spacing.sm))
                .child(
                    label("Indeterminate — a shuttle on a fixed loop, eased so it settles at each end rather than snapping back.")
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                )
                .child(progress_indeterminate().id("k.busy"))
                .child(
                    div()
                        .flex_row()
                        .items_center()
                        .gap(theme.spacing.md)
                        .child({
                            let s = Rc::clone(state);
                            let on = state.busy.get();
                            toggle(on).id("k.busytoggle").label("Running").on_change(move |v| {
                                s.busy.set(v);
                                s.say(if v { "Task running." } else { "Task paused." });
                            })
                        })
                        .child(
                            label(if state.busy.get() {
                                "Running — the page holds the frame loop open while this is on."
                            } else {
                                "Paused — the shuttle is a function of paint time, so it stops when frames do."
                            })
                            .text_size(theme.typography.sm)
                            .text_color(c.text_muted),
                        ),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Spinner and badges",
            "A spinner is a function of paint time, like the bar above it. A badge is tinted rather than filled, because it annotates content rather than competing with it.",
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .child(
                    row(theme)
                        .child(spinner().id("k.spin"))
                        .child(spinner().id("k.spin.lg").size(px(28.0)).thickness(px(3.0)))
                        .child(
                            spinner().id("k.spin.warn").size(px(16.0)).color(theme.colors.warning),
                        )
                        .child(
                            label("Only spins while frames keep coming \u{2014} the Running switch above holds the loop open.")
                                .text_size(theme.typography.sm)
                                .text_color(c.text_muted),
                        ),
                )
                .child(
                    row(theme)
                        .child(badge("Neutral"))
                        .child(badge("New").variant(BadgeVariant::Accent))
                        .child(badge("Passing").variant(BadgeVariant::Success).dot(true))
                        .child(badge("Deprecated").variant(BadgeVariant::Warning))
                        .child(badge("Failed").variant(BadgeVariant::Danger).dot(true))
                        .child(badge("12")),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Tooltip",
            "Driven by one spring, exactly as a dropdown is. The widget owns no timer: when a hover has been earned is a product decision, and this one gives it a moment.",
            div()
                .flex_row()
                .child(
                    div()
                        .flex_col()
                        .child(
                            button("Delete take").id("k.tip").variant(spherekit::ui::ButtonVariant::Danger),
                        )
                        .child(tooltip("Removes the take permanently", state.hint.get().value()))
                        .on_mouse_enter({
                            let s = Rc::clone(state);
                            move |cx: &mut EventContext<'_>| {
                                s.hint_hovered.set(true);
                                cx.notify();
                            }
                        })
                        .on_mouse_leave({
                            let s = Rc::clone(state);
                            move |cx: &mut EventContext<'_>| {
                                s.hint_hovered.set(false);
                                cx.notify();
                            }
                        }),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Separator",
            "One *logical* pixel, so it antialiases at 150% rather than drifting off the grid.",
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .child(separator(false).bg(c.border))
                .child(
                    div()
                        .flex_row()
                        .h(px(28.0))
                        .items_center()
                        .gap(theme.spacing.md)
                        .child(label("left").text_size(theme.typography.sm).text_color(c.text))
                        .child(separator(true).bg(c.border))
                        .child(label("right").text_size(theme.typography.sm).text_color(c.text)),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Scroll view",
            "Overlay bars: they draw over the content, so showing them never changes what the content is laid out into. Wheel over the list, or drag the thumb.",
            scroll_area().id("k.scroll").w(relative(1.0)).h(px(160.0)).child({
                    let mut list = div().flex_col().p(theme.spacing.sm).gap(theme.spacing.xs);
                    for i in 1..=24 {
                        list = list.child(
                            div()
                                .id(("k.row", i))
                                .flex_row()
                                .items_center()
                                .h(px(26.0))
                                .px_(theme.spacing.sm)
                                .rounded(theme.radii.sm)
                                .hover_bg(c.hover)
                                .child(
                                    label(format!("Row {i:02}"))
                                        .text_size(theme.typography.sm)
                                        .text_color(c.text)
                                        .no_wrap(),
                                ),
                        );
                    }
                    list
                })
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Horizontal and both axes",
            "The axis is the container's, not the wheel's — a vertical wheel scrolls a horizontal strip.",
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .child(
                    scroll_area().id("k.hscroll").horizontal(true).w(relative(1.0)).h(px(72.0)).child({
                        let mut strip = div().flex_row().p(theme.spacing.sm).gap(theme.spacing.sm);
                        for i in 1..=14 {
                            strip = strip.child(
                                div()
                                    .id(("k.card", i))
                                    .shrink(0.0)
                                    .w(px(96.0))
                                    .h(px(44.0))
                                    .rounded(theme.radii.md)
                                    .bg(c.elevated)
                                    .border(px(1.0), c.border)
                                    .items_center()
                                    .justify_center()
                                    .child(
                                        label(format!("Card {i:02}"))
                                            .text_size(theme.typography.sm)
                                            .text_color(c.text)
                                            .no_wrap(),
                                    ),
                            );
                        }
                        strip
                    }),
                )
                .child(
                    label("`scrollbars(Always)` keeps the bar even when nothing overflows, so a pane's width never jumps as content grows.")
                        .text_size(theme.typography.sm)
                        .text_color(c.text_muted),
                )
                .child(
                    scroll_area()
                        .id("k.always")
                        .scrollbars(spherekit::ui::ScrollbarPolicy::Always)
                        .w(relative(1.0))
                        .h(px(72.0))
                        .child(
                            div().flex_col().p(theme.spacing.sm).child(
                                label("This fits — the bar is shown anyway.")
                                    .text_size(theme.typography.sm)
                                    .text_color(c.text),
                            ),
                        ),
                )
                .into_element(),
        ))
        .child(specimen(
            theme,
            "This frame",
            "The gallery reporting on itself, straight from the renderer.",
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .child(info_row(theme, "Adapter", adapter))
                .child(info_row(theme, "Draw calls", &stats.frame.draw_calls.to_string()))
                .child(info_row(theme, "Quad instances", &stats.frame.quads.to_string()))
                .child(info_row(theme, "Glyph instances", &stats.frame.glyphs.to_string()))
                .child(info_row(theme, "Mesh triangles", &stats.frame.triangles.to_string()))
                .child(info_row(theme, "Offscreen layers", &stats.frame.layers.to_string()))
                .child(info_row(theme, "Elements built", &stats.tree.elements.to_string()))
                .child(info_row(theme, "Elements culled", &stats.tree.elements_culled.to_string()))
                .child(info_row(theme, "Nodes relaid out", &stats.nodes_laid_out.to_string()))
                .child(info_row(theme, "CPU per frame", &format!("{:.2} ms", stats.cpu_ms)))
                .into_element(),
        ))
        .into_element()
}

fn palette(theme: &Theme) -> AnyElement {
    let c = theme.colors;

    page(theme, Page::Palette)
        .child(specimen(
            theme,
            "Surfaces",
            "Background, then each step that sits on top of the one before it.",
            swatches(
                theme,
                &[
                    ("background", c.background),
                    ("mica_surface", c.mica_surface),
                    ("surface", c.surface),
                    ("elevated", c.elevated),
                ],
            ),
        ))
        .child(specimen(
            theme,
            "Interaction",
            "Hover and pressed are states of a surface, not colours a widget picks for itself.",
            swatches(theme, &[("hover", c.hover), ("pressed", c.pressed), ("focus", c.focus)]),
        ))
        .child(specimen(
            theme,
            "Lines",
            "A hairline and the stronger one a focused or selected container earns.",
            swatches(theme, &[("border", c.border), ("border_strong", c.border_strong)]),
        ))
        .child(specimen(
            theme,
            "Text",
            "Three roles. `text_on_accent` exists so a filled button never has to guess.",
            swatches(
                theme,
                &[
                    ("text", c.text),
                    ("text_muted", c.text_muted),
                    ("text_on_accent", c.text_on_accent),
                ],
            ),
        ))
        .child(specimen(
            theme,
            "Accent and semantics",
            "Semantic rather than named: swapping the theme swaps meaning, not hex codes.",
            swatches(
                theme,
                &[
                    ("accent", c.accent),
                    ("accent_hover", c.accent_hover),
                    ("success", c.success),
                    ("warning", c.warning),
                    ("danger", c.danger),
                ],
            ),
        ))
        .child(specimen(
            theme,
            "Radii and spacing",
            "Tokens, so a product restyles by changing four numbers rather than four hundred.",
            div()
                .flex_col()
                .gap(theme.spacing.md)
                .child(
                    row(theme)
                        .items_center()
                        .child(radius_chip(theme, "sm", theme.radii.sm))
                        .child(radius_chip(theme, "md", theme.radii.md))
                        .child(radius_chip(theme, "lg", theme.radii.lg)),
                )
                .child(
                    row(theme)
                        .items_center()
                        .child(space_chip(theme, "xs", theme.spacing.xs))
                        .child(space_chip(theme, "sm", theme.spacing.sm))
                        .child(space_chip(theme, "md", theme.spacing.md))
                        .child(space_chip(theme, "lg", theme.spacing.lg))
                        .child(space_chip(theme, "xl", theme.spacing.xl)),
                )
                .into_element(),
        ))
        .into_element()
}

// ---------------------------------------------------------------------------
// Building blocks
// ---------------------------------------------------------------------------

/// The outer column of a page, with its heading already in place.
fn page(theme: &Theme, which: Page) -> spherekit::ui::Div {
    let c = theme.colors;
    div().flex_col().gap(theme.spacing.xl).child(
        div()
            .flex_col()
            .gap(theme.spacing.xs)
            .child(
                label(which.title())
                    .text_size(theme.typography.xl)
                    .weight(theme.typography.strong)
                    .text_color(c.text),
            )
            .child(label(which.blurb()).text_size(theme.typography.md).text_color(c.text_muted)),
    )
}

/// One titled block: a name, a note, and the live controls.
fn specimen(theme: &Theme, title: &str, note: &str, body: AnyElement) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_col()
        .gap(theme.spacing.sm)
        .child(
            label(title)
                .text_size(theme.typography.md)
                .weight(theme.typography.strong)
                .text_color(c.text),
        )
        .child(label(note).text_size(theme.typography.sm).text_color(c.text_muted))
        .child(div().h(theme.spacing.xs))
        .child(body)
        .into_element()
}

/// A wrapping row of specimens.
fn row(theme: &Theme) -> spherekit::ui::Div {
    div().flex_row().items_center().gap(theme.spacing.md)
}

/// A bordered card.
fn card(theme: &Theme) -> spherekit::ui::Div {
    let c = theme.colors;
    div()
        .flex_col()
        .gap(theme.spacing.xs)
        .p(theme.spacing.md)
        .rounded(theme.radii.lg)
        .bg(c.surface)
        .border(px(1.0), c.border)
}

fn check_row(
    state: &Rc<State>,
    theme: &Theme,
    key: &'static str,
    text: &'static str,
    cell: &std::cell::Cell<bool>,
) -> AnyElement {
    let on = cell.get();
    let s = Rc::clone(state);
    div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.md)
        .child(checkbox(on).id(key).label(text).on_change(move |v| {
            match key {
                "c.a" => s.opt_a.set(v),
                "c.b" => s.opt_b.set(v),
                _ => s.opt_c.set(v),
            }
            s.say(format!("{text}: {v}"));
        }))
        .child(label(text).text_size(theme.typography.sm).text_color(theme.colors.text))
        .into_element()
}

/// One preset chip on the Colour page, wired into the shared value.
fn swatch_pick(state: &Rc<State>, key: &'static str, color: Color) -> AnyElement {
    let s = Rc::clone(state);
    let selected = state.tint.get().to_color().to_rgba8() == color.to_rgba8();
    color_swatch(color)
        .id(key)
        .size(px(28.0))
        .selected(selected)
        .on_select(move |c| {
            s.tint.set(Hsva::from_color(c));
            s.say(format!("Swatch {} chosen.", spherekit::ui::hex_string(c, c.a < 1.0)));
        })
        .into_element()
}

/// One row of the Selection page's radio group.
fn radio_row(
    state: &Rc<State>,
    theme: &Theme,
    index: usize,
    title: &'static str,
    note: &'static str,
) -> AnyElement {
    let s = Rc::clone(state);
    let selected = state.quality.get() == index;
    div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.md)
        .child(radio(selected).id(("q", index)).label(title).on_select(move || {
            s.quality.set(index);
            s.say(format!("Quality: {title}."));
        }))
        .child(
            div()
                .flex_col()
                .child(label(title).text_size(theme.typography.sm).text_color(theme.colors.text))
                .child(
                    label(note).text_size(theme.typography.xs).text_color(theme.colors.text_muted),
                ),
        )
        .into_element()
}

fn presence_chip(theme: &Theme, name: &str, presence: Presence) -> AnyElement {
    div()
        .flex_col()
        .items_center()
        .gap(theme.spacing.xs)
        .child(
            avatar(name)
                .size(px(40.0))
                .presence(presence)
                // The gallery pane is translucent over Mica, so that — not the
                // theme's surface — is what the dot has to be cut out of.
                .ring(theme.colors.mica_surface),
        )
        .child(label(name).text_size(theme.typography.xs).text_color(theme.colors.text_muted))
        .into_element()
}

fn demo_menu_item(
    state: &Rc<State>,
    theme: &Theme,
    key: &'static str,
    text: &'static str,
) -> AnyElement {
    let c = theme.colors;
    let s = Rc::clone(state);
    div()
        .id(key)
        .focusable()
        .flex_row()
        .items_center()
        .h(px(30.0))
        .px_(theme.spacing.sm)
        .rounded(theme.radii.sm)
        .hover_bg(c.hover)
        .active_bg(c.pressed)
        .cursor(Cursor::Pointer)
        .focus_ring(spherekit::ui::FocusRing { color: c.focus, ..Default::default() })
        .semantics(Semantics::new(Role::MenuItem, text))
        .child(label(text).text_size(theme.typography.sm).text_color(c.text).no_wrap())
        .on_click(move |cx: &mut EventContext<'_>| {
            s.demo_menu_open.set(false);
            s.say(format!("{text} selected."));
            cx.notify();
        })
        .into_element()
}

fn type_row(theme: &Theme, name: &str, size: Px) -> AnyElement {
    div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.md)
        .child(
            label(format!("{name} — {:.0} px", size.get()))
                .text_size(size)
                .text_color(theme.colors.text),
        )
        .into_element()
}

fn readout(theme: &Theme, name: &str, value: &str) -> AnyElement {
    div()
        .flex_row()
        .gap(theme.spacing.sm)
        .child(
            label(name)
                .text_size(theme.typography.xs)
                .text_color(theme.colors.text_muted)
                .w(px(52.0))
                .no_wrap(),
        )
        .child(
            label(value)
                .text_size(theme.typography.xs)
                .weight(theme.typography.strong)
                .text_color(theme.colors.text)
                .no_wrap(),
        )
        .into_element()
}

fn info_row(theme: &Theme, name: &str, value: &str) -> AnyElement {
    div()
        .flex_row()
        .items_center()
        .gap(theme.spacing.md)
        .child(
            label(name)
                .text_size(theme.typography.sm)
                .text_color(theme.colors.text_muted)
                .w(px(160.0))
                .no_wrap(),
        )
        .child(label(value).text_size(theme.typography.sm).text_color(theme.colors.text).no_wrap())
        .into_element()
}

/// A row of colour chips, each labelled with its token name and hex.
fn swatches(theme: &Theme, entries: &[(&str, Color)]) -> AnyElement {
    let c = theme.colors;
    let mut r = div().flex_row().gap(theme.spacing.md);
    for (name, color) in entries {
        let [rr, gg, bb, aa] = color.to_rgba8();
        let hex = if aa == 255 {
            format!("#{rr:02X}{gg:02X}{bb:02X}")
        } else {
            format!("#{rr:02X}{gg:02X}{bb:02X} · {:.0}%", f32::from(aa) / 2.55)
        };
        r = r.child(
            div()
                .flex_col()
                .gap(theme.spacing.xs)
                .w(px(112.0))
                .child(
                    div()
                        .h(px(44.0))
                        .w(relative(1.0))
                        .rounded(theme.radii.md)
                        .bg(*color)
                        .border(px(1.0), c.border),
                )
                .child(
                    label(*name)
                        .text_size(theme.typography.xs)
                        .weight(theme.typography.strong)
                        .text_color(c.text)
                        .no_wrap(),
                )
                .child(
                    label(hex).text_size(theme.typography.xs).text_color(c.text_muted).no_wrap(),
                ),
        );
    }
    r.into_element()
}

fn radius_chip(theme: &Theme, name: &str, radius: Px) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_col()
        .items_center()
        .gap(theme.spacing.xs)
        .child(div().size(px(48.0)).rounded(radius).bg(c.elevated).border(px(1.0), c.border))
        .child(
            label(format!("{name} · {:.0}", radius.get()))
                .text_size(theme.typography.xs)
                .text_color(c.text_muted),
        )
        .into_element()
}

fn space_chip(theme: &Theme, name: &str, space: Px) -> AnyElement {
    let c = theme.colors;
    div()
        .flex_col()
        .items_center()
        .gap(theme.spacing.xs)
        .child(div().h(px(24.0)).w(space).bg(c.accent).rounded(px(2.0)))
        .child(
            label(format!("{name} · {:.0}", space.get()))
                .text_size(theme.typography.xs)
                .text_color(c.text_muted),
        )
        .into_element()
}

fn pan_text(v: f32) -> String {
    if v.abs() < 0.005 {
        "centre".to_string()
    } else if v < 0.0 {
        format!("{:.0}% left", -v * 100.0)
    } else {
        format!("{:.0}% right", v * 100.0)
    }
}
