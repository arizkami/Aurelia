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
    AnyElement, Cursor, EventContext, Interactive, IntoElement, ParentElement, Presence, Role,
    Semantics, Styled, StyledInteraction, Theme, avatar, button, checkbox, div, dropdown, label,
    progress, progress_indeterminate, scroll_area, separator, slider, text_field, toggle,
};

use crate::{State, USER_EMAIL, USER_NAME};

/// The pages the sidebar navigates between.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    Buttons,
    Selection,
    Values,
    Text,
    Identity,
    Containers,
    Palette,
}

impl Page {
    pub(crate) const ALL: [Page; 7] = [
        Page::Buttons,
        Page::Selection,
        Page::Values,
        Page::Text,
        Page::Identity,
        Page::Containers,
        Page::Palette,
    ];

    pub(crate) fn title(self) -> &'static str {
        match self {
            Page::Buttons => "Buttons",
            Page::Selection => "Selection",
            Page::Values => "Values",
            Page::Text => "Text",
            Page::Identity => "Identity",
            Page::Containers => "Containers",
            Page::Palette => "Palette",
        }
    }

    /// The one-line summary under the page title.
    pub(crate) fn blurb(self) -> &'static str {
        match self {
            Page::Buttons => "Push buttons: four weights, and what each one is for.",
            Page::Selection => "Switches and checkboxes — the same widget, two shapes.",
            Page::Values => "Sliders, faders and knobs over one continuous value.",
            Page::Text => "Editable fields and the type scale they sit in.",
            Page::Identity => "Avatars, presence, and the menu they hang off.",
            Page::Containers => "Panels, scrolling, separators and progress.",
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
) -> AnyElement {
    match page {
        Page::Buttons => buttons(state, theme),
        Page::Selection => selection(state, theme),
        Page::Values => values(state, theme),
        Page::Text => text(state, theme),
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
                .child(one("Ghost", V::Ghost, "b.ghost", state))
                .child(one("Danger", V::Danger, "b.danger", state))
                .into_element(),
        ))
        .child(specimen(
            theme,
            "Disabled",
            "A disabled button also leaves the tab order — it is not merely greyed out.",
            row(theme)
                .child(button("Primary").id("b.dp").variant(V::Primary).disabled(true))
                .child(button("Secondary").id("b.ds").disabled(true))
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
            "State readout",
            "Widgets own no value. What you see here is the same state the widgets above wrote.",
            label(format!(
                "wifi={}   notes={}   list={}   tag={}",
                state.wifi.get(),
                state.opt_a.get(),
                state.opt_b.get(),
                state.opt_c.get(),
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
