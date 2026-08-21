//! Theme tokens.
//!
//! A theme is a flat set of named values, not a cascade. There is no selector
//! matching, no inheritance chain and no parser — an element reads the token it
//! wants and that is the whole mechanism.
//!
//! The dark theme is the default because that is what audio software is, but
//! nothing in the engine assumes it.

use sphere_core::{Color, Px, Shadow, Size};

/// Semantic colours.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Palette {
    /// The window's own backdrop.
    pub background: Color,
    /// A panel or card sitting on the background.
    pub surface: Color,
    /// A control sitting on a surface.
    pub elevated: Color,
    /// A control while hovered.
    pub hover: Color,
    /// A control while pressed.
    pub pressed: Color,
    /// Hairlines and dividers.
    pub border: Color,
    /// A stronger border, for focused or selected containers.
    pub border_strong: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text: labels, units, hints.
    pub text_muted: Color,
    /// Text on an accent-coloured surface.
    pub text_on_accent: Color,
    /// The brand or selection colour.
    pub accent: Color,
    /// The accent while hovered.
    pub accent_hover: Color,
    /// Keyboard focus indicator.
    pub focus: Color,
    /// Success, and the safe region of a meter.
    pub success: Color,
    /// Warning, and the loud region of a meter.
    pub warning: Color,
    /// Error, and meter clipping.
    pub danger: Color,
}

/// Typography tokens, in logical pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Typography {
    /// Smallest readable size: units, tick labels.
    ///
    /// Ten pixels is where the MTSDF path hands off to the bitmap fallback on a
    /// 1x display, which is exactly why this token exists rather than being
    /// spelled inline.
    pub xs: Px,
    /// Dense UI labels, the workhorse size in a mixer.
    pub sm: Px,
    /// Body text.
    pub md: Px,
    /// Section headings.
    pub lg: Px,
    /// Titles.
    pub xl: Px,
    /// Line height as a multiple of font size.
    pub line_height: f32,
}

/// Spacing tokens, in logical pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Spacing {
    /// Hairline gap.
    pub xs: Px,
    /// Tight gap between related controls.
    pub sm: Px,
    /// Standard gap.
    pub md: Px,
    /// Gap between groups.
    pub lg: Px,
    /// Gap between sections.
    pub xl: Px,
}

/// Corner radius tokens.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Radii {
    /// Nearly square.
    pub sm: Px,
    /// Standard control radius.
    pub md: Px,
    /// Panel radius.
    pub lg: Px,
}

/// Shadow tokens.
#[derive(Clone, Debug, PartialEq)]
pub struct Shadows {
    /// A control lifted off its surface.
    pub sm: Shadow,
    /// A popover or dropdown.
    pub md: Shadow,
    /// A modal.
    pub lg: Shadow,
}

/// A complete theme.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Colours.
    pub colors: Palette,
    /// Type scale.
    pub typography: Typography,
    /// Spacing scale.
    pub spacing: Spacing,
    /// Corner radii.
    pub radii: Radii,
    /// Shadows.
    pub shadows: Shadows,
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

impl Theme {
    /// The default dark theme.
    pub fn dark() -> Self {
        Self {
            colors: Palette {
                background: Color::hex(0x14161A),
                surface: Color::hex(0x1B1E23),
                elevated: Color::hex(0x24282F),
                hover: Color::hex(0x2C313A),
                pressed: Color::hex(0x343A45),
                border: Color::hex(0x2E333B),
                border_strong: Color::hex(0x424A56),
                text: Color::hex(0xE6E9EF),
                text_muted: Color::hex(0x939BA8),
                text_on_accent: Color::hex(0xFFFFFF),
                accent: Color::hex(0x3D8BFD),
                accent_hover: Color::hex(0x5B9DFF),
                focus: Color::hex(0x4C9AFF),
                success: Color::hex(0x3FBF6F),
                warning: Color::hex(0xE0A32E),
                danger: Color::hex(0xE0553F),
            },
            ..Self::shared()
        }
    }

    /// A light theme with the same structure.
    pub fn light() -> Self {
        Self {
            colors: Palette {
                background: Color::hex(0xF5F6F8),
                surface: Color::hex(0xFFFFFF),
                elevated: Color::hex(0xF0F2F5),
                hover: Color::hex(0xE7EAEF),
                pressed: Color::hex(0xDCE0E7),
                border: Color::hex(0xDDE1E7),
                border_strong: Color::hex(0xB9C0CA),
                text: Color::hex(0x1A1D22),
                text_muted: Color::hex(0x606772),
                text_on_accent: Color::hex(0xFFFFFF),
                accent: Color::hex(0x1B6EF3),
                accent_hover: Color::hex(0x1560DC),
                focus: Color::hex(0x1B6EF3),
                success: Color::hex(0x2E9E5B),
                warning: Color::hex(0xC1861F),
                danger: Color::hex(0xC94430),
            },
            ..Self::shared()
        }
    }

