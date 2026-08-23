//! Declarations, turned into a [`ResolvedStyle`].
//!
//! Every property here follows one rule: a value the engine cannot interpret
//! leaves the style untouched. It is never coerced into the nearest thing that
//! parses. `width: 12` stays unset rather than becoming `12px`, and
//! `border-radius: 8px / 4px` stays unset rather than dropping the second
//! radius — because an author who writes an elliptical radius and gets a
//! circular one has been told something false about their own stylesheet.

use crate::length::{parse_length, parse_number, parse_px};
use crate::parser::{split_components, split_top_level};
use crate::{ResolvedStyle, StyleContext, color, text};
use spherekit_core::{Color, Corners, Edges, Length, Px, Shadow, Size};
use spherekit_layout::{
    Align, Display, Distribute, FlexDirection, FlexWrap, Overflow, Position, edges_all,
};
use spherekit_ui::Cursor;

/// The order in which a winning declaration is applied.
///
/// Declarations are applied sorted by `(tier, name)` rather than by source
/// order, because the cascade above has already picked one winner per property
/// and their relative order is otherwise arbitrary. Shorthands go first so a
/// longhand always has the last word: `border: 1px solid red;
/// border-left-width: 4px` has to leave the left edge at 4 px whichever way
/// round the author wrote it.
pub(crate) fn apply_tier(property: &str) -> u8 {
    match property {
        "background" | "border" | "flex" | "overflow" | "place-content" | "place-items" => 0,
        "border-radius" | "border-width" | "gap" | "inset" | "margin" | "padding" => 1,
        _ => 2,
    }
}

