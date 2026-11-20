use gpui::{
    AnyElement, App, Bounds, BoxShadow, ContentMask, Corners, Div, Edges, Hsla, PathBuilder,
    Pixels, Styled, Window, canvas, div, fill, hsla, point, prelude::*, px, size,
};
use settings::Settings as _;
use theme::ActiveTheme as _;
use util::ResultExt as _;

use crate::WorkspaceSettings;

/// Distance between two neighboring surfaces, and between a surface and the
/// edge of the workspace.
pub const GAP: Pixels = px(8.);
pub(crate) const CORNER_RADIUS: Pixels = px(10.);

pub fn enabled(cx: &App) -> bool {
    WorkspaceSettings::get_global(cx).floating_panes
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
        0.65
    };
    let mut color = colors.background;
    color.l = color.l.min(surface_lightness * step);
    // A translucent background shows what is behind the window, which keeps
    // it from getting darker than that. Letting less of it through does.
    color.a += (1. - color.a) / 2.;
    color
}

/// The space that every surface keeps free around itself. Two neighbors add
/// up to one [`GAP`], and so do a surface and the edge of the workspace.
pub(crate) fn margin() -> Edges<Pixels> {
    Edges::all(GAP / 2.)
}

/// Reserves the space around a surface that its frame paints into.
pub(crate) fn outset<E: Styled>(element: E) -> E {
    element.p(GAP / 2.)
}

/// Rounds a surface. Content is clipped to its bounding box, the corners are
/// finished by [`frame`].
pub(crate) fn surface<E: Styled>(element: E) -> E {
    element.rounded(CORNER_RADIUS).overflow_hidden()
}

/// Makes `container` hold `content` as a floating surface that keeps `margin`
/// free around itself.
pub(crate) fn float<E: Styled + ParentElement>(
    container: E,
    content: AnyElement,
    margin: Edges<Pixels>,
    cx: &App,
) -> E {
    container
        .pt(margin.top)
        .pr(margin.right)
        .pb(margin.bottom)
        .pl(margin.left)
        .relative()
        .child(backdrop(margin, cx))
        .child(
            surface(div())
                .relative()
                .size_full()
                .bg(cx.theme().colors().editor_background)
                .child(content),
        )
        .child(frame(margin, cx))
}

/// Fills the padding added by [`outset`] to the workspace itself, so that the
/// edge of the workspace looks the same as the space between two surfaces.
pub(crate) fn workspace_edge(cx: &App) -> Div {
    div()
        .absolute()
        .inset_0()
        .border(GAP / 2.)
        .border_color(gap_color(cx))
}

fn inset_by<E: Styled>(element: E, margin: Edges<Pixels>) -> E {
    element
        .absolute()
        .top(margin.top)
        .right(margin.right)
        .bottom(margin.bottom)
        .left(margin.left)
}

/// The workspace background underneath a surface. The workspace does not paint
/// a background of its own when panes float. Instead this paints it under the
/// surfaces, and [`frame`] and [`workspace_edge`] paint [`gap_color`] around
/// them, so that every pixel gets a background exactly once and the surfaces
/// of translucent themes look the same as they do without floating panes. It
/// has to come before the surface, in an element that was padded by `margin`.
pub(crate) fn backdrop(margin: Edges<Pixels>, cx: &App) -> Div {
    inset_by(div(), margin)
        .rounded(CORNER_RADIUS)
        .bg(cx.theme().colors().background)
}

struct Elevation {
    ring: Hsla,
    highlight: Hsla,
    shadows: [BoxShadow; 2],
}

/// What makes something look raised: a hairline ring, light catching its top
/// edge and a short shadow underneath. All of it is black or white at a low
/// opacity, so it works on top of the colors of any theme.
fn elevation(cx: &App) -> Elevation {
    let is_light = cx.theme().appearance().is_light();
    let shadow = hsla(0., 0., 0., if is_light { 0.05 } else { 0.18 });
    Elevation {
        ring: if is_light {
            hsla(0., 0., 0., 0.08)
        } else {
            hsla(0., 0., 1., 0.06)
        },
        highlight: hsla(0., 0., 1., if is_light { 0.3 } else { 0.03 }),
        shadows: [
            BoxShadow::new(px(0.), px(2.), shadow).blur_radius(px(4.)),
            BoxShadow::new(px(0.), px(1.), shadow).blur_radius(px(2.)),
        ],
    }
}

/// The decoration that lifts a surface off the workspace. It has to be the
/// last child of an element that was padded by `margin`, after the surface.
pub(crate) fn frame(margin: Edges<Pixels>, cx: &App) -> Div {
    let elevation = elevation(cx);
    let background = gap_color(cx);
    let shadows = elevation.shadows;

    div()
        .absolute()
        .inset_0()
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    paint_surround(bounds, margin, background, &shadows, window)
                },
            )
            .absolute()
            .size_full(),
        )
        .child(
            inset_by(div(), margin)
                .rounded(CORNER_RADIUS)
                .border_1()
                .border_color(elevation.ring)
                .shadow(vec![
                    BoxShadow::new(px(0.), px(1.), elevation.highlight).inset(),
                ]),
        )
}

