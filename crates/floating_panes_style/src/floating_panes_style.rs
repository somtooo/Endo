//! The look that Endo's `floating_panes` setting gives the whole app.
//!
//! The setting belongs to the workspace, but most of what it restyles lives in
//! crates that cannot depend on the workspace, such as `ui` and `editor`. The
//! workspace mirrors the setting into a global with [`set_enabled`], and every
//! crate reads it back with [`is_enabled`] while it renders, so that changing
//! the setting restyles open windows without a restart.

use std::sync::Arc;

use gpui::{App, BoxShadow, Global, Hsla, Pixels, Rgba, Styled, hsla, px};
use theme::{ActiveTheme as _, Theme};

/// Distance between two neighboring surfaces, and between a surface and the
/// edge of the workspace.
pub const GAP: Pixels = px(8.);
/// Corner radius of a surface: a dock, the editor area, a pop-up or a menu.
pub const CORNER_RADIUS: Pixels = px(10.);
/// Corner radius of a window that Endo decorates itself. It is
/// [`CORNER_RADIUS`] plus [`GAP`], so that the window's corners and the
/// corners of the surfaces [`GAP`] inside of it share a center.
pub const WINDOW_CORNER_RADIUS: Pixels = px(18.);
/// Distance between a row and the edges of the surface it is in.
pub const ROW_INSET: Pixels = px(4.);
/// Corner radius of a row. It is [`CORNER_RADIUS`] minus [`ROW_INSET`], so that
/// a row in the corner of a surface shares the center of that corner.
pub const ROW_CORNER_RADIUS: Pixels = px(6.);
/// Distance between a block, such as a message or a code block, and the edges
/// of the surface it is in.
pub const BLOCK_INSET: Pixels = px(4.);
/// Corner radius of a block, for the same reason as [`ROW_CORNER_RADIUS`].
pub const BLOCK_CORNER_RADIUS: Pixels = px(6.);
/// Corner radius of a control, such as a button or a chip.
pub const CONTROL_CORNER_RADIUS: Pixels = px(4.);
/// Corner radius of a pop-up that holds a single row, such as a tooltip.
pub const SINGLE_ROW_POPUP_CORNER_RADIUS: Pixels = px(6.);

/// How much of the way toward white (dark themes) or black (light themes)
/// text colors are moved, so that text reads crisply on the floating surfaces.
const INK_LIFT: f32 = 0.15;
/// How much of the way toward white or black the text color is moved for
/// titles and other text that should stand out.
const EMPHASIS_LIFT: f32 = 0.55;

/// Whether panes float, mirrored from the workspace's `floating_panes`
/// setting.
struct FloatingPanesEnabled(bool);

impl Global for FloatingPanesEnabled {}

/// Whether panes float. False until the workspace has set it.
pub fn is_enabled(cx: &App) -> bool {
    cx.try_global::<FloatingPanesEnabled>()
        .is_some_and(|enabled| enabled.0)
}

/// Records whether panes float. Returns whether that changed.
pub fn set_enabled(enabled: bool, cx: &mut App) -> bool {
    let changed = is_enabled(cx) != enabled;
    cx.set_global(FloatingPanesEnabled(enabled));
    changed
}

/// Corner radius of a window that Endo decorates itself, and of anything
/// that sits in one of its corners.
pub fn window_corner_radius(cx: &App) -> Pixels {
    if is_enabled(cx) {
        WINDOW_CORNER_RADIUS
    } else {
        theme::CLIENT_SIDE_DECORATION_ROUNDING
    }
}

/// The color of the workspace between the surfaces. Surfaces only look raised
/// when they are lighter than what is around them, which themes do not
/// promise, so this is the theme's background, darkened as far as it takes to
/// end up below its surfaces.
pub fn gap_color(cx: &App) -> Hsla {
    let colors = cx.theme().colors();
    let surface_lightness = |surface: Hsla| colors.background.blend(surface).l;
    let surface_lightness =
        surface_lightness(colors.panel_background).min(surface_lightness(colors.editor_background));
    let step = if cx.theme().appearance().is_light() {
        0.96
    } else {
        0.68
    };
    let mut color = colors.background;
    color.l = color.l.min(surface_lightness * step);
    // A translucent background shows what is behind the window, which keeps
    // it from getting darker than that. Letting less of it through does.
    color.a += (1. - color.a) / 2.;
    color
}