/// Applies one resolved declaration.
pub(crate) fn apply_declaration(
    style: &mut ResolvedStyle,
    property: &str,
    value: &str,
    context: &StyleContext,
) {
    if text::apply_text_declaration(&mut style.text, property, value, context) {
        return;
    }
    match property {
        "display" => {
            if let Some(display) = parse_display(value) {
                style.layout.display = display;
            }
        }
        "position" => {
            if let Some(position) = parse_position(value) {
                style.layout.position = position;
            }
        }
        "flex" => apply_flex_shorthand(style, value, context),
        "flex-direction" => {
            if let Some(direction) = parse_flex_direction(value) {
                style.layout.flex_direction = direction;
            }
        }
        "flex-wrap" => {
            if let Some(wrap) = parse_flex_wrap(value) {
                style.layout.flex_wrap = wrap;
            }
        }
        "flex-grow" => {
            if let Some(number) = parse_number(value, context) {
                style.layout.flex_grow = number.max(0.0);
            }
        }
        "flex-shrink" => {
            if let Some(number) = parse_number(value, context) {
                style.layout.flex_shrink = number.max(0.0);
            }
        }
        "flex-basis" => {
            if let Some(length) = parse_length(value, context) {
                style.layout.flex_basis = length;
            }
        }
        "width" => set_length(&mut style.layout.size.width, value, context),
        "height" => set_length(&mut style.layout.size.height, value, context),
        "min-width" => set_length(&mut style.layout.min_size.width, value, context),
        "min-height" => set_length(&mut style.layout.min_size.height, value, context),
        "max-width" => set_length(&mut style.layout.max_size.width, value, context),
        "max-height" => set_length(&mut style.layout.max_size.height, value, context),
        "padding" => {
            if let Some(edges) = parse_edges(value, context) {
                style.layout.padding = edges;
            }
        }
        "padding-top" => set_length(&mut style.layout.padding.top, value, context),
        "padding-right" => set_length(&mut style.layout.padding.right, value, context),
        "padding-bottom" => set_length(&mut style.layout.padding.bottom, value, context),
        "padding-left" => set_length(&mut style.layout.padding.left, value, context),
        "margin" => {
            if let Some(edges) = parse_edges(value, context) {
                style.layout.margin = edges;
            }
        }
        "margin-top" => set_length(&mut style.layout.margin.top, value, context),
        "margin-right" => set_length(&mut style.layout.margin.right, value, context),
        "margin-bottom" => set_length(&mut style.layout.margin.bottom, value, context),
        "margin-left" => set_length(&mut style.layout.margin.left, value, context),
        "border-width" => {
            if let Some(edges) = parse_edges(value, context) {
                style.layout.border = edges;
                style.border_width = uniform_edge(edges);
            }
        }
        "border-top-width" => set_length(&mut style.layout.border.top, value, context),
        "border-right-width" => set_length(&mut style.layout.border.right, value, context),
        "border-bottom-width" => set_length(&mut style.layout.border.bottom, value, context),
        "border-left-width" => set_length(&mut style.layout.border.left, value, context),
        "gap" => {
            if let Some((column, row)) = parse_gap(value, context) {
                style.layout.gap = Size::new(column, row);
            }
        }
        "column-gap" => {
            if let Some(column) = parse_px(value, context) {
                style.layout.gap.width = column;
            }
        }
        "row-gap" => {
            if let Some(row) = parse_px(value, context) {
                style.layout.gap.height = row;
            }
        }
        "inset" => {
            if let Some(edges) = parse_edges(value, context) {
                style.layout.inset = edges;
            }
        }
        "top" => set_length(&mut style.layout.inset.top, value, context),
        "right" => set_length(&mut style.layout.inset.right, value, context),
        "bottom" => set_length(&mut style.layout.inset.bottom, value, context),
        "left" => set_length(&mut style.layout.inset.left, value, context),
        "align-items" => {
            if let Some(align) = parse_align(value) {
                style.layout.align_items = Some(align);
            }
        }
        "align-self" => {
            if let Some(align) = parse_align(value) {
                style.layout.align_self = Some(align);
            }
        }
        "align-content" => {
            if let Some(distribute) = parse_distribute(value) {
                style.layout.align_content = Some(distribute);
            }
        }
        "justify-content" => {
            if let Some(distribute) = parse_distribute(value) {
                style.layout.justify_content = Some(distribute);
            }
        }
        // `place-items` also carries `justify-items`, which has no counterpart
        // in SphereKit's layout style: every flex item is placed by its own
        // `align-self`, so a container-level inline alignment would have
        // nothing to act on. The block axis half is applied; the other half is
        // ignored rather than silently mapped onto `justify-content`, which
        // distributes free space and is a different thing entirely.
        "place-items" => {
            if let Some(align) = split_components(value).first().copied().and_then(parse_align) {
                style.layout.align_items = Some(align);
            }
        }
        "place-content" => {
            let parts = split_components(value);
            if let Some(align) = parts.first().copied().and_then(parse_distribute) {
                style.layout.align_content = Some(align);
                let justify = parts.get(1).copied().and_then(parse_distribute);
                style.layout.justify_content = Some(justify.unwrap_or(align));
            }
        }
        "overflow" => {
            let parts = split_components(value);
            let Some(horizontal) = parts.first().copied().and_then(parse_overflow) else {
                return;
            };
            let vertical = parts.get(1).copied().and_then(parse_overflow).unwrap_or(horizontal);
            style.layout.overflow_x = horizontal;
            style.layout.overflow_y = vertical;
            style.clip_content = horizontal != Overflow::Visible || vertical != Overflow::Visible;
        }
        "overflow-x" => {
            if let Some(overflow) = parse_overflow(value) {
                style.layout.overflow_x = overflow;
                style.clip_content |= overflow != Overflow::Visible;
            }
        }
        "overflow-y" => {
            if let Some(overflow) = parse_overflow(value) {
                style.layout.overflow_y = overflow;
                style.clip_content |= overflow != Overflow::Visible;
            }
        }
        "aspect-ratio" => {
            if let Some(ratio) = parse_aspect_ratio(value, context) {
                style.layout.aspect_ratio = (ratio > 0.0).then_some(ratio);
            }
        }
        "z-index" => {
            if let Some(number) = parse_number(value, context) {
                style.layout.z_index = number as i32;
            }
        }
        "opacity" => {
            if let Some(number) = parse_opacity(value, context) {
                style.layout.opacity = number;
                style.paint_opacity = Some(number);
            }
        }
        // SphereKit has no `visibility`: a node is laid out and painted, or it
        // is `Display::None` and neither. Zero opacity is the mapping that
        // keeps CSS's actual guarantee — the box still occupies its space.
        "visibility" => match value.trim().to_ascii_lowercase().as_str() {
            "hidden" | "collapse" => {
                style.layout.opacity = 0.0;
                style.paint_opacity = Some(0.0);
            }
            "visible" => {
                style.layout.opacity = 1.0;
                style.paint_opacity = Some(1.0);
            }
            _ => {}
        },
        "background" | "background-color" => {
            if let Some(color) = color::parse_color(value) {
                style.background = Some(color);
            }
        }
        "border" => apply_border_shorthand(style, value, context),
        "border-color" => {
            if let Some(color) = color::parse_color(value) {
                style.border_color = Some(color);
            }
        }
        "border-radius" => {
            if let Some(corners) = parse_corners(value, context) {
                set_corners(style, corners);
            }
        }
        "border-top-left-radius" => set_corner(style, value, context, Corner::TopLeft),
        "border-top-right-radius" => set_corner(style, value, context, Corner::TopRight),
        "border-bottom-right-radius" => set_corner(style, value, context, Corner::BottomRight),
        "border-bottom-left-radius" => set_corner(style, value, context, Corner::BottomLeft),
        "box-shadow" => {
            if let Some(shadows) = parse_box_shadow(value, context) {
                style.shadows = shadows;
            }
        }
        "cursor" => {
            if let Some(cursor) = parse_cursor(value) {
                style.cursor = Some(cursor);
            }
        }
        _ => {}
    }
}

