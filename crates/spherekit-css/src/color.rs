//! Colour values.
//!
//! Every CSS colour syntax that names a single sRGB colour is supported here:
//! the full named-colour table, all four hex forms, and `rgb()`/`rgba()`/
//! `hsl()`/`hsla()` in both the legacy comma syntax and the modern
//! space-with-slash syntax. They all land on [`Color`], which is what the
//! painter speaks.
//!
//! Two things are deliberately absent. `currentcolor` needs the element's own
//! resolved `color`, which is not known while a single declaration is being
//! interpreted, and faking it with black would be worse than ignoring it.
//! `linear-gradient()` and friends parse into [`spherekit_core::Gradient`]
//! shapes whose stops sit at *absolute* points in the shape's local space, so a
//! gradient cannot be computed without the element's final box — and a computed
//! style in this crate is deliberately size-independent, resolved once per node
//! and reused. Supporting gradients means re-resolving paint after layout,
//! which is a change to the whole pipeline rather than to this file.
//!
//! Note that `green` is the CSS `#008000`, not [`Color::GREEN`] (`#00ff00`),
//! which CSS calls `lime`. The earlier six-colour table conflated the two; the
//! real table is the one authors expect.

use crate::length::{Component, component, strip_function};
use spherekit_core::{Color, hsla};

/// Parses any supported CSS colour value.
pub(crate) fn parse_color(text: &str) -> Option<Color> {
    let text = text.trim();
    if let Some(hex) = text.strip_prefix('#') {
        return parse_hex(hex);
    }
    if text.contains('(') {
        return parse_function(text);
    }
    named_color(text)
}

/// Looks up a CSS named colour, plus `transparent`.
pub(crate) fn named_color(name: &str) -> Option<Color> {
    let lower = name.trim().to_ascii_lowercase();
    if lower == "transparent" {
        return Some(Color::TRANSPARENT);
    }
    NAMED
        .binary_search_by(|(candidate, _)| (*candidate).cmp(lower.as_str()))
        .ok()
        .map(|index| Color::hex(NAMED[index].1))
}

fn parse_hex(hex: &str) -> Option<Color> {
    if !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        3 | 4 => {
            let mut expanded = String::with_capacity(8);
            for character in hex.chars() {
                expanded.push(character);
                expanded.push(character);
            }
            let value = u32::from_str_radix(&expanded, 16).ok()?;
            Some(if hex.len() == 3 { Color::hex(value) } else { Color::hex_rgba(value) })
        }
        6 => u32::from_str_radix(hex, 16).ok().map(Color::hex),
        8 => u32::from_str_radix(hex, 16).ok().map(Color::hex_rgba),
        _ => None,
    }
}

fn parse_function(text: &str) -> Option<Color> {
    let lower = text.to_ascii_lowercase();
    let name = lower.split('(').next()?.trim().to_string();
    let body = strip_function(text, &name)?;
    let (channels, alpha) = split_arguments(body)?;
    match name.as_str() {
        "rgb" | "rgba" => parse_rgb(&channels, alpha.as_deref()),
        "hsl" | "hsla" => parse_hsl(&channels, alpha.as_deref()),
        _ => None,
    }
}

/// Splits a colour function body into channels and an optional alpha.
///
/// Handles both `rgb(1, 2, 3, 0.5)` and `rgb(1 2 3 / 50%)` without needing to
/// know which function is being parsed, because the two syntaxes differ only in
/// where the alpha lives.
fn split_arguments(body: &str) -> Option<(Vec<String>, Option<String>)> {
    let (main, slash_alpha) = match body.split_once('/') {
        Some((main, alpha)) => (main, Some(alpha.trim().to_string())),
        None => (body, None),
    };
    let mut parts: Vec<String> = if main.contains(',') {
        main.split(',').map(|part| part.trim().to_string()).collect()
    } else {
        main.split_whitespace().map(ToOwned::to_owned).collect()
    };
    if parts.iter().any(String::is_empty) {
        return None;
    }
    let alpha = match slash_alpha {
        Some(alpha) => {
            if parts.len() != 3 || alpha.is_empty() {
                return None;
            }
            Some(alpha)
        }
        None if parts.len() == 4 => Some(parts.pop()?),
        None if parts.len() == 3 => None,
        None => return None,
    };
    Some((parts, alpha))
}

