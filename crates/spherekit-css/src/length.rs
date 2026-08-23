//! Length values, and how they collapse into SphereKit's [`Length`].
//!
//! SphereKit's [`Length`] has exactly three shapes: `Auto`, an absolute pixel
//! count, and a fraction of the parent. CSS has many more units, but all of the
//! ones worth supporting here are *absolute once a context is known*: `rem` and
//! `em` need a font size, `vw`/`vh`/`vmin`/`vmax` need a viewport, `pt` needs
//! nothing at all. So the parser reduces every value to a [`LengthSum`] — an
//! absolute part plus a parent-relative part — and then asks whether that sum
//! is expressible.
//!
//! It usually is. `calc(100% - 12px)` is the case that is not, because the
//! layout model has no "parent minus a constant" length. Rather than rounding
//! it to whichever half looks bigger, the value is rejected and the declaration
//! is ignored, which is the same contract the rest of the crate keeps for
//! syntax it does not model.

use crate::StyleContext;
use cssparser::{Parser, ParserInput, Token};
use spherekit_core::{Length, Px, px};

/// One tokenised CSS value component.
///
/// Tokenisation goes through `cssparser` rather than hand-rolled scanning so
/// that numeric edge cases — `+.5`, `1e3`, escaped identifiers — behave the way
/// a browser would, and so the crate keeps exactly one definition of what a CSS
/// token is.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Component {
    /// A bare number, such as the `1.5` in `line-height: 1.5`.
    Number(f32),
    /// A percentage, already divided by 100.
    Percentage(f32),
    /// A number with a unit, such as `12px` or `90deg`.
    Dimension(f32, String),
    /// A bare keyword.
    Ident(String),
}

/// Tokenises a value that must consist of exactly one component.
///
/// Anything with trailing tokens is rejected, so `12px 4px` never sneaks
/// through a single-value property as `12px`.
pub(crate) fn component(text: &str) -> Option<Component> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut input = ParserInput::new(text);
    let mut parser = Parser::new(&mut input);
    let parsed = match parser.next().ok()? {
        Token::Number { value, .. } => Component::Number(*value),
        Token::Percentage { unit_value, .. } => Component::Percentage(*unit_value),
        Token::Dimension { value, unit, .. } => {
            Component::Dimension(*value, unit.as_ref().to_ascii_lowercase())
        }
        Token::Ident(name) => Component::Ident(name.as_ref().to_ascii_lowercase()),
        _ => return None,
    };
    if parser.next().is_ok() {
        return None;
    }
    Some(parsed)
}

/// A length reduced to an absolute part and a parent-relative part.
///
/// Keeping the two separate is what lets `calc()` add and scale mixed units and
/// still report honestly at the end whether the result is representable.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct LengthSum {
    /// Absolute logical pixels.
    pub pixels: f32,
    /// Fraction of the parent extent, where `1.0` is 100 %.
    pub fraction: f32,
    /// Number of bare numbers folded in, used to reject `10px * 2px`.
    pub numbers: f32,
    /// True when the sum contains no unit at all, only bare numbers.
    pub unitless: bool,
}

impl LengthSum {
    fn absolute(pixels: f32) -> Self {
        Self { pixels, fraction: 0.0, numbers: 0.0, unitless: false }
    }

    fn number(value: f32) -> Self {
        Self { pixels: 0.0, fraction: 0.0, numbers: value, unitless: true }
    }

    fn add(self, other: Self, sign: f32) -> Option<Self> {
        if self.unitless != other.unitless {
            return None;
        }
        Some(Self {
            pixels: self.pixels + other.pixels * sign,
            fraction: self.fraction + other.fraction * sign,
            numbers: self.numbers + other.numbers * sign,
            unitless: self.unitless,
        })
    }

    fn scale(self, factor: f32) -> Self {
        Self {
            pixels: self.pixels * factor,
            fraction: self.fraction * factor,
            numbers: self.numbers * factor,
            unitless: self.unitless,
        }
    }
}

/// Converts a unit-carrying number into a [`LengthSum`].
///
/// `em` resolves against [`StyleContext::font_size`] rather than against a
/// `font-size` declared in the same rule; see [`crate::Stylesheet::resolve_in`]
/// for why that distinction is handled one level up.
fn resolve_unit(value: f32, unit: &str, context: &StyleContext) -> Option<LengthSum> {
    let viewport = context.viewport;
    let pixels = match unit {
        "px" => value,
        "pt" => value * (96.0 / 72.0),
        "rem" => value * context.root_font_size.get(),
        "em" => value * context.font_size.get(),
        "vw" => value * viewport.width.get() / 100.0,
        "vh" => value * viewport.height.get() / 100.0,
        "vmin" => value * viewport.width.get().min(viewport.height.get()) / 100.0,
        "vmax" => value * viewport.width.get().max(viewport.height.get()) / 100.0,
        _ => return None,
    };
    Some(LengthSum::absolute(pixels))
}