fn set_length(slot: &mut Length, value: &str, context: &StyleContext) {
    if let Some(length) = parse_length(value, context) {
        *slot = length;
    }
}

fn set_corners(style: &mut ResolvedStyle, corners: Corners<Px>) {
    style.corner_radii = Some(corners);
    style.corner_radius = uniform_corner(corners);
    style.layout.corner_radius = corners;
}

/// Which corner a `border-*-radius` longhand addresses.
#[derive(Copy, Clone)]
enum Corner {
    TopLeft,
    TopRight,
    BottomRight,
    BottomLeft,
}

fn set_corner(style: &mut ResolvedStyle, value: &str, context: &StyleContext, corner: Corner) {
    let Some(radius) = parse_px(value, context) else { return };
    let mut corners = style.corner_radii.unwrap_or(style.layout.corner_radius);
    match corner {
        Corner::TopLeft => corners.top_left = radius,
        Corner::TopRight => corners.top_right = radius,
        Corner::BottomRight => corners.bottom_right = radius,
        Corner::BottomLeft => corners.bottom_left = radius,
    }
    set_corners(style, corners);
}

/// Parses the `flex` shorthand.
fn apply_flex_shorthand(style: &mut ResolvedStyle, value: &str, context: &StyleContext) {
    let parts = split_components(value);
    match parts.as_slice() {
        [single] if single.eq_ignore_ascii_case("none") => {
            style.layout.flex_grow = 0.0;
            style.layout.flex_shrink = 0.0;
            style.layout.flex_basis = Length::Auto;
        }
        [single] if single.eq_ignore_ascii_case("auto") => {
            style.layout.flex_grow = 1.0;
            style.layout.flex_shrink = 1.0;
            style.layout.flex_basis = Length::Auto;
        }
        [single] if single.eq_ignore_ascii_case("initial") => {
            style.layout.flex_grow = 0.0;
            style.layout.flex_shrink = 1.0;
            style.layout.flex_basis = Length::Auto;
        }
        // `flex: 1` is the one shorthand everybody writes, and its basis is
        // `0`, not `auto`: that is what makes two `flex: 1` siblings the same
        // width regardless of their content.
        [single] => {
            let Some(grow) = parse_number(single, context) else { return };
            style.layout.flex_grow = grow.max(0.0);
            style.layout.flex_shrink = 1.0;
            style.layout.flex_basis = Length::Px(Px::ZERO);
        }
        [first, second] => {
            let Some(grow) = parse_number(first, context) else { return };
            style.layout.flex_grow = grow.max(0.0);
            if let Some(shrink) = parse_number(second, context) {
                style.layout.flex_shrink = shrink.max(0.0);
                style.layout.flex_basis = Length::Px(Px::ZERO);
            } else if let Some(basis) = parse_length(second, context) {
                style.layout.flex_shrink = 1.0;
                style.layout.flex_basis = basis;
            }
        }
        [first, second, third] => {
            let Some(grow) = parse_number(first, context) else { return };
            let Some(shrink) = parse_number(second, context) else { return };
            let Some(basis) = parse_length(third, context) else { return };
            style.layout.flex_grow = grow.max(0.0);
            style.layout.flex_shrink = shrink.max(0.0);
            style.layout.flex_basis = basis;
        }
        _ => {}
    }
}

