//! Resolved typography.
//!
//! Text is the one place where CSS inheritance genuinely earns its keep: an
//! author sets `color` and `font-family` once on a container and expects every
//! label inside it to follow. But this crate resolves one node at a time and
//! has no tree to walk, so inheritance is modelled rather than performed —
//! every field is an `Option`, `None` means "nobody said", and
//! [`TextProperties::inherit_from`] lets a caller that *does* have a tree fold
//! a parent's answers in.
//!
//! The alternative, defaulting each field to a concrete value, would silently
//! override the theme: a widget whose colour came from
//! [`spherekit_ui::Theme`] would be repainted black by a stylesheet that never
//! mentioned colour at all.

use crate::length::{Component, component, parse_number, parse_px};
use crate::parser::split_top_level;
use crate::{StyleContext, color};
use spherekit_core::{Color, Px};
use spherekit_text::{FontStyle, FontWeight, TextAlign};
use spherekit_ui::Label;

/// The text and font properties a stylesheet can set.
///
/// Every field is optional; `None` leaves the widget's own default — usually a
/// theme token — in place.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextProperties {
    /// `color`.
    pub color: Option<Color>,
    /// `font-size`.
    pub font_size: Option<Px>,
    /// `font-family`, in author preference order.
    pub font_family: Option<Vec<String>>,
    /// `font-weight`.
    pub font_weight: Option<FontWeight>,
    /// `font-style`.
    pub font_style: Option<FontStyle>,
    /// `line-height`, already resolved from a number or a length.
    pub line_height: Option<Px>,
    /// `letter-spacing`.
    pub letter_spacing: Option<Px>,
    /// `text-align`.
    pub align: Option<TextAlign>,
    /// `white-space: nowrap`.
    pub no_wrap: bool,
    /// `text-overflow: ellipsis`.
    pub truncate: bool,
}

impl TextProperties {
    /// True when nothing was set, so a caller can skip the whole apply step.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Applies to a [`Label`], leaving unset properties alone.
    ///
    /// Only the first family reaches the label: [`Label::font`] replaces the
    /// family list, and SphereKit's font fallback already walks the system
    /// chain, so passing a CSS fallback stack through would fight it rather
    /// than help.
    pub fn apply_to_label(&self, mut label: Label) -> Label {
        if let Some(color) = self.color {
            label = label.text_color(color);
        }
        if let Some(size) = self.font_size {
            label = label.text_size(size);
        }
        if let Some(family) = self.font_family.as_ref().and_then(|families| families.first()) {
            label = label.font(family.clone());
        }
        if let Some(weight) = self.font_weight {
            label = label.weight(weight);
        }
        if self.font_style.is_some_and(|style| style != FontStyle::Normal) {
            label = label.italic();
        }
        if let Some(height) = self.line_height {
            label = label.line_height(height);
        }
        if let Some(spacing) = self.letter_spacing {
            label = label.letter_spacing(spacing);
        }
        if let Some(align) = self.align {
            label = label.align(align);
        }
        // `truncate` already implies no wrapping, so applying it second would
        // undo nothing but applying `no_wrap` after it is free.
        if self.truncate {
            label = label.truncate();
        } else if self.no_wrap {
            label = label.no_wrap();
        }
        label
    }

    /// Merges a parent's text properties under this one.
    ///
    /// Anything this node stated wins; anything it left unstated is taken from
    /// the parent. `no_wrap` and `truncate` are booleans rather than options
    /// because CSS's own `white-space` and `text-overflow` do not inherit in
    /// any way a native label can act on — a parent that wraps says nothing
    /// about a child that does not.
    pub fn inherit_from(&mut self, parent: &TextProperties) {
        self.color = self.color.or(parent.color);
        self.font_size = self.font_size.or(parent.font_size);
        self.font_family = self.font_family.take().or_else(|| parent.font_family.clone());
        self.font_weight = self.font_weight.or(parent.font_weight);
        self.font_style = self.font_style.or(parent.font_style);
        self.line_height = self.line_height.or(parent.line_height);
        self.letter_spacing = self.letter_spacing.or(parent.letter_spacing);
        self.align = self.align.or(parent.align);
    }
}

