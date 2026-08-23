//! Stylesheet syntax: rules, declarations, `@media`, and `var()` substitution.
//!
//! The block structure is scanned by hand rather than driven through
//! `cssparser`'s rule-parser traits. The reason is the error contract: this
//! crate promises that a construct it does not model is *ignored*, not
//! reinterpreted and not fatal, and a hand-written scanner makes "skip this
//! block and keep the byte offset" a two-line operation instead of an exercise
//! in satisfying three parser traits. `cssparser` still owns tokenisation of
//! the values themselves, where the fiddly cases actually live.
//!
//! One thing changed from the first version of this crate and is worth stating
//! loudly: rules inside a `@media` block that does not currently match are
//! **retained**, not dropped. Dropping them made a stylesheet's meaning depend
//! on the window size at the moment it was installed, which is exactly the bug
//! a media query exists to prevent.

use crate::length::parse_px;
use crate::selector::{Selector, parse_selector_list, split_top_level_commas};
use crate::{ColorScheme, CssError, StyleContext};
use std::collections::HashMap;

/// One `property: value` pair.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Declaration {
    pub property: String,
    pub value: String,
    pub important: bool,
    /// Position within its own block, which breaks ties between two
    /// declarations of the same property in the same rule.
    pub order: usize,
}

/// One selector with the declarations it carries.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Rule {
    pub selector: Selector,
    pub declarations: Vec<Declaration>,
    /// Every enclosing `@media` condition. All of them must match, which is how
    /// nested media blocks compose.
    pub media: Vec<MediaQueryList>,
    pub order: usize,
}

/// A comma-separated list of media queries: any one of them matching is enough.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MediaQueryList(Vec<MediaQuery>);

impl MediaQueryList {
    pub(crate) fn matches(&self, context: &StyleContext) -> bool {
        self.0.iter().any(|query| query.matches(context))
    }
}

/// A single query: every feature in it must hold.
#[derive(Clone, Debug, PartialEq)]
struct MediaQuery(Vec<MediaFeature>);

impl MediaQuery {
    fn matches(&self, context: &StyleContext) -> bool {
        self.0.iter().all(|feature| feature.matches(context))
    }
}

/// One media feature or media type.
///
/// Lengths are kept as text and resolved at match time, so a query written in
/// `rem` follows the same [`StyleContext`] as the rest of the cascade instead
/// of freezing whatever font size happened to be current at parse time.
#[derive(Clone, Debug, PartialEq)]
enum MediaFeature {
    MinWidth(String),
    MaxWidth(String),
    MinHeight(String),
    MaxHeight(String),
    PrefersColorScheme(ColorScheme),
    /// A media type this runtime always satisfies, such as `all` or `screen`.
    Always,
    /// Anything unrecognised. A query containing one can never match, which is
    /// what CSS requires and what keeps an unknown feature from widening a rule
    /// instead of narrowing it.
    Never,
}

impl MediaFeature {
    fn matches(&self, context: &StyleContext) -> bool {
        let viewport = context.viewport;
        match self {
            MediaFeature::MinWidth(text) => {
                parse_px(text, context).is_some_and(|value| viewport.width >= value)
            }
            MediaFeature::MaxWidth(text) => {
                parse_px(text, context).is_some_and(|value| viewport.width <= value)
            }
            MediaFeature::MinHeight(text) => {
                parse_px(text, context).is_some_and(|value| viewport.height >= value)
            }
            MediaFeature::MaxHeight(text) => {
                parse_px(text, context).is_some_and(|value| viewport.height <= value)
            }
            MediaFeature::PrefersColorScheme(scheme) => context.color_scheme == *scheme,
            MediaFeature::Always => true,
            MediaFeature::Never => false,
        }
    }
}

/// Parses a whole stylesheet into flat rules.
pub(crate) fn parse_rules(source: &str) -> Result<Vec<Rule>, CssError> {
    let source = strip_comments(source);
    let mut rules = Vec::new();
    parse_block(&source, &[], &mut rules)?;
    Ok(rules)
}