/// What makes something look raised: a hairline ring, light catching its top
/// edge and shadows underneath. All of it is black or white at a low opacity,
/// so it works on top of the colors of any theme.
pub struct Elevation {
    pub ring: Hsla,
    pub highlight: Hsla,
    pub shadows: Vec<BoxShadow>,
}

/// The elevation of a dock or the editor area, and of blocks raised inside of
/// them.
pub fn card_elevation(cx: &App) -> Elevation {
    if cx.theme().appearance().is_light() {
        let shadow = hsla(0., 0., 0., 0.05);
        Elevation {
            ring: hsla(0., 0., 0., 0.08),
            highlight: hsla(0., 0., 1., 0.3),
            shadows: vec![
                BoxShadow::new(px(0.), px(2.), shadow).blur_radius(px(4.)),
                BoxShadow::new(px(0.), px(1.), shadow).blur_radius(px(2.)),
            ],
        }
    } else {
        Elevation {
            ring: hsla(0., 0., 1., 0.08),
            highlight: hsla(0., 0., 1., 0.02),
            shadows: vec![
                BoxShadow::new(px(0.), px(2.), hsla(0., 0., 0., 0.22)).blur_radius(px(6.)),
                BoxShadow::new(px(0.), px(1.), hsla(0., 0., 0., 0.18)).blur_radius(px(2.)),
            ],
        }
    }
}

/// The elevation of a pop-up, such as a menu, a picker or a tooltip. It sits
/// above the cards, so its shadow reaches further.
pub fn popup_elevation(cx: &App) -> Elevation {
    if cx.theme().appearance().is_light() {
        Elevation {
            ring: hsla(0., 0., 0., 0.1),
            highlight: hsla(0., 0., 1., 0.3),
            shadows: vec![
                BoxShadow::new(px(0.), px(16.), hsla(0., 0., 0., 0.12)).blur_radius(px(40.)),
                BoxShadow::new(px(0.), px(4.), hsla(0., 0., 0., 0.08)).blur_radius(px(12.)),
            ],
        }
    } else {
        Elevation {
            ring: hsla(0., 0., 1., 0.1),
            highlight: hsla(0., 0., 1., 0.02),
            shadows: vec![
                BoxShadow::new(px(0.), px(16.), hsla(0., 0., 0., 0.45)).blur_radius(px(40.)),
                BoxShadow::new(px(0.), px(4.), hsla(0., 0., 0., 0.3)).blur_radius(px(12.)),
            ],
        }
    }
}

/// Gives a surface that is not part of the workspace, such as the page of
/// the settings window, the corners and edge of a floating card.
pub fn style_card<E: Styled>(element: E, cx: &App) -> E {
    let elevation = card_elevation(cx);
    element
        .rounded(CORNER_RADIUS)
        .border_1()
        .border_color(elevation.ring)
        .shadow(with_top_highlight(elevation.shadows, elevation.highlight))
}

/// Gives a pop-up the floating edge: its corner radius, its ring and light on
/// its top edge, and its shadows.
pub fn style_popup<E: Styled>(element: E, cx: &App) -> E {
    let elevation = popup_elevation(cx);
    element
        .rounded(CORNER_RADIUS)
        .border_1()
        .border_color(elevation.ring)
        .shadow(with_top_highlight(elevation.shadows, elevation.highlight))
}

/// Gives a pop-up without a border the floating corner radius and shadows.
pub fn style_borderless_popup<E: Styled>(element: E, cx: &App) -> E {
    element
        .rounded(CORNER_RADIUS)
        .shadow(popup_elevation(cx).shadows)
}

/// The background of a hovered row: `ghost_element.hover` while panes float,
/// so hovering looks the same in every list, and `docked_color` otherwise.
pub fn row_hover_background(docked_color: Hsla, cx: &App) -> Hsla {
    if is_enabled(cx) {
        cx.theme().colors().ghost_element_hover
    } else {
        docked_color
    }
}