/// Applies one text-related declaration, returning false when it is not one.
pub(crate) fn apply_text_declaration(
    text: &mut TextProperties,
    property: &str,
    value: &str,
    context: &StyleContext,
) -> bool {
    match property {
        "color" => {
            if let Some(color) = color::parse_color(value) {
                text.color = Some(color);
            }
        }
        "font-size" => {
            if let Some(size) = parse_font_size(value, context) {
                text.font_size = Some(size);
            }
        }
        "font-family" => {
            let families = parse_font_family(value);
            if !families.is_empty() {
                text.font_family = Some(families);
            }
        }
        "font-weight" => {
            if let Some(weight) = parse_font_weight(value) {
                text.font_weight = Some(weight);
            }
        }
        "font-style" => {
            if let Some(style) = parse_font_style(value) {
                text.font_style = Some(style);
            }
        }
        "line-height" => {
            if let Some(height) = parse_line_height(value, text.font_size, context) {
                text.line_height = Some(height);
            }
        }
        "letter-spacing" => {
            if value.trim().eq_ignore_ascii_case("normal") {
                text.letter_spacing = Some(Px::ZERO);
            } else if let Some(spacing) = parse_px(value, context) {
                text.letter_spacing = Some(spacing);
            }
        }
        "text-align" => {
            if let Some(align) = parse_text_align(value) {
                text.align = Some(align);
            }
        }
        "white-space" => match value.trim().to_ascii_lowercase().as_str() {
            "nowrap" | "pre" => text.no_wrap = true,
            "normal" => text.no_wrap = false,
            _ => {}
        },
        "text-overflow" => match value.trim().to_ascii_lowercase().as_str() {
            "ellipsis" => text.truncate = true,
            "clip" => text.truncate = false,
            _ => {}
        },
        _ => return false,
    }
    true
}

/// Parses `font-size`, including the absolute keywords.
///
/// The keyword scale is the CSS one relative to a 16 px medium, expressed
/// against [`StyleContext::root_font_size`] so an application that scales its
/// UI gets `small` scaled with it.
pub(crate) fn parse_font_size(value: &str, context: &StyleContext) -> Option<Px> {
    let factor = match value.trim().to_ascii_lowercase().as_str() {
        "xx-small" => Some(0.5625),
        "x-small" => Some(0.625),
        "small" => Some(0.8125),
        "medium" => Some(1.0),
        "large" => Some(1.125),
        "x-large" => Some(1.5),
        "xx-large" => Some(2.0),
        _ => None,
    };
    match factor {
        Some(factor) => Some(Px(context.root_font_size.get() * factor)),
        None => parse_px(value, context),
    }
}

fn parse_font_family(value: &str) -> Vec<String> {
    split_top_level(value, ',')
        .into_iter()
        .filter_map(|family| {
            let family = family.trim().trim_matches(['"', '\'']).trim();
            (!family.is_empty()).then(|| family.to_string())
        })
        .collect()
}

/// Parses `font-weight`.
///
/// `lighter` and `bolder` are relative to the inherited weight in CSS. Nothing
/// here knows the inherited weight at the point a declaration is interpreted,
/// so they map to fixed steps. Guessing 300 and 700 is wrong for an already
/// light or already black parent, but it is wrong in a direction the author
/// asked for, which is more useful than ignoring the declaration.
fn parse_font_weight(value: &str) -> Option<FontWeight> {
    let value = value.trim();
    match value.to_ascii_lowercase().as_str() {
        "normal" => return Some(FontWeight::NORMAL),
        "bold" => return Some(FontWeight::BOLD),
        "lighter" => return Some(FontWeight::LIGHT),
        "bolder" => return Some(FontWeight::BOLD),
        _ => {}
    }
    match component(value)? {
        Component::Number(number) if (1.0..=1000.0).contains(&number) => {
            Some(FontWeight(number.round() as u16))
        }
        _ => None,
    }
}

fn parse_font_style(value: &str) -> Option<FontStyle> {
    match value.trim().to_ascii_lowercase().as_str() {
        "normal" => Some(FontStyle::Normal),
        "italic" => Some(FontStyle::Italic),
        "oblique" => Some(FontStyle::Oblique),
        _ => None,
    }
}

/// Parses `line-height`, which is a length, a multiplier or `normal`.
///
/// A unitless multiplier needs a font size to become pixels. It uses the
/// `font-size` resolved for this same element when there is one, and the
/// context's font size otherwise, which is what CSS means by "the element's own
/// font size".
fn parse_line_height(value: &str, font_size: Option<Px>, context: &StyleContext) -> Option<Px> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("normal") {
        return None;
    }
    let base = font_size.unwrap_or(context.font_size);
    if let Some(multiplier) = parse_number(value, context) {
        return Some(Px(base.get() * multiplier));
    }
    if let Some(Component::Percentage(fraction)) = component(value) {
        return Some(Px(base.get() * fraction));
    }
    parse_px(value, context)
}

