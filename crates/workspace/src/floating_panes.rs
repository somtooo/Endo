use floating_panes_style::{
    BLOCK_CORNER_RADIUS, CORNER_RADIUS, card_elevation, with_top_highlight,
};
pub use floating_panes_style::{GAP, gap_color, window_corner_radius};
use gpui::{
    AnyElement, App, Bounds, BoxShadow, ContentMask, Corners, Div, Edges, Hsla, PathBuilder,
    Pixels, Styled, Window, canvas, div, fill, point, prelude::*, px, size,
};
use settings::{Settings as _, SettingsStore};
use theme::ActiveTheme as _;
use util::ResultExt as _;

use crate::{SidebarRenderState, SidebarSide, WorkspaceSettings};

pub fn enabled(cx: &App) -> bool {
    WorkspaceSettings::get_global(cx).floating_panes
}

/// Keeps [`floating_panes_style`] in step with the setting, for the crates
/// that cannot read it, and re-resolves the theme, whose text colors depend on
/// it.
pub(crate) fn init(cx: &mut App) {
    sync_style(cx);
    cx.observe_global::<SettingsStore>(sync_style).detach();
}

fn sync_style(cx: &mut App) {
    if floating_panes_style::set_enabled(enabled(cx), cx) {
        theme_settings::reload_theme(cx);
    }
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

/// The space the workspace keeps free inside its own edges, so that a surface
/// next to the window edge is one [`GAP`] away from it.
///
/// Next to the threads sidebar the workspace keeps none: the sidebar's rows
/// are already inset from its edge, and adding more would put them further
/// from the surfaces than from the window edge.
pub(crate) fn workspace_edges(sidebar: Option<SidebarRenderState>) -> Edges<Pixels> {
    let mut edges = Edges::all(GAP / 2.);
    if let Some(sidebar) = sidebar.filter(|sidebar| sidebar.open) {
        match sidebar.side {
            SidebarSide::Left => edges.left = px(0.),
            SidebarSide::Right => edges.right = px(0.),
        }
    }
    edges
}

/// Reserves `edges` inside the workspace and fills them, so that the edge of
/// the workspace looks the same as the space between two surfaces.
pub(crate) fn workspace_outset<E: Styled + ParentElement>(
    element: E,
    edges: Edges<Pixels>,
    cx: &App,
) -> E {
    element
        .pt(edges.top)
        .pr(edges.right)
        .pb(edges.bottom)
        .pl(edges.left)
        .child(
            div()
                .absolute()
                .inset_0()
                .border_t(edges.top)
                .border_r(edges.right)
                .border_b(edges.bottom)
                .border_l(edges.left)
                .border_color(gap_color(cx)),
        )
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
/// surfaces, and [`frame`] and [`workspace_outset`] paint [`gap_color`] around
/// them, so that every pixel gets a background exactly once and the surfaces
/// of translucent themes look the same as they do without floating panes. It
/// has to come before the surface, in an element that was padded by `margin`.
pub(crate) fn backdrop(margin: Edges<Pixels>, cx: &App) -> Div {
    inset_by(div(), margin)
        .rounded(CORNER_RADIUS)
        .bg(cx.theme().colors().background)
}

/// The decoration that lifts a surface off the workspace. It has to be the
/// last child of an element that was padded by `margin`, after the surface.
pub(crate) fn frame(margin: Edges<Pixels>, cx: &App) -> Div {
    let elevation = card_elevation(cx);
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
                .shadow(with_top_highlight(Vec::new(), elevation.highlight)),
        )
}

/// Styles `element` as a block that is raised off the floating surface it is
/// in, the way a floating surface is raised off the workspace. While
/// `focused`, its ring takes the theme's focus color, with a faint halo of it
/// around the block.
pub fn raised<E: Styled + ParentElement>(element: E, focused: bool, cx: &App) -> E {
    const SHADOW_REACH: Pixels = px(6.);
    const FOCUS_HALO_WIDTH: Pixels = px(3.);
    const FOCUS_HALO_OPACITY: f32 = 0.18;
    let elevation = card_elevation(cx);
    let shadows = elevation.shadows;
    let focus_color = cx.theme().colors().border_focused;
    let mut surface_shadows = with_top_highlight(Vec::new(), elevation.highlight);
    if focused {
        surface_shadows.push(
            BoxShadow::new(px(0.), px(0.), focus_color.opacity(FOCUS_HALO_OPACITY))
                .spread_radius(FOCUS_HALO_WIDTH),
        );
    }

    element
        .relative()
        .rounded(BLOCK_CORNER_RADIUS)
        .border_1()
        .border_color(if focused { focus_color } else { elevation.ring })
        .shadow(surface_shadows)
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let margin = Edges::all(SHADOW_REACH);
                    paint_shadows_around(bounds, margin, BLOCK_CORNER_RADIUS, &shadows, window)
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