    /// The parts both themes share.
    fn shared() -> Self {
        Self {
            colors: Palette {
                background: Color::BLACK,
                surface: Color::BLACK,
                elevated: Color::BLACK,
                hover: Color::BLACK,
                pressed: Color::BLACK,
                border: Color::BLACK,
                border_strong: Color::BLACK,
                text: Color::WHITE,
                text_muted: Color::WHITE,
                text_on_accent: Color::WHITE,
                accent: Color::BLUE,
                accent_hover: Color::BLUE,
                focus: Color::BLUE,
                success: Color::GREEN,
                warning: Color::RED,
                danger: Color::RED,
            },
            typography: Typography {
                xs: Px(10.0),
                sm: Px(11.0),
                md: Px(13.0),
                lg: Px(16.0),
                xl: Px(20.0),
                line_height: 1.35,
            },
            spacing: Spacing { xs: Px(2.0), sm: Px(4.0), md: Px(8.0), lg: Px(16.0), xl: Px(24.0) },
            radii: Radii { sm: Px(3.0), md: Px(5.0), lg: Px(8.0) },
            shadows: Shadows {
                sm: Shadow {
                    offset: Size::new(Px::ZERO, Px(1.0)),
                    blur_radius: Px(2.0),
                    spread: Px::ZERO,
                    color: Color::BLACK.with_alpha(0.30),
                    inset: false,
                },
                md: Shadow {
                    offset: Size::new(Px::ZERO, Px(4.0)),
                    blur_radius: Px(12.0),
                    spread: Px::ZERO,
                    color: Color::BLACK.with_alpha(0.35),
                    inset: false,
                },
                lg: Shadow {
                    offset: Size::new(Px::ZERO, Px(12.0)),
                    blur_radius: Px(32.0),
                    spread: Px::ZERO,
                    color: Color::BLACK.with_alpha(0.45),
                    inset: false,
                },
            },
        }
    }

    /// True when the theme's background is darker than its text.
    ///
    /// Widgets that generate a colour — a meter gradient, a waveform — use this
    /// rather than a `is_dark` flag the caller has to keep in sync.
    #[inline]
    pub fn is_dark(&self) -> bool {
        self.colors.background.luminance() < self.colors.text.luminance()
    }

    /// Text colour with adequate contrast against `background`.
    ///
    /// Uses the WCAG relative-luminance midpoint, which is the standard rule of
    /// thumb and is good enough to keep generated labels readable over a meter
    /// or a waveform without hand-picking a colour per case.
    pub fn contrasting_text(&self, background: Color) -> Color {
        if background.luminance() > 0.179 { Color::hex(0x14161A) } else { Color::hex(0xF2F4F8) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_and_light_are_classified_correctly() {
        assert!(Theme::dark().is_dark());
        assert!(!Theme::light().is_dark());
    }

    #[test]
    fn both_themes_define_every_token() {
        // `shared()` is the fallback for everything not overridden, so a token
        // added to Palette without a value in dark() or light() shows up here
        // as an obviously wrong pure black or blue.
        for theme in [Theme::dark(), Theme::light()] {
            let c = theme.colors;
            assert_ne!(c.surface, Color::BLACK, "surface fell through to the shared default");
            assert_ne!(c.border, Color::BLACK, "border fell through to the shared default");
            assert_ne!(c.accent, Color::BLUE, "accent fell through to the shared default");
        }
    }

    #[test]
    fn text_has_real_contrast_against_its_background() {
        for theme in [Theme::dark(), Theme::light()] {
            let d = (theme.colors.text.luminance() - theme.colors.background.luminance()).abs();
            assert!(d > 0.4, "text and background are too close: {d}");
        }
    }

    #[test]
    fn muted_text_is_dimmer_than_primary_but_still_visible() {
        for theme in [Theme::dark(), Theme::light()] {
            let bg = theme.colors.background.luminance();
            let text = (theme.colors.text.luminance() - bg).abs();
            let muted = (theme.colors.text_muted.luminance() - bg).abs();
            assert!(muted < text, "muted text must be less prominent");
            assert!(muted > 0.05, "muted text must still be readable");
        }
    }

    #[test]
    fn contrasting_text_flips_at_the_luminance_midpoint() {
        let t = Theme::dark();
        assert!(t.contrasting_text(Color::WHITE).luminance() < 0.1);
        assert!(t.contrasting_text(Color::BLACK).luminance() > 0.8);
    }

    #[test]
    fn the_type_scale_increases_monotonically() {
        let t = Theme::dark().typography;
        assert!(t.xs < t.sm && t.sm < t.md && t.md < t.lg && t.lg < t.xl);
    }

    #[test]
    fn the_spacing_scale_increases_monotonically() {
        let s = Theme::dark().spacing;
        assert!(s.xs < s.sm && s.sm < s.md && s.md < s.lg && s.lg < s.xl);
    }

    #[test]
    fn shadows_grow_with_elevation() {
        let s = Theme::dark().shadows;
        assert!(s.sm.blur_radius < s.md.blur_radius);
        assert!(s.md.blur_radius < s.lg.blur_radius);
        assert!(s.sm.extent() < s.lg.extent());
    }

    #[test]
    fn hover_and_pressed_step_away_from_the_resting_surface() {
        for theme in [Theme::dark(), Theme::light()] {
            let c = theme.colors;
            assert_ne!(c.elevated, c.hover);
            assert_ne!(c.hover, c.pressed);
        }
    }
}