fn parse_text_align(value: &str) -> Option<TextAlign> {
    match value.trim().to_ascii_lowercase().as_str() {
        "start" => Some(TextAlign::Start),
        "end" => Some(TextAlign::End),
        "left" => Some(TextAlign::Left),
        "right" => Some(TextAlign::Right),
        "center" => Some(TextAlign::Center),
        "justify" => Some(TextAlign::Justify),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::px;

    fn apply(css: &str) -> TextProperties {
        let mut text = TextProperties::default();
        let context = StyleContext::default();
        for declaration in css.split(';') {
            let Some((property, value)) = declaration.split_once(':') else { continue };
            apply_text_declaration(&mut text, property.trim(), value.trim(), &context);
        }
        text
    }

    #[test]
    fn colour_and_size_reach_the_text_properties() {
        let text = apply("color: #ff0000; font-size: 18px");
        assert_eq!(text.color, Some(Color::RED));
        assert_eq!(text.font_size, Some(px(18.0)));
    }

    #[test]
    fn font_family_is_split_and_unquoted() {
        let text = apply("font-family: \"Inter Display\", Arial, sans-serif");
        assert_eq!(
            text.font_family,
            Some(vec!["Inter Display".into(), "Arial".into(), "sans-serif".into()])
        );
    }

    #[test]
    fn numeric_and_keyword_weights_agree() {
        assert_eq!(apply("font-weight: 700").font_weight, Some(FontWeight::BOLD));
        assert_eq!(apply("font-weight: bold").font_weight, Some(FontWeight::BOLD));
        assert_eq!(apply("font-weight: normal").font_weight, Some(FontWeight::NORMAL));
        assert_eq!(apply("font-weight: 250").font_weight, Some(FontWeight(250)));
    }

    #[test]
    fn relative_weight_keywords_step_in_the_asked_for_direction() {
        assert_eq!(apply("font-weight: lighter").font_weight, Some(FontWeight::LIGHT));
        assert_eq!(apply("font-weight: bolder").font_weight, Some(FontWeight::BOLD));
    }

    #[test]
    fn an_out_of_range_weight_is_ignored() {
        assert_eq!(apply("font-weight: 1200").font_weight, None);
    }

    #[test]
    fn font_style_covers_italic_and_oblique() {
        assert_eq!(apply("font-style: italic").font_style, Some(FontStyle::Italic));
        assert_eq!(apply("font-style: oblique").font_style, Some(FontStyle::Oblique));
        assert_eq!(apply("font-style: normal").font_style, Some(FontStyle::Normal));
    }

    #[test]
    fn a_unitless_line_height_multiplies_the_font_size() {
        let text = apply("font-size: 20px; line-height: 1.5");
        assert_eq!(text.line_height, Some(px(30.0)));
    }

    #[test]
    fn a_line_height_length_is_taken_literally() {
        assert_eq!(apply("line-height: 24px").line_height, Some(px(24.0)));
    }

    #[test]
    fn normal_line_height_leaves_the_font_metrics_alone() {
        assert_eq!(apply("line-height: normal").line_height, None);
    }

    #[test]
    fn letter_spacing_normal_is_zero_not_unset() {
        assert_eq!(apply("letter-spacing: normal").letter_spacing, Some(Px::ZERO));
        assert_eq!(apply("letter-spacing: 0.5px").letter_spacing, Some(px(0.5)));
    }

    #[test]
    fn text_align_maps_onto_the_native_alignment() {
        assert_eq!(apply("text-align: center").align, Some(TextAlign::Center));
        assert_eq!(apply("text-align: justify").align, Some(TextAlign::Justify));
        assert_eq!(apply("text-align: sideways").align, None);
    }

    #[test]
    fn nowrap_and_ellipsis_set_their_flags() {
        let text = apply("white-space: nowrap; text-overflow: ellipsis");
        assert!(text.no_wrap);
        assert!(text.truncate);
    }

    #[test]
    fn absolute_font_size_keywords_scale_from_the_root_size() {
        let context = StyleContext::default().with_root_font_size(px(16.0));
        assert_eq!(parse_font_size("large", &context), Some(px(18.0)));
        assert_eq!(parse_font_size("medium", &context), Some(px(16.0)));
    }

    #[test]
    fn inheritance_fills_only_what_the_child_left_unsaid() {
        let parent = apply("color: #ff0000; font-size: 20px; font-weight: 700");
        let mut child = apply("font-size: 12px");
        child.inherit_from(&parent);
        assert_eq!(child.font_size, Some(px(12.0)));
        assert_eq!(child.color, Some(Color::RED));
        assert_eq!(child.font_weight, Some(FontWeight::BOLD));
    }

    #[test]
    fn an_untouched_text_block_is_empty() {
        assert!(TextProperties::default().is_empty());
        assert!(!apply("color: red").is_empty());
    }

    #[test]
    fn unrelated_properties_are_not_claimed_by_the_text_pass() {
        let mut text = TextProperties::default();
        let claimed = apply_text_declaration(&mut text, "width", "10px", &StyleContext::default());
        assert!(!claimed);
    }

    #[test]
    fn text_properties_reach_a_label() {
        let text = apply("color: #ff0000; font-size: 18px; font-family: Inter; text-align: center");
        let label = text.apply_to_label(spherekit_ui::label("hello"));
        assert_eq!(label.content(), "hello");
    }
}