/// Styles `element` as a block that is raised off the floating surface it is
/// in, the way a floating surface is raised off the workspace.
pub fn raised<E: Styled + ParentElement>(element: E, cx: &App) -> E {
    const SHADOW_REACH: Pixels = px(6.);
    let elevation = elevation(cx);
    let shadows = elevation.shadows;

    element
        .relative()
        .rounded(CORNER_RADIUS)
        .border_1()
        .border_color(elevation.ring)
        .shadow(vec![
            BoxShadow::new(px(0.), px(1.), elevation.highlight).inset(),
        ])
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let margin = Edges::all(SHADOW_REACH);
                    paint_shadows_around(bounds, margin, CORNER_RADIUS, &shadows, window)
                },
            )
            .absolute()
            .inset(-SHADOW_REACH - px(1.)),
        )
}

/// The parts of `bounds` that are outside of the box that is left when
/// `margin` is taken off of it.
fn surround(bounds: Bounds<Pixels>, margin: Edges<Pixels>) -> [Bounds<Pixels>; 4] {
    let inner = Bounds::from_corners(
        bounds.origin + point(margin.left, margin.top),
        bounds.bottom_right() - point(margin.right, margin.bottom),
    );
    [
        Bounds::new(bounds.origin, size(bounds.size.width, margin.top)),
        Bounds::new(
            point(bounds.left(), inner.bottom()),
            size(bounds.size.width, margin.bottom),
        ),
        Bounds::new(
            point(bounds.left(), inner.top()),
            size(margin.left, inner.size.height),
        ),
        Bounds::new(
            point(inner.right(), inner.top()),
            size(margin.right, inner.size.height),
        ),
    ]
}

/// Paints the drop shadows of the box that is `margin` inside of `bounds`.
///
/// Not every renderer cuts the element out of its drop shadow, and the shadow
/// must not darken the content of the box or show through a translucent one,
/// so it is only painted around the box.
fn paint_shadows_around(
    bounds: Bounds<Pixels>,
    margin: Edges<Pixels>,
    corner_radius: Pixels,
    shadows: &[BoxShadow],
    window: &mut Window,
) {
    let inner = Bounds::from_corners(
        bounds.origin + point(margin.left, margin.top),
        bounds.bottom_right() - point(margin.right, margin.bottom),
    );
    for edge in surround(bounds, margin) {
        window.with_content_mask(Some(ContentMask { bounds: edge }), |window| {
            window.paint_drop_shadows(inner, Corners::all(corner_radius), shadows)
        });
    }
}

/// Paints the workspace background and the surface's drop shadow over
/// everything in `bounds` that is outside of the rounded surface.
///
/// GPUI can only clip to rectangles, so content that paints its own background
/// pokes out of a surface's rounded corners, and this covers it.
fn paint_surround(
    bounds: Bounds<Pixels>,
    margin: Edges<Pixels>,
    color: Hsla,
    shadows: &[BoxShadow],
    window: &mut Window,
) {
    // Between whole pixels, the fills below would only cover part of the
    // pixels they meet in, and what is underneath would show through as a
    // seam.
    let scale_factor = window.scale_factor();
    let snap = |length: Pixels| px((f32::from(length) * scale_factor).round() / scale_factor);
    let snap = |position: gpui::Point<Pixels>| point(snap(position.x), snap(position.y));
    let surface = Bounds::from_corners(
        snap(bounds.origin + point(margin.left, margin.top)),
        snap(bounds.bottom_right() - point(margin.right, margin.bottom)),
    );
    let margin = Edges {
        top: surface.top() - bounds.top(),
        right: bounds.right() - surface.right(),
        bottom: bounds.bottom() - surface.bottom(),
        left: surface.left() - bounds.left(),
    };

    for edge in surround(bounds, margin) {
        window.paint_quad(fill(edge, color));
    }
    paint_shadows_around(bounds, margin, CORNER_RADIUS, shadows, window);

    // How far the control points of a cubic Bézier sit from the end points of
    // the quarter circle it approximates, as a fraction of the radius.
    const QUARTER_CIRCLE: f32 = 0.5523;
    let radius = CORNER_RADIUS;
    let control = radius * (1. - QUARTER_CIRCLE);
    for (corner, x, y) in [
        (surface.origin, 1., 1.),
        (surface.top_right(), -1., 1.),
        (surface.bottom_right(), -1., -1.),
        (surface.bottom_left(), 1., -1.),
    ] {
        let mut path = PathBuilder::fill();
        path.move_to(corner + point(px(0.), radius * y));
        path.cubic_bezier_to(
            corner + point(radius * x, px(0.)),
            corner + point(px(0.), control * y),
            corner + point(control * x, px(0.)),
        );
        path.line_to(corner);
        path.close();
        if let Some(path) = path.build().log_err() {
            window.paint_path(path, color);
        }
    }
}