/// Reduces a value to a [`LengthSum`], following `calc()` when present.
pub(crate) fn parse_sum(text: &str, context: &StyleContext) -> Option<LengthSum> {
    let text = text.trim();
    if let Some(body) = strip_function(text, "calc") {
        let tokens = tokenise_calc(body)?;
        let mut cursor = 0;
        let sum = parse_calc_sum(&tokens, &mut cursor, context)?;
        return (cursor == tokens.len()).then_some(sum);
    }
    match component(text)? {
        Component::Number(value) => Some(LengthSum::number(value)),
        Component::Percentage(value) => {
            Some(LengthSum { pixels: 0.0, fraction: value, numbers: 0.0, unitless: false })
        }
        Component::Dimension(value, unit) => resolve_unit(value, &unit, context),
        Component::Ident(_) => None,
    }
}

/// Parses a value that may be `auto`, a length or a percentage.
pub(crate) fn parse_length(text: &str, context: &StyleContext) -> Option<Length> {
    let trimmed = text.trim();
    if trimmed.eq_ignore_ascii_case("auto") {
        return Some(Length::Auto);
    }
    let sum = parse_sum(trimmed, context)?;
    if sum.unitless {
        // A bare `0` is a valid length; any other bare number is not, and
        // accepting it would quietly make `width: 12` mean `12px`.
        return (sum.numbers == 0.0).then_some(Length::Px(Px::ZERO));
    }
    // A sum with both parts nonzero — `calc(100% - 10px)` — has no shape in
    // `Length`, so it is rejected rather than rounded to whichever half is
    // larger.
    if sum.fraction == 0.0 {
        Some(Length::Px(px(sum.pixels)))
    } else if sum.pixels == 0.0 {
        Some(Length::Fraction(sum.fraction))
    } else {
        None
    }
}

/// Parses a value that must resolve to absolute pixels.
pub(crate) fn parse_px(text: &str, context: &StyleContext) -> Option<Px> {
    match parse_length(text, context)? {
        Length::Px(value) => Some(value),
        _ => None,
    }
}

/// Parses a bare number, allowing `calc()` over numbers.
pub(crate) fn parse_number(text: &str, context: &StyleContext) -> Option<f32> {
    let sum = parse_sum(text, context)?;
    sum.unitless.then_some(sum.numbers)
}

/// Returns the body of `name(...)` when `text` is exactly that call.
pub(crate) fn strip_function<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let text = text.trim();
    let open = text.find('(')?;
    if !text[..open].trim().eq_ignore_ascii_case(name) || !text.ends_with(')') {
        return None;
    }
    Some(&text[open + 1..text.len() - 1])
}

/// One token of a `calc()` expression.
#[derive(Clone, Debug, PartialEq)]
enum CalcToken {
    Value(String),
    Plus,
    Minus,
    Times,
    Divide,
    Open,
    Close,
}

/// Splits a `calc()` body into tokens.
///
/// The only genuinely ambiguous character is `-`: CSS requires whitespace
/// around a binary minus precisely because `-5px` and `10px -5px` would
/// otherwise be indistinguishable. That rule is enforced here rather than
/// guessed at, so `calc(10px -5px)` is rejected instead of silently becoming a
/// subtraction.
fn tokenise_calc(body: &str) -> Option<Vec<CalcToken>> {
    let bytes = body.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        match byte {
            b'(' => {
                tokens.push(CalcToken::Open);
                index += 1;
            }
            b')' => {
                tokens.push(CalcToken::Close);
                index += 1;
            }
            b'*' => {
                tokens.push(CalcToken::Times);
                index += 1;
            }
            b'/' => {
                tokens.push(CalcToken::Divide);
                index += 1;
            }
            b'+' | b'-' if index + 1 < bytes.len() && bytes[index + 1].is_ascii_whitespace() => {
                tokens.push(if byte == b'+' { CalcToken::Plus } else { CalcToken::Minus });
                index += 1;
            }
            _ => {
                let start = index;
                while index < bytes.len() {
                    let byte = bytes[index];
                    if byte.is_ascii_whitespace() || matches!(byte, b'(' | b')' | b'*' | b'/') {
                        break;
                    }
                    if matches!(byte, b'+' | b'-')
                        && index > start
                        && !matches!(bytes[index - 1] | 0x20, b'e')
                    {
                        break;
                    }
                    index += 1;
                }
                if start == index {
                    return None;
                }
                tokens.push(CalcToken::Value(body[start..index].to_string()));
            }
        }
    }
    Some(tokens)
}

fn parse_calc_sum(
    tokens: &[CalcToken],
    cursor: &mut usize,
    context: &StyleContext,
) -> Option<LengthSum> {
    let mut total = parse_calc_product(tokens, cursor, context)?;
    while let Some(token) = tokens.get(*cursor) {
        let sign = match token {
            CalcToken::Plus => 1.0,
            CalcToken::Minus => -1.0,
            _ => break,
        };
        *cursor += 1;
        let next = parse_calc_product(tokens, cursor, context)?;
        total = total.add(next, sign)?;
    }
    Some(total)
}