fn apply_border_shorthand(style: &mut ResolvedStyle, value: &str, context: &StyleContext) {
    let mut width = None;
    let mut color = None;
    for token in split_components(value) {
        if token.eq_ignore_ascii_case("none") || token.eq_ignore_ascii_case("hidden") {
            width = Some(Px::ZERO);
            continue;
        }
        if width.is_none()
            && let Some(Length::Px(parsed)) = parse_length(token, context)
        {
            width = Some(parsed);
            continue;
        }
        if color.is_none() {
            color = color::parse_color(token);
        }
    }
    if let Some(width) = width {
        style.layout.border = edges_all(Length::Px(width));
        style.border_width = Some(width);
    }
    if let Some(color) = color {
        style.border_color = Some(color);
    }
}

/// Parses `box-shadow`, which is a comma-separated list drawn back to front.
///
/// `inset` is recognised and then dropped. [`spherekit_ui::PaintStyle`] paints
/// every shadow behind the element's background, so an inset shadow would be
/// covered by the fill and produce nothing visible. Honouring the flag would
/// mean reordering the paint pass, which is a change in `spherekit-ui`, not
/// here; recognising it at least keeps `box-shadow: inset 0 1px 2px #000` from
/// being read as a four-length outer shadow.
fn parse_box_shadow(value: &str, context: &StyleContext) -> Option<Vec<Shadow>> {
    if value.trim().eq_ignore_ascii_case("none") {
        return Some(Vec::new());
    }
    let mut shadows = Vec::new();
    for entry in split_top_level(value, ',') {
        let mut lengths: Vec<Px> = Vec::new();
        let mut color = None;
        let mut valid = true;
        for token in split_components(entry) {
            if token.eq_ignore_ascii_case("inset") {
                continue;
            }
            if lengths.len() < 4
                && let Some(length) = parse_px(token, context)
            {
                lengths.push(length);
                continue;
            }
            if color.is_none()
                && let Some(parsed) = color::parse_color(token)
            {
                color = Some(parsed);
                continue;
            }
            valid = false;
            break;
        }
        if !valid || lengths.len() < 2 {
            return None;
        }
        shadows.push(Shadow {
            offset: Size::new(lengths[0], lengths[1]),
            blur_radius: lengths.get(2).copied().unwrap_or(Px::ZERO),
            spread: lengths.get(3).copied().unwrap_or(Px::ZERO),
            color: color.unwrap_or(Color::BLACK),
            inset: false,
        });
    }
    (!shadows.is_empty()).then_some(shadows)
}

fn parse_display(value: &str) -> Option<Display> {
    match value.trim().to_ascii_lowercase().as_str() {
        // `inline-flex` differs from `flex` only in how the box participates in
        // an inline formatting context, and SphereKit has no inline flow to
        // participate in.
        "flex" | "inline-flex" => Some(Display::Flex),
        "block" | "inline-block" => Some(Display::Block),
        "grid" | "inline-grid" => Some(Display::Grid),
        "none" => Some(Display::None),
        _ => None,
    }
}

fn parse_position(value: &str) -> Option<Position> {
    match value.trim().to_ascii_lowercase().as_str() {
        "relative" | "static" => Some(Position::Relative),
        "absolute" | "fixed" => Some(Position::Absolute),
        _ => None,
    }
}

fn parse_flex_direction(value: &str) -> Option<FlexDirection> {
    match value.trim().to_ascii_lowercase().as_str() {
        "row" => Some(FlexDirection::Row),
        "column" => Some(FlexDirection::Column),
        "row-reverse" => Some(FlexDirection::RowReverse),
        "column-reverse" => Some(FlexDirection::ColumnReverse),
        _ => None,
    }
}

fn parse_flex_wrap(value: &str) -> Option<FlexWrap> {
    match value.trim().to_ascii_lowercase().as_str() {
        "nowrap" => Some(FlexWrap::NoWrap),
        "wrap" => Some(FlexWrap::Wrap),
        "wrap-reverse" => Some(FlexWrap::WrapReverse),
        _ => None,
    }
}