fn parse_rgb(channels: &[String], alpha: Option<&str>) -> Option<Color> {
    let mut values = [0.0f32; 3];
    for (slot, text) in values.iter_mut().zip(channels) {
        *slot = match component(text)? {
            Component::Number(value) => value / 255.0,
            Component::Percentage(value) => value,
            _ => return None,
        }
        .clamp(0.0, 1.0);
    }
    Some(Color::rgba(values[0], values[1], values[2], parse_alpha(alpha)?))
}

fn parse_hsl(channels: &[String], alpha: Option<&str>) -> Option<Color> {
    let hue = parse_hue(&channels[0])?;
    let saturation = parse_ratio(&channels[1])?;
    let lightness = parse_ratio(&channels[2])?;
    Some(hsla(hue, saturation, lightness, parse_alpha(alpha)?).to_color())
}

/// Converts a CSS hue into turns, which is what [`hsla`] takes.
fn parse_hue(text: &str) -> Option<f32> {
    let degrees = match component(text)? {
        Component::Number(value) => value,
        Component::Dimension(value, unit) => match unit.as_str() {
            "deg" => value,
            "grad" => value * 0.9,
            "rad" => value.to_degrees(),
            "turn" => value * 360.0,
            _ => return None,
        },
        _ => return None,
    };
    Some(degrees / 360.0)
}

fn parse_ratio(text: &str) -> Option<f32> {
    match component(text)? {
        Component::Percentage(value) => Some(value.clamp(0.0, 1.0)),
        Component::Number(value) => Some(value.clamp(0.0, 1.0)),
        _ => None,
    }
}

fn parse_alpha(text: Option<&str>) -> Option<f32> {
    match text {
        None => Some(1.0),
        Some(text) => parse_ratio(text),
    }
}