fn parse_calc_product(
    tokens: &[CalcToken],
    cursor: &mut usize,
    context: &StyleContext,
) -> Option<LengthSum> {
    let mut total = parse_calc_term(tokens, cursor, context)?;
    while let Some(token) = tokens.get(*cursor) {
        let dividing = match token {
            CalcToken::Times => false,
            CalcToken::Divide => true,
            _ => break,
        };
        *cursor += 1;
        let next = parse_calc_term(tokens, cursor, context)?;
        // Only one side of a product may carry a unit; `10px * 2px` is an area,
        // which no CSS property accepts.
        let (scaled, factor) = match (total.unitless, next.unitless) {
            (false, true) => (total, next.numbers),
            (true, false) if !dividing => (next, total.numbers),
            (true, true) => (total, next.numbers),
            _ => return None,
        };
        if dividing && factor == 0.0 {
            return None;
        }
        total = if dividing { scaled.scale(1.0 / factor) } else { scaled.scale(factor) };
    }
    Some(total)
}

fn parse_calc_term(
    tokens: &[CalcToken],
    cursor: &mut usize,
    context: &StyleContext,
) -> Option<LengthSum> {
    match tokens.get(*cursor)? {
        CalcToken::Open => {
            *cursor += 1;
            let inner = parse_calc_sum(tokens, cursor, context)?;
            if tokens.get(*cursor) != Some(&CalcToken::Close) {
                return None;
            }
            *cursor += 1;
            Some(inner)
        }
        CalcToken::Value(text) => {
            *cursor += 1;
            parse_sum(text, context)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::Size;

    fn context() -> StyleContext {
        StyleContext::default()
            .with_root_font_size(px(16.0))
            .with_font_size(px(20.0))
            .with_viewport(Size::new(px(1000.0), px(500.0)))
    }

    #[test]
    fn pixels_are_taken_literally() {
        assert_eq!(parse_length("12px", &context()), Some(Length::Px(px(12.0))));
    }

    #[test]
    fn percentages_become_parent_fractions() {
        assert_eq!(parse_length("50%", &context()), Some(Length::Fraction(0.5)));
    }

    #[test]
    fn auto_is_recognised_case_insensitively() {
        assert_eq!(parse_length("AUTO", &context()), Some(Length::Auto));
    }

    #[test]
    fn rem_resolves_against_the_root_font_size() {
        assert_eq!(parse_length("2rem", &context()), Some(Length::Px(px(32.0))));
    }

    #[test]
    fn em_resolves_against_the_current_font_size() {
        assert_eq!(parse_length("2em", &context()), Some(Length::Px(px(40.0))));
    }

    #[test]
    fn viewport_units_resolve_against_the_viewport() {
        let context = context();
        assert_eq!(parse_length("10vw", &context), Some(Length::Px(px(100.0))));
        assert_eq!(parse_length("10vh", &context), Some(Length::Px(px(50.0))));
        assert_eq!(parse_length("10vmin", &context), Some(Length::Px(px(50.0))));
        assert_eq!(parse_length("10vmax", &context), Some(Length::Px(px(100.0))));
    }

    #[test]
    fn points_use_the_css_ninety_six_over_seventy_two_ratio() {
        assert_eq!(parse_length("72pt", &context()), Some(Length::Px(px(96.0))));
    }

    #[test]
    fn a_bare_zero_is_a_length_but_a_bare_twelve_is_not() {
        assert_eq!(parse_length("0", &context()), Some(Length::Px(Px::ZERO)));
        assert_eq!(parse_length("12", &context()), None);
    }

    #[test]
    fn unknown_units_are_rejected_rather_than_treated_as_pixels() {
        assert_eq!(parse_length("12ch", &context()), None);
        assert_eq!(parse_length("12", &context()), None);
    }

    #[test]
    fn calc_adds_absolute_lengths() {
        assert_eq!(parse_length("calc(10px + 2rem)", &context()), Some(Length::Px(px(42.0))));
    }

    #[test]
    fn calc_subtracts_and_scales() {
        assert_eq!(parse_length("calc((10px + 30px) / 2)", &context()), Some(Length::Px(px(20.0))));
        assert_eq!(parse_length("calc(100px - 40px)", &context()), Some(Length::Px(px(60.0))));
    }

    #[test]
    fn calc_mixing_pixels_and_percentages_is_rejected_not_rounded() {
        assert_eq!(parse_length("calc(100% - 10px)", &context()), None);
    }

    #[test]
    fn calc_scales_a_percentage_by_a_number() {
        assert_eq!(parse_length("calc(100% / 3)", &context()), Some(Length::Fraction(1.0 / 3.0)));
    }

    #[test]
    fn calc_rejects_multiplying_two_lengths() {
        assert_eq!(parse_length("calc(10px * 2px)", &context()), None);
    }

    #[test]
    fn calc_requires_whitespace_around_a_binary_minus() {
        assert_eq!(parse_length("calc(10px -5px)", &context()), None);
    }

    #[test]
    fn a_trailing_token_invalidates_a_single_value() {
        assert_eq!(parse_length("12px 4px", &context()), None);
    }

    #[test]
    fn numbers_survive_calc_but_lengths_do_not_pose_as_numbers() {
        let context = context();
        assert_eq!(parse_number("calc(2 * 3)", &context), Some(6.0));
        assert_eq!(parse_number("2px", &context), None);
    }
}