fn parse_block(
    source: &str,
    media: &[MediaQueryList],
    rules: &mut Vec<Rule>,
) -> Result<(), CssError> {
    let bytes = source.as_bytes();
    let mut cursor = 0;

    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            break;
        }
        let open = find_top_level_byte(source, cursor, b'{');
        let semicolon = find_top_level_byte(source, cursor, b';');

        // A statement at-rule such as `@import` ends at its semicolon and has
        // no block to recurse into.
        if let Some(semicolon) = semicolon
            && open.is_none_or(|open| semicolon < open)
        {
            cursor = semicolon + 1;
            continue;
        }

        let Some(open) = open else {
            return Err(CssError::InvalidRule { offset: cursor, message: "expected `{`".into() });
        };
        let Some(close) = find_matching_brace(source, open) else {
            return Err(CssError::InvalidRule {
                offset: open,
                message: "unclosed declaration block".into(),
            });
        };
        let prelude = source[cursor..open].trim();
        let body = &source[open + 1..close];

        if let Some(at_rule) = prelude.strip_prefix('@') {
            let (name, condition) = split_at_rule_name(at_rule);
            if name.eq_ignore_ascii_case("media") {
                let mut nested: Vec<MediaQueryList> = media.to_vec();
                nested.push(parse_media_query_list(condition));
                parse_block(body, &nested, rules)?;
            }
            // Every other at-rule — @keyframes, @font-face, @supports — has no
            // native counterpart. Its whole block is skipped rather than having
            // its declarations leak into the enclosing scope.
        } else {
            let declarations = parse_declarations(body)?;
            for selector in parse_selector_list(prelude) {
                let order = rules.len();
                rules.push(Rule {
                    selector,
                    declarations: declarations.clone(),
                    media: media.to_vec(),
                    order,
                });
            }
        }
        cursor = close + 1;
    }
    Ok(())
}

fn split_at_rule_name(text: &str) -> (&str, &str) {
    let end = text.find(|c: char| c.is_whitespace() || c == '(').unwrap_or(text.len());
    (&text[..end], text[end..].trim())
}

/// Parses a media condition such as `(min-width: 600px) and (orientation: landscape), print`.
fn parse_media_query_list(text: &str) -> MediaQueryList {
    let queries = split_top_level_commas(text)
        .into_iter()
        .map(|query| MediaQuery(split_and(query).into_iter().map(parse_media_feature).collect()))
        .collect();
    MediaQueryList(queries)
}

/// Splits a query on the `and` keyword, ignoring `and` inside parentheses.
fn split_and(text: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0
            && index + 3 <= bytes.len()
            && (bytes[index] | 0x20) == b'a'
            && (bytes[index + 1] | 0x20) == b'n'
            && (bytes[index + 2] | 0x20) == b'd'
            && (index == 0 || bytes[index - 1].is_ascii_whitespace())
            && (index + 3 == bytes.len() || bytes[index + 3].is_ascii_whitespace())
        {
            parts.push(&text[start..index]);
            index += 3;
            start = index;
            continue;
        }
        index += 1;
    }
    parts.push(&text[start..]);
    parts
}

fn parse_media_feature(text: &str) -> MediaFeature {
    let text = text.trim();
    let Some(inner) = text.strip_prefix('(').and_then(|rest| rest.strip_suffix(')')) else {
        // A bare media type. Everything this runtime draws is a screen.
        return match text.to_ascii_lowercase().as_str() {
            "all" | "screen" => MediaFeature::Always,
            _ => MediaFeature::Never,
        };
    };
    let Some((name, value)) = inner.split_once(':') else {
        return MediaFeature::Never;
    };
    let value = value.trim();
    match name.trim().to_ascii_lowercase().as_str() {
        "min-width" => MediaFeature::MinWidth(value.to_string()),
        "max-width" => MediaFeature::MaxWidth(value.to_string()),
        "min-height" => MediaFeature::MinHeight(value.to_string()),
        "max-height" => MediaFeature::MaxHeight(value.to_string()),
        "prefers-color-scheme" => match value.to_ascii_lowercase().as_str() {
            "dark" => MediaFeature::PrefersColorScheme(ColorScheme::Dark),
            "light" => MediaFeature::PrefersColorScheme(ColorScheme::Light),
            _ => MediaFeature::Never,
        },
        _ => MediaFeature::Never,
    }
}