/// The CSS named colours, sorted so the lookup can binary-search.
///
/// The table is spelled out rather than generated because it is the one place
/// where being wrong is invisible: a mistyped hex digit produces a colour that
/// still renders, just not the one the author asked for.
static NAMED: &[(&str, u32)] = &[
    ("aliceblue", 0xF0F8FF),
    ("antiquewhite", 0xFAEBD7),
    ("aqua", 0x00FFFF),
    ("aquamarine", 0x7FFFD4),
    ("azure", 0xF0FFFF),
    ("beige", 0xF5F5DC),
    ("bisque", 0xFFE4C4),
    ("black", 0x000000),
    ("blanchedalmond", 0xFFEBCD),
    ("blue", 0x0000FF),
    ("blueviolet", 0x8A2BE2),
    ("brown", 0xA52A2A),
    ("burlywood", 0xDEB887),
    ("cadetblue", 0x5F9EA0),
    ("chartreuse", 0x7FFF00),
    ("chocolate", 0xD2691E),
    ("coral", 0xFF7F50),
    ("cornflowerblue", 0x6495ED),
    ("cornsilk", 0xFFF8DC),
    ("crimson", 0xDC143C),
    ("cyan", 0x00FFFF),
    ("darkblue", 0x00008B),
    ("darkcyan", 0x008B8B),
    ("darkgoldenrod", 0xB8860B),
    ("darkgray", 0xA9A9A9),
    ("darkgreen", 0x006400),
    ("darkgrey", 0xA9A9A9),
    ("darkkhaki", 0xBDB76B),
    ("darkmagenta", 0x8B008B),
    ("darkolivegreen", 0x556B2F),
    ("darkorange", 0xFF8C00),
    ("darkorchid", 0x9932CC),
    ("darkred", 0x8B0000),
    ("darksalmon", 0xE9967A),
    ("darkseagreen", 0x8FBC8F),
    ("darkslateblue", 0x483D8B),
    ("darkslategray", 0x2F4F4F),
    ("darkslategrey", 0x2F4F4F),
    ("darkturquoise", 0x00CED1),
    ("darkviolet", 0x9400D3),
    ("deeppink", 0xFF1493),
    ("deepskyblue", 0x00BFFF),
    ("dimgray", 0x696969),
    ("dimgrey", 0x696969),
    ("dodgerblue", 0x1E90FF),
    ("firebrick", 0xB22222),
    ("floralwhite", 0xFFFAF0),
    ("forestgreen", 0x228B22),
    ("fuchsia", 0xFF00FF),
    ("gainsboro", 0xDCDCDC),
    ("ghostwhite", 0xF8F8FF),
    ("gold", 0xFFD700),
    ("goldenrod", 0xDAA520),
    ("gray", 0x808080),
    ("green", 0x008000),
    ("greenyellow", 0xADFF2F),
    ("grey", 0x808080),
    ("honeydew", 0xF0FFF0),
    ("hotpink", 0xFF69B4),
    ("indianred", 0xCD5C5C),
    ("indigo", 0x4B0082),
    ("ivory", 0xFFFFF0),
    ("khaki", 0xF0E68C),
    ("lavender", 0xE6E6FA),
    ("lavenderblush", 0xFFF0F5),
    ("lawngreen", 0x7CFC00),
    ("lemonchiffon", 0xFFFACD),
    ("lightblue", 0xADD8E6),
    ("lightcoral", 0xF08080),
    ("lightcyan", 0xE0FFFF),
    ("lightgoldenrodyellow", 0xFAFAD2),
    ("lightgray", 0xD3D3D3),
    ("lightgreen", 0x90EE90),
    ("lightgrey", 0xD3D3D3),
    ("lightpink", 0xFFB6C1),
    ("lightsalmon", 0xFFA07A),
    ("lightseagreen", 0x20B2AA),
    ("lightskyblue", 0x87CEFA),
    ("lightslategray", 0x778899),
    ("lightslategrey", 0x778899),
    ("lightsteelblue", 0xB0C4DE),
    ("lightyellow", 0xFFFFE0),
    ("lime", 0x00FF00),
    ("limegreen", 0x32CD32),
    ("linen", 0xFAF0E6),
    ("magenta", 0xFF00FF),
    ("maroon", 0x800000),
    ("mediumaquamarine", 0x66CDAA),
    ("mediumblue", 0x0000CD),
    ("mediumorchid", 0xBA55D3),
    ("mediumpurple", 0x9370DB),
    ("mediumseagreen", 0x3CB371),
    ("mediumslateblue", 0x7B68EE),
    ("mediumspringgreen", 0x00FA9A),
    ("mediumturquoise", 0x48D1CC),
    ("mediumvioletred", 0xC71585),
    ("midnightblue", 0x191970),
    ("mintcream", 0xF5FFFA),
    ("mistyrose", 0xFFE4E1),
    ("moccasin", 0xFFE4B5),
    ("navajowhite", 0xFFDEAD),
    ("navy", 0x000080),
    ("oldlace", 0xFDF5E6),
    ("olive", 0x808000),
    ("olivedrab", 0x6B8E23),
    ("orange", 0xFFA500),
    ("orangered", 0xFF4500),
    ("orchid", 0xDA70D6),
    ("palegoldenrod", 0xEEE8AA),
    ("palegreen", 0x98FB98),
    ("paleturquoise", 0xAFEEEE),
    ("palevioletred", 0xDB7093),
    ("papayawhip", 0xFFEFD5),
    ("peachpuff", 0xFFDAB9),
    ("peru", 0xCD853F),
    ("pink", 0xFFC0CB),
    ("plum", 0xDDA0DD),
    ("powderblue", 0xB0E0E6),
    ("purple", 0x800080),
    ("rebeccapurple", 0x663399),
    ("red", 0xFF0000),
    ("rosybrown", 0xBC8F8F),
    ("royalblue", 0x4169E1),
    ("saddlebrown", 0x8B4513),
    ("salmon", 0xFA8072),
    ("sandybrown", 0xF4A460),
    ("seagreen", 0x2E8B57),
    ("seashell", 0xFFF5EE),
    ("sienna", 0xA0522D),
    ("silver", 0xC0C0C0),
    ("skyblue", 0x87CEEB),
    ("slateblue", 0x6A5ACD),
    ("slategray", 0x708090),
    ("slategrey", 0x708090),
    ("snow", 0xFFFAFA),
    ("springgreen", 0x00FF7F),
    ("steelblue", 0x4682B4),
    ("tan", 0xD2B48C),
    ("teal", 0x008080),
    ("thistle", 0xD8BFD8),
    ("tomato", 0xFF6347),
    ("turquoise", 0x40E0D0),
    ("violet", 0xEE82EE),
    ("wheat", 0xF5DEB3),
    ("white", 0xFFFFFF),
    ("whitesmoke", 0xF5F5F5),
    ("yellow", 0xFFFF00),
    ("yellowgreen", 0x9ACD32),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(color: Color, expected: [f32; 4]) -> bool {
        let actual = [color.r, color.g, color.b, color.a];
        actual.iter().zip(expected).all(|(a, b)| (a - b).abs() < 0.01)
    }

    #[test]
    fn the_named_table_is_sorted_so_the_lookup_is_correct() {
        assert!(NAMED.windows(2).all(|pair| pair[0].0 < pair[1].0));
    }

    #[test]
    fn named_colours_cover_more_than_the_six_the_old_table_had() {
        assert_eq!(named_color("rebeccapurple"), Some(Color::hex(0x663399)));
        assert_eq!(named_color("MidnightBlue"), Some(Color::hex(0x191970)));
        assert_eq!(named_color("transparent"), Some(Color::TRANSPARENT));
    }

    #[test]
    fn css_green_is_dark_and_lime_is_the_bright_one() {
        assert_eq!(named_color("green"), Some(Color::hex(0x008000)));
        assert_eq!(named_color("lime"), Some(Color::GREEN));
    }

    #[test]
    fn an_unknown_name_is_rejected_rather_than_defaulted() {
        assert_eq!(named_color("burntsienna"), None);
        assert_eq!(parse_color("currentcolor"), None);
    }

    #[test]
    fn three_and_six_digit_hex_agree() {
        assert_eq!(parse_color("#f00"), Some(Color::RED));
        assert_eq!(parse_color("#FF0000"), Some(Color::RED));
    }

    #[test]
    fn four_and_eight_digit_hex_carry_alpha() {
        assert!(approx(parse_color("#ff000080").unwrap(), [1.0, 0.0, 0.0, 0.502]));
        assert!(approx(parse_color("#f008").unwrap(), [1.0, 0.0, 0.0, 0.533]));
    }

    #[test]
    fn malformed_hex_is_rejected() {
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#gg0000"), None);
    }

    #[test]
    fn legacy_rgb_and_rgba_parse() {
        assert!(approx(parse_color("rgb(255, 0, 0)").unwrap(), [1.0, 0.0, 0.0, 1.0]));
        assert!(approx(parse_color("rgba(0, 128, 255, 0.5)").unwrap(), [0.0, 0.502, 1.0, 0.5]));
    }

    #[test]
    fn modern_rgb_uses_spaces_and_a_slash_for_alpha() {
        assert!(approx(parse_color("rgb(255 0 0 / 50%)").unwrap(), [1.0, 0.0, 0.0, 0.5]));
        assert!(approx(parse_color("rgb(100% 0% 0%)").unwrap(), [1.0, 0.0, 0.0, 1.0]));
    }

    #[test]
    fn hsl_matches_the_equivalent_rgb() {
        assert!(approx(parse_color("hsl(0, 100%, 50%)").unwrap(), [1.0, 0.0, 0.0, 1.0]));
        assert!(approx(parse_color("hsl(120 100% 50%)").unwrap(), [0.0, 1.0, 0.0, 1.0]));
    }

    #[test]
    fn hsla_accepts_alpha_in_both_syntaxes() {
        assert!(approx(parse_color("hsla(240, 100%, 50%, 0.25)").unwrap(), [0.0, 0.0, 1.0, 0.25]));
        assert!(approx(parse_color("hsl(240 100% 50% / 25%)").unwrap(), [0.0, 0.0, 1.0, 0.25]));
    }

    #[test]
    fn hue_units_all_reach_the_same_colour() {
        let expected = [0.0, 1.0, 0.0, 1.0];
        assert!(approx(parse_color("hsl(120deg 100% 50%)").unwrap(), expected));
        assert!(approx(parse_color("hsl(0.333333turn 100% 50%)").unwrap(), expected));
        assert!(approx(parse_color("hsl(2.0944rad 100% 50%)").unwrap(), expected));
        assert!(approx(parse_color("hsl(133.33grad 100% 50%)").unwrap(), expected));
    }

    #[test]
    fn colour_functions_with_the_wrong_arity_are_rejected() {
        assert_eq!(parse_color("rgb(1, 2)"), None);
        assert_eq!(parse_color("rgb(1 2 3 4 5)"), None);
        assert_eq!(parse_color("hsl(1 2 3 / )"), None);
    }

    #[test]
    fn gradients_are_ignored_rather_than_flattened_to_a_stop() {
        assert_eq!(parse_color("linear-gradient(#000, #fff)"), None);
    }
}