fn parse_align(value: &str) -> Option<Align> {
    match value.trim().to_ascii_lowercase().as_str() {
        "start" | "flex-start" | "self-start" => Some(Align::Start),
        "end" | "flex-end" | "self-end" => Some(Align::End),
        "center" => Some(Align::Center),
        "stretch" | "normal" => Some(Align::Stretch),
        "baseline" => Some(Align::Baseline),
        _ => None,
    }
}

fn parse_distribute(value: &str) -> Option<Distribute> {
    match value.trim().to_ascii_lowercase().as_str() {
        "start" | "flex-start" => Some(Distribute::Start),
        "end" | "flex-end" => Some(Distribute::End),
        "center" => Some(Distribute::Center),
        "stretch" => Some(Distribute::Stretch),
        "space-between" => Some(Distribute::SpaceBetween),
        "space-evenly" => Some(Distribute::SpaceEvenly),
        "space-around" => Some(Distribute::SpaceAround),
        _ => None,
    }
}

fn parse_overflow(value: &str) -> Option<Overflow> {
    match value.trim().to_ascii_lowercase().as_str() {
        "visible" => Some(Overflow::Visible),
        "hidden" | "clip" => Some(Overflow::Hidden),
        "scroll" | "auto" => Some(Overflow::Scroll),
        _ => None,
    }
}

/// Maps the CSS cursor keywords onto the pointer shapes SphereKit can show.
fn parse_cursor(value: &str) -> Option<Cursor> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" | "default" => Some(Cursor::Default),
        "pointer" => Some(Cursor::Pointer),
        "text" | "vertical-text" => Some(Cursor::Text),
        "crosshair" | "cell" => Some(Cursor::Crosshair),
        "move" | "all-scroll" => Some(Cursor::Move),
        "grab" => Some(Cursor::Grab),
        "grabbing" => Some(Cursor::Grabbing),
        "ew-resize" | "e-resize" | "w-resize" => Some(Cursor::ResizeEw),
        "ns-resize" | "n-resize" | "s-resize" => Some(Cursor::ResizeNs),
        "nesw-resize" | "ne-resize" | "sw-resize" => Some(Cursor::ResizeNesw),
        "nwse-resize" | "nw-resize" | "se-resize" => Some(Cursor::ResizeNwse),
        "col-resize" => Some(Cursor::ColResize),
        "row-resize" => Some(Cursor::RowResize),
        "not-allowed" | "no-drop" => Some(Cursor::NotAllowed),
        "wait" | "progress" => Some(Cursor::Wait),
        "none" => Some(Cursor::None),
        _ => None,
    }
}

/// Parses `aspect-ratio`, which is a number or a `width / height` pair.
fn parse_aspect_ratio(value: &str, context: &StyleContext) -> Option<f32> {
    if let Some((width, height)) = value.split_once('/') {
        let width = parse_number(width, context)?;
        let height = parse_number(height, context)?;
        return (height != 0.0).then_some(width / height);
    }
    parse_number(value, context)
}

/// Parses `opacity`, which accepts a number or a percentage.
fn parse_opacity(value: &str, context: &StyleContext) -> Option<f32> {
    if let Some(percent) = value.trim().strip_suffix('%') {
        return percent.trim().parse::<f32>().ok().map(|value| (value / 100.0).clamp(0.0, 1.0));
    }
    parse_number(value, context).map(|number| number.clamp(0.0, 1.0))
}

fn parse_edges(value: &str, context: &StyleContext) -> Option<Edges<Length>> {
    let lengths = split_components(value)
        .into_iter()
        .map(|item| parse_length(item, context))
        .collect::<Option<Vec<_>>>()?;
    match lengths.as_slice() {
        [one] => Some(edges_all(*one)),
        [vertical, horizontal] => {
            Some(Edges { top: *vertical, right: *horizontal, bottom: *vertical, left: *horizontal })
        }
        [top, horizontal, bottom] => {
            Some(Edges { top: *top, right: *horizontal, bottom: *bottom, left: *horizontal })
        }
        [top, right, bottom, left] => {
            Some(Edges { top: *top, right: *right, bottom: *bottom, left: *left })
        }
        _ => None,
    }
}