/// Parses a declaration block body.
pub(crate) fn parse_declarations(block: &str) -> Result<Vec<Declaration>, CssError> {
    let mut declarations = Vec::new();
    for (order, raw) in split_top_level(block, ';').into_iter().enumerate() {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        let Some(colon) = raw.find(':') else {
            return Err(CssError::InvalidDeclaration { declaration: raw.into() });
        };
        let property = normalize_property(raw[..colon].trim());
        if property.is_empty() {
            return Err(CssError::InvalidDeclaration { declaration: raw.into() });
        }
        let (value, important) = split_important(raw[colon + 1..].trim());
        declarations.push(Declaration { property, value, important, order });
    }
    Ok(declarations)
}

/// Splits a trailing `!important` off a value, tolerating `! important`.
fn split_important(value: &str) -> (String, bool) {
    if let Some(bang) = value.rfind('!')
        && value[bang + 1..].trim().eq_ignore_ascii_case("important")
    {
        return (value[..bang].trim_end().to_string(), true);
    }
    (value.to_string(), false)
}

/// Normalises a property name to lower-case kebab case.
///
/// The camel-case split exists because the React host serialises a JavaScript
/// `style` object straight through, so `borderTopWidth` and `border-top-width`
/// have to mean the same thing. Custom properties are exempt: `--Brand` and
/// `--brand` are different properties in CSS, and rewriting either one would
/// break a stylesheet that uses both.
pub(crate) fn normalize_property(property: &str) -> String {
    let property = property.trim();
    if property.starts_with("--") {
        return property.to_string();
    }
    let mut result = String::with_capacity(property.len());
    for character in property.chars() {
        if character.is_ascii_uppercase() {
            result.push('-');
            result.push(character.to_ascii_lowercase());
        } else {
            result.push(character.to_ascii_lowercase());
        }
    }
    result
}

/// How many `var()` indirections are followed before giving up.
///
/// A cycle — `--a: var(--b); --b: var(--a)` — is legal to write and impossible
/// to resolve, so it has to be bounded somewhere. The limit is a depth rather
/// than a visited set because a value can legitimately mention the same
/// variable twice, and only recursion through it is a cycle.
const MAX_VAR_DEPTH: usize = 16;

/// Replaces every `var(--name, fallback)` in a value.
///
/// Returns `None` when a variable is neither defined nor given a fallback, or
/// when substitution recurses too deeply. CSS calls that "invalid at computed
/// value time"; here the declaration is simply ignored, which is the same
/// answer this crate gives to any value it cannot interpret.
pub(crate) fn substitute_vars(
    value: &str,
    variables: &HashMap<String, String>,
    depth: usize,
) -> Option<String> {
    if !contains_var(value) {
        return Some(value.to_string());
    }
    if depth >= MAX_VAR_DEPTH {
        return None;
    }
    let Some(start) = find_var(value) else {
        return Some(value.to_string());
    };
    let open = start + 3;
    let close = find_matching_paren(value, open)?;
    let inner = &value[open + 1..close];
    let (name, fallback) = match inner.split_once(',') {
        Some((name, fallback)) => (name.trim(), Some(fallback.trim())),
        None => (inner.trim(), None),
    };
    let replacement = match variables.get(name) {
        Some(defined) => substitute_vars(defined, variables, depth + 1)?,
        None => substitute_vars(fallback?, variables, depth + 1)?,
    };
    let tail = substitute_vars(&value[close + 1..], variables, depth)?;
    Some(format!("{}{}{}", &value[..start], replacement, tail))
}

fn contains_var(value: &str) -> bool {
    find_var(value).is_some()
}