/// Turns a row that spans its surface into a rounded row inset from the
/// surface's edges, with a 1px border all around for keyboard focus.
pub fn style_inset_row<E: Styled>(element: E) -> E {
    element.mx(ROW_INSET).border_1().rounded(ROW_CORNER_RADIUS)
}

/// Gives a tab that hangs below a block, such as the Stage and Restore
/// buttons under a diff hunk, the corners and shadow of a raised block.
pub fn style_hanging_tab<E: Styled>(element: E, cx: &App) -> E {
    element
        .rounded_b(BLOCK_CORNER_RADIUS)
        .shadow(card_elevation(cx).shadows)
}

/// Adds the line of light that catches the top edge of something raised to
/// its shadows.
pub fn with_top_highlight(mut shadows: Vec<BoxShadow>, highlight: Hsla) -> Vec<BoxShadow> {
    shadows.push(BoxShadow::new(px(0.), px(1.), highlight).inset());
    shadows
}

/// Moves the theme's text colors a little toward white in dark themes and
/// toward black in light themes, so that text reads crisply on the floating
/// surfaces. Syntax colors are left alone.
pub fn lift_ink(theme: Arc<Theme>) -> Arc<Theme> {
    let mut theme = (*theme).clone();
    let is_light = theme.appearance.is_light();
    let colors = &mut theme.styles.colors;
    for color in [
        &mut colors.text,
        &mut colors.text_muted,
        &mut colors.text_placeholder,
        &mut colors.editor_foreground,
    ] {
        *color = mix_toward_contrast(*color, INK_LIFT, is_light);
    }
    Arc::new(theme)
}

/// The text color for titles and other text that should stand out from the
/// text around it.
pub fn emphasized_text_color(cx: &App) -> Hsla {
    let theme = cx.theme();
    mix_toward_contrast(
        theme.colors().text,
        EMPHASIS_LIFT,
        theme.appearance().is_light(),
    )
}

/// Mixes `color` with white in dark themes and with black in light themes,
/// keeping its opacity.
fn mix_toward_contrast(color: Hsla, amount: f32, is_light: bool) -> Hsla {
    let target = if is_light { 0. } else { 1. };
    let mix = |channel: f32| channel + (target - channel) * amount;
    let rgba = color.to_rgb();
    Rgba {
        r: mix(rgba.r),
        g: mix(rgba.g),
        b: mix(rgba.b),
        a: rgba.a,
    }
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::rgb;

    fn assert_close(actual: Hsla, expected: Rgba) {
        let actual = actual.to_rgb();
        for (actual, expected) in [
            (actual.r, expected.r),
            (actual.g, expected.g),
            (actual.b, expected.b),
            (actual.a, expected.a),
        ] {
            assert!(
                (actual - expected).abs() < 1. / 255.,
                "{actual:?} is not {expected:?}"
            );
        }
    }

    #[test]
    fn test_mix_toward_contrast_lightens_dark_theme_text() {
        let one_dark_text: Hsla = rgb(0xdce0e5).into();
        assert_close(
            mix_toward_contrast(one_dark_text, INK_LIFT, false),
            rgb(0xe1e5e9),
        );
    }

    #[test]
    fn test_mix_toward_contrast_darkens_light_theme_text() {
        let white: Hsla = rgb(0xffffff).into();
        assert_close(mix_toward_contrast(white, 0.5, true), rgb(0x808080));
    }

    #[test]
    fn test_mix_toward_contrast_keeps_opacity() {
        let translucent = hsla(0., 0., 0.5, 0.4);
        assert!((mix_toward_contrast(translucent, INK_LIFT, false).a - 0.4).abs() < 1e-6);
    }

    #[gpui::test]
    fn test_is_enabled_follows_set_enabled(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            assert!(!is_enabled(cx));
            assert!(set_enabled(true, cx));
            assert!(is_enabled(cx));
            assert!(!set_enabled(true, cx));
            assert!(set_enabled(false, cx));
            assert!(!is_enabled(cx));
        });
    }
}