/// Parses the one-to-four-value `border-radius` shorthand.
///
/// The elliptical form, `8px / 4px`, is rejected: [`Corners`] carries one
/// radius per corner, so the second set could only be discarded.
fn parse_corners(value: &str, context: &StyleContext) -> Option<Corners<Px>> {
    if value.contains('/') {
        return None;
    }
    let radii = split_components(value)
        .into_iter()
        .map(|item| parse_px(item, context))
        .collect::<Option<Vec<_>>>()?;
    match radii.as_slice() {
        [all] => Some(Corners::all(*all)),
        [main, cross] => Some(Corners {
            top_left: *main,
            top_right: *cross,
            bottom_right: *main,
            bottom_left: *cross,
        }),
        [top_left, cross, bottom_right] => Some(Corners {
            top_left: *top_left,
            top_right: *cross,
            bottom_right: *bottom_right,
            bottom_left: *cross,
        }),
        [top_left, top_right, bottom_right, bottom_left] => Some(Corners {
            top_left: *top_left,
            top_right: *top_right,
            bottom_right: *bottom_right,
            bottom_left: *bottom_left,
        }),
        _ => None,
    }
}

fn uniform_edge(edges: Edges<Length>) -> Option<Px> {
    match (edges.top, edges.right, edges.bottom, edges.left) {
        (Length::Px(top), Length::Px(right), Length::Px(bottom), Length::Px(left))
            if top == right && right == bottom && bottom == left =>
        {
            Some(top)
        }
        _ => None,
    }
}

fn uniform_corner(corners: Corners<Px>) -> Option<Px> {
    (corners.top_left == corners.top_right
        && corners.top_right == corners.bottom_right
        && corners.bottom_right == corners.bottom_left)
        .then_some(corners.top_left)
}