/// Finds the next `var(` that starts a function rather than ending an identifier.
fn find_var(value: &str) -> Option<usize> {
    let bytes = value.as_bytes();
    let mut index = 0;
    while index + 4 <= bytes.len() {
        if (bytes[index] | 0x20) == b'v'
            && (bytes[index + 1] | 0x20) == b'a'
            && (bytes[index + 2] | 0x20) == b'r'
            && bytes[index + 3] == b'('
            && (index == 0 || !is_ident_byte(bytes[index - 1]))
        {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

fn find_matching_paren(value: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (index, byte) in value.bytes().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// Splits a value on whitespace that is not inside parentheses.
pub(crate) fn split_components(value: &str) -> Vec<&str> {
    let mut values = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            character if character.is_ascii_whitespace() && depth == 0 => {
                if start < index {
                    values.push(&value[start..index]);
                }
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if start < value.len() {
        values.push(&value[start..]);
    }
    values
}

/// Splits on a separator that is not inside parentheses.
pub(crate) fn split_top_level(value: &str, separator: char) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    for (index, character) in value.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            character if character == separator && depth == 0 => {
                parts.push(&value[start..index]);
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

/// Removes `/* */` comments, preserving newlines so byte offsets stay usable.
fn strip_comments(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(character) = chars.next() {
                if character == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
                if character == '\n' {
                    result.push('\n');
                }
            }
        } else {
            result.push(character);
        }
    }
    result
}

fn find_top_level_byte(source: &str, start: usize, wanted: u8) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    for (offset, byte) in bytes.iter().enumerate().skip(start) {
        if let Some(current_quote) = quote {
            if *byte == current_quote && (offset == 0 || bytes[offset - 1] != b'\\') {
                quote = None;
            }
            continue;
        }
        match *byte {
            b'"' | b'\'' => quote = Some(*byte),
            b'(' => depth += 1,
            b')' => depth = depth.saturating_sub(1),
            byte if byte == wanted && depth == 0 => return Some(offset),
            _ => {}
        }
    }
    None
}

fn find_matching_brace(source: &str, open: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    let mut depth = 0usize;
    let mut quote = None;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        if let Some(current_quote) = quote {
            if *byte == current_quote && (offset == 0 || bytes[offset - 1] != b'\\') {
                quote = None;
            }
            continue;
        }
        match *byte {
            b'"' | b'\'' => quote = Some(*byte),
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use spherekit_core::{Size, px};

    fn variables(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    }

    fn viewport(width: f32, height: f32) -> StyleContext {
        StyleContext::default().with_viewport(Size::new(px(width), px(height)))
    }

    #[test]
    fn declarations_split_on_semicolons_and_keep_their_order() {
        let declarations = parse_declarations("color: red; width: 2px").unwrap();
        assert_eq!(declarations.len(), 2);
        assert_eq!(declarations[1].property, "width");
        assert_eq!(declarations[1].order, 1);
    }

    #[test]
    fn a_declaration_with_no_colon_is_an_error() {
        assert!(parse_declarations("padding").is_err());
    }

    #[test]
    fn important_is_recognised_with_or_without_a_space() {
        let declarations = parse_declarations("a: 1 !important; b: 2 ! important; c: 3").unwrap();
        assert!(declarations[0].important);
        assert_eq!(declarations[0].value, "1");
        assert!(declarations[1].important);
        assert!(!declarations[2].important);
    }

    #[test]
    fn camel_case_property_names_become_kebab_case() {
        assert_eq!(normalize_property("borderTopWidth"), "border-top-width");
        assert_eq!(normalize_property("  Color "), "-color");
    }

    #[test]
    fn custom_property_names_keep_their_case() {
        assert_eq!(normalize_property("--Brand"), "--Brand");
        assert_ne!(normalize_property("--Brand"), normalize_property("--brand"));
    }

    #[test]
    fn comments_are_removed_without_shifting_lines() {
        let stripped = strip_comments("a /* one\ntwo */ b");
        assert_eq!(stripped, "a \n b");
    }

    #[test]
    fn a_semicolon_at_rule_is_skipped_without_swallowing_the_next_rule() {
        let rules = parse_rules("@import url(x.css); .a { width: 1px; }").unwrap();
        assert_eq!(rules.len(), 1);
    }

    #[test]
    fn unknown_at_rules_take_their_whole_block_with_them() {
        let rules =
            parse_rules("@keyframes spin { from { opacity: 0 } } .a { width: 1px }").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].declarations[0].property, "width");
    }

    #[test]
    fn media_rules_are_retained_and_carry_their_condition() {
        let rules = parse_rules("@media (min-width: 600px) { .a { width: 1px } }").unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].media.len(), 1);
    }

    #[test]
    fn min_and_max_width_compare_against_the_viewport() {
        let rules = parse_rules("@media (min-width: 600px) { .a { width: 1px } }").unwrap();
        assert!(rules[0].media[0].matches(&viewport(800.0, 600.0)));
        assert!(!rules[0].media[0].matches(&viewport(400.0, 600.0)));
    }

    #[test]
    fn media_heights_work_the_same_way() {
        let rules = parse_rules("@media (max-height: 500px) { .a { width: 1px } }").unwrap();
        assert!(rules[0].media[0].matches(&viewport(800.0, 400.0)));
        assert!(!rules[0].media[0].matches(&viewport(800.0, 600.0)));
    }

    #[test]
    fn and_requires_every_feature() {
        let rules =
            parse_rules("@media (min-width: 600px) and (max-width: 900px) { .a { width: 1px } }")
                .unwrap();
        assert!(rules[0].media[0].matches(&viewport(800.0, 600.0)));
        assert!(!rules[0].media[0].matches(&viewport(1000.0, 600.0)));
    }

    #[test]
    fn a_comma_list_needs_only_one_query_to_match() {
        let rules =
            parse_rules("@media (max-width: 400px), (min-width: 900px) { .a { width: 1px } }")
                .unwrap();
        assert!(rules[0].media[0].matches(&viewport(300.0, 600.0)));
        assert!(rules[0].media[0].matches(&viewport(1000.0, 600.0)));
        assert!(!rules[0].media[0].matches(&viewport(600.0, 600.0)));
    }

    #[test]
    fn prefers_color_scheme_reads_the_style_context() {
        let rules =
            parse_rules("@media (prefers-color-scheme: dark) { .a { width: 1px } }").unwrap();
        let dark = StyleContext::default().with_color_scheme(ColorScheme::Dark);
        let light = StyleContext::default().with_color_scheme(ColorScheme::Light);
        assert!(rules[0].media[0].matches(&dark));
        assert!(!rules[0].media[0].matches(&light));
    }

    #[test]
    fn an_unknown_media_feature_never_matches() {
        let rules = parse_rules("@media (orientation: landscape) { .a { width: 1px } }").unwrap();
        assert!(!rules[0].media[0].matches(&viewport(800.0, 600.0)));
    }

    #[test]
    fn nested_media_blocks_must_both_hold() {
        let rules = parse_rules(
            "@media (min-width: 600px) { @media (prefers-color-scheme: dark) { .a { width: 1px } } }",
        )
        .unwrap();
        assert_eq!(rules[0].media.len(), 2);
    }

    #[test]
    fn var_substitutes_a_defined_value() {
        let vars = variables(&[("--brand", "#ff0000")]);
        assert_eq!(substitute_vars("var(--brand)", &vars, 0).as_deref(), Some("#ff0000"));
    }

    #[test]
    fn var_falls_back_when_undefined() {
        let vars = variables(&[]);
        assert_eq!(substitute_vars("var(--gap, 8px)", &vars, 0).as_deref(), Some("8px"));
    }

    #[test]
    fn an_undefined_var_with_no_fallback_invalidates_the_declaration() {
        assert_eq!(substitute_vars("var(--gap)", &variables(&[]), 0), None);
    }

    #[test]
    fn var_substitution_is_recursive() {
        let vars = variables(&[("--a", "var(--b)"), ("--b", "4px")]);
        assert_eq!(substitute_vars("var(--a)", &vars, 0).as_deref(), Some("4px"));
    }

    #[test]
    fn a_var_cycle_terminates_instead_of_hanging() {
        let vars = variables(&[("--a", "var(--b)"), ("--b", "var(--a)")]);
        assert_eq!(substitute_vars("var(--a)", &vars, 0), None);
    }

    #[test]
    fn several_vars_in_one_value_are_all_replaced() {
        let vars = variables(&[("--x", "2px"), ("--y", "4px")]);
        assert_eq!(substitute_vars("var(--x) var(--y)", &vars, 0).as_deref(), Some("2px 4px"));
    }

    #[test]
    fn a_word_ending_in_var_is_not_a_var_call() {
        let vars = variables(&[]);
        assert_eq!(substitute_vars("mivar(1)", &vars, 0).as_deref(), Some("mivar(1)"));
    }

    #[test]
    fn values_split_on_whitespace_outside_parentheses() {
        assert_eq!(split_components("1px rgb(1, 2, 3) 4px"), vec!["1px", "rgb(1, 2, 3)", "4px"]);
    }

    #[test]
    fn an_unclosed_block_is_reported_rather_than_silently_truncated() {
        assert!(matches!(parse_rules(".a { width: 1px"), Err(CssError::InvalidRule { .. })));
    }
}