fn parse_gap(value: &str, context: &StyleContext) -> Option<(Px, Px)> {
    let parts = split_components(value);
    match parts.as_slice() {
        [one] => {
            let value = parse_px(one, context)?;
            Some((value, value))
        }
        [row, column] => Some((parse_px(column, context)?, parse_px(row, context)?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::px;

    fn resolve(css: &str) -> ResolvedStyle {
        let mut style = ResolvedStyle::default();
        let context = StyleContext::default();
        let mut declarations: Vec<(String, String)> = css
            .split(';')
            .filter_map(|item| item.split_once(':'))
            .map(|(property, value)| (property.trim().to_string(), value.trim().to_string()))
            .collect();
        declarations.sort_by_key(|(property, _)| (apply_tier(property), property.clone()));
        for (property, value) in declarations {
            apply_declaration(&mut style, &property, &value, &context);
        }
        style
    }

    #[test]
    fn inline_flex_lays_out_as_flex() {
        assert_eq!(resolve("display: inline-flex").layout.display, Display::Flex);
    }

    #[test]
    fn an_unknown_display_leaves_the_default_alone() {
        assert_eq!(resolve("display: ruby").layout.display, Display::Flex);
        assert_eq!(resolve("display: none").layout.display, Display::None);
    }

    #[test]
    fn the_flex_shorthand_with_one_number_zeroes_the_basis() {
        let style = resolve("flex: 1");
        assert_eq!(style.layout.flex_grow, 1.0);
        assert_eq!(style.layout.flex_shrink, 1.0);
        assert_eq!(style.layout.flex_basis, Length::Px(Px::ZERO));
    }

    #[test]
    fn the_three_value_flex_shorthand_sets_all_three() {
        let style = resolve("flex: 2 3 40px");
        assert_eq!(style.layout.flex_grow, 2.0);
        assert_eq!(style.layout.flex_shrink, 3.0);
        assert_eq!(style.layout.flex_basis, Length::Px(px(40.0)));
    }

    #[test]
    fn flex_none_and_auto_are_the_keyword_forms() {
        let none = resolve("flex: none");
        assert_eq!((none.layout.flex_grow, none.layout.flex_shrink), (0.0, 0.0));
        let auto = resolve("flex: auto");
        assert_eq!((auto.layout.flex_grow, auto.layout.flex_shrink), (1.0, 1.0));
        assert_eq!(auto.layout.flex_basis, Length::Auto);
    }

    #[test]
    fn a_flex_longhand_overrides_the_shorthand_whatever_the_source_order() {
        assert_eq!(resolve("flex-grow: 5; flex: 1").layout.flex_grow, 5.0);
        assert_eq!(resolve("flex: 1; flex-grow: 5").layout.flex_grow, 5.0);
    }

    #[test]
    fn edge_shorthands_follow_the_one_to_four_value_rule() {
        assert_eq!(resolve("padding: 4px").layout.padding.left, Length::Px(px(4.0)));
        let two = resolve("padding: 4px 8px").layout.padding;
        assert_eq!((two.top, two.right), (Length::Px(px(4.0)), Length::Px(px(8.0))));
        let three = resolve("padding: 1px 2px 3px").layout.padding;
        assert_eq!((three.top, three.bottom), (Length::Px(px(1.0)), Length::Px(px(3.0))));
        let four = resolve("margin: 1px 2px 3px 4px").layout.margin;
        assert_eq!(four.left, Length::Px(px(4.0)));
    }

    #[test]
    fn a_padding_longhand_wins_over_the_shorthand() {
        let style = resolve("padding-left: 12px; padding: 4px");
        assert_eq!(style.layout.padding.left, Length::Px(px(12.0)));
        assert_eq!(style.layout.padding.top, Length::Px(px(4.0)));
    }

    #[test]
    fn gap_takes_row_then_column() {
        let style = resolve("gap: 2px 6px");
        assert_eq!(style.layout.gap.height, px(2.0));
        assert_eq!(style.layout.gap.width, px(6.0));
    }

    #[test]
    fn overflow_accepts_one_or_two_values_and_drives_clipping() {
        let one = resolve("overflow: hidden");
        assert_eq!(one.layout.overflow_y, Overflow::Hidden);
        assert!(one.clip_content);
        let two = resolve("overflow: hidden auto");
        assert_eq!(two.layout.overflow_x, Overflow::Hidden);
        assert_eq!(two.layout.overflow_y, Overflow::Scroll);
        assert!(!resolve("overflow: visible").clip_content);
    }

    #[test]
    fn place_content_fills_both_axes_from_one_value() {
        let style = resolve("place-content: center");
        assert_eq!(style.layout.align_content, Some(Distribute::Center));
        assert_eq!(style.layout.justify_content, Some(Distribute::Center));
    }

    #[test]
    fn place_items_sets_the_cross_axis_alignment() {
        assert_eq!(resolve("place-items: center").layout.align_items, Some(Align::Center));
    }

    #[test]
    fn a_longhand_alignment_beats_the_place_shorthand() {
        let style = resolve("justify-content: flex-end; place-content: center");
        assert_eq!(style.layout.justify_content, Some(Distribute::End));
        assert_eq!(style.layout.align_content, Some(Distribute::Center));
    }

    #[test]
    fn border_radius_expands_one_to_four_values() {
        assert_eq!(resolve("border-radius: 8px").corner_radius, Some(px(8.0)));
        let two = resolve("border-radius: 8px 2px").corner_radii.unwrap();
        assert_eq!((two.top_left, two.top_right), (px(8.0), px(2.0)));
        assert_eq!((two.bottom_right, two.bottom_left), (px(8.0), px(2.0)));
        let four = resolve("border-radius: 1px 2px 3px 4px").corner_radii.unwrap();
        assert_eq!(four.bottom_left, px(4.0));
    }

    #[test]
    fn a_non_uniform_radius_leaves_the_uniform_field_unset() {
        assert_eq!(resolve("border-radius: 8px 2px").corner_radius, None);
    }

    #[test]
    fn per_corner_longhands_layer_onto_the_shorthand() {
        let style = resolve("border-radius: 4px; border-top-left-radius: 12px");
        let corners = style.corner_radii.unwrap();
        assert_eq!(corners.top_left, px(12.0));
        assert_eq!(corners.bottom_right, px(4.0));
    }

    #[test]
    fn an_elliptical_radius_is_ignored_rather_than_halved() {
        assert_eq!(resolve("border-radius: 8px / 4px").corner_radii, None);
    }

    #[test]
    fn the_border_shorthand_takes_a_width_and_a_colour_in_any_order() {
        let style = resolve("border: 2px solid #ff0000");
        assert_eq!(style.border_width, Some(px(2.0)));
        assert_eq!(style.border_color, Some(Color::RED));
        assert_eq!(resolve("border: none").border_width, Some(Px::ZERO));
    }

    #[test]
    fn box_shadow_reads_offset_blur_spread_and_colour() {
        let shadows = resolve("box-shadow: 1px 2px 3px 4px #000000").shadows;
        assert_eq!(shadows.len(), 1);
        assert_eq!(shadows[0].offset, Size::new(px(1.0), px(2.0)));
        assert_eq!(shadows[0].blur_radius, px(3.0));
        assert_eq!(shadows[0].spread, px(4.0));
        assert_eq!(shadows[0].color, Color::BLACK);
    }

    #[test]
    fn box_shadow_defaults_the_optional_parts() {
        let shadows = resolve("box-shadow: 0 2px rgba(0, 0, 0, 0.5)").shadows;
        assert_eq!(shadows[0].blur_radius, Px::ZERO);
        assert_eq!(shadows[0].spread, Px::ZERO);
        assert!((shadows[0].color.a - 0.5).abs() < 0.01);
    }

    #[test]
    fn box_shadow_parses_a_comma_separated_list() {
        assert_eq!(resolve("box-shadow: 0 1px 2px #000, 0 4px 8px #111").shadows.len(), 2);
    }

    #[test]
    fn inset_is_recognised_but_never_reaches_the_painter() {
        let shadows = resolve("box-shadow: inset 0 1px 2px #000").shadows;
        assert_eq!(shadows.len(), 1);
        assert!(!shadows[0].inset);
        assert_eq!(shadows[0].blur_radius, px(2.0));
    }

    #[test]
    fn box_shadow_none_clears_the_list() {
        assert!(resolve("box-shadow: 0 1px 2px #000; box-shadow: none").shadows.is_empty());
    }

    #[test]
    fn a_malformed_box_shadow_leaves_the_list_empty() {
        assert!(resolve("box-shadow: 1px").shadows.is_empty());
        assert!(resolve("box-shadow: wobble 1px 2px").shadows.is_empty());
    }

    #[test]
    fn cursor_keywords_map_onto_the_native_pointer_shapes() {
        assert_eq!(resolve("cursor: pointer").cursor, Some(Cursor::Pointer));
        assert_eq!(resolve("cursor: ew-resize").cursor, Some(Cursor::ResizeEw));
        assert_eq!(resolve("cursor: not-allowed").cursor, Some(Cursor::NotAllowed));
        assert_eq!(resolve("cursor: zoom-in").cursor, None);
    }

    #[test]
    fn visibility_hidden_keeps_the_box_but_paints_nothing() {
        let style = resolve("visibility: hidden");
        assert_eq!(style.paint_opacity, Some(0.0));
        assert_eq!(style.layout.display, Display::Flex);
    }

    #[test]
    fn opacity_accepts_a_number_or_a_percentage() {
        assert_eq!(resolve("opacity: 0.4").paint_opacity, Some(0.4));
        assert_eq!(resolve("opacity: 40%").paint_opacity, Some(0.4));
        assert_eq!(resolve("opacity: 4").paint_opacity, Some(1.0));
    }

    #[test]
    fn aspect_ratio_accepts_a_ratio_pair() {
        assert_eq!(resolve("aspect-ratio: 16 / 9").layout.aspect_ratio, Some(16.0 / 9.0));
        assert_eq!(resolve("aspect-ratio: 2").layout.aspect_ratio, Some(2.0));
    }

    #[test]
    fn absolute_and_fixed_both_leave_the_flow() {
        assert_eq!(resolve("position: absolute").layout.position, Position::Absolute);
        assert_eq!(resolve("position: fixed").layout.position, Position::Absolute);
    }

    #[test]
    fn viewport_and_font_units_reach_layout_properties() {
        let mut style = ResolvedStyle::default();
        let context = StyleContext::default().with_root_font_size(px(10.0));
        apply_declaration(&mut style, "width", "3rem", &context);
        assert_eq!(style.layout.size.width, Length::Px(px(30.0)));
    }
}
