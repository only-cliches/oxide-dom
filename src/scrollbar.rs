//! Scrollbar geometry, painting, and interaction.
//!
//! Blitz tracks scroll offsets on nodes and supports `overflow: auto/scroll`
//! at the layout level, but it doesn't paint scrollbars. We do it here as a
//! post-pass on the Vello scene the document is painted into.
//!
//! The current implementation supports both vertical and horizontal scrollbars
//! with fixed width (SCROLLBAR_WIDTH), and they correctly inset each other
//! when both are present. No transition animations. CSS theming via
//! `scrollbar-color` is layered on top.

use anyrender::PaintScene;
use blitz_dom::{BaseDocument, Node};
use kurbo::{Affine, RoundedRect};
use peniko::{Color, Fill};
use style::values::specified::box_::Overflow;

/// Width of the painted scrollbar in CSS pixels.
pub(crate) const SCROLLBAR_WIDTH: f32 = 10.0;

/// Minimum thumb length so the bar stays grabbable at any scroll size.
const MIN_THUMB_LENGTH: f32 = 24.0;

/// Resolved colour of a scrollbar's track and thumb.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarColors {
    pub track: Color,
    pub thumb: Color,
}

impl ScrollbarColors {
    /// Subtle defaults that read well on both light and dark backgrounds.
    pub fn defaults() -> Self {
        Self {
            track: Color::from_rgba8(0, 0, 0, 30),
            thumb: Color::from_rgba8(120, 120, 120, 200),
        }
    }
}

/// Host-provided scrollbar theme. When set on an [`Instance`], overrides the
/// per-node heuristic that would otherwise tint the bars from each scroll
/// container's computed `color` property.
#[derive(Debug, Clone, Copy)]
pub struct ScrollbarTheme {
    /// Track colour as a premultiplied-friendly RGBA byte tuple.
    pub track: (u8, u8, u8, u8),
    /// Thumb colour as a premultiplied-friendly RGBA byte tuple.
    pub thumb: (u8, u8, u8, u8),
}

impl ScrollbarTheme {
    pub(crate) fn to_colors(self) -> ScrollbarColors {
        let (tr, tg, tb, ta) = self.track;
        let (hr, hg, hb, ha) = self.thumb;
        ScrollbarColors {
            track: Color::from_rgba8(tr, tg, tb, ta),
            thumb: Color::from_rgba8(hr, hg, hb, ha),
        }
    }
}

/// Which axis a [`ScrollbarRegion`] scrolls. The geometry fields are the
/// same shape for both axes; this tag tells [`hit_scrollbar`] which
/// coordinate to check against the thumb and tells [`ScrollbarDrag`] which
/// coordinate to use when translating a pointer position into a scroll
/// offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScrollAxis {
    Vertical,
    Horizontal,
}

/// Geometry + state for one scrollbar on one scrollable node.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollbarRegion {
    pub node_id: usize,
    pub axis: ScrollAxis,
    /// Track rect: x, y, width, height (in document coordinates).
    pub track: (f32, f32, f32, f32),
    /// Thumb rect: x, y, width, height (in document coordinates).
    pub thumb: (f32, f32, f32, f32),
    /// Maximum scrollable distance along this axis (content - visible).
    pub max_scroll: f32,
}

/// Walk the document and produce scrollbar regions for every node that has
/// overflowing content on the X or Y axis with `overflow-{x,y}: auto | scroll`.
///
/// Returns an empty list if no scrollable nodes are present. The caller is
/// expected to feed this into [`paint_scrollbars`] and use the same list to
/// hit-test mouse input.
pub(crate) fn collect_scrollbar_regions(doc: &BaseDocument) -> Vec<ScrollbarRegion> {
    let mut out = Vec::new();
    let scroll = doc.viewport_scroll();
    let viewport_offset = (-scroll.x as f32, -scroll.y as f32);
    let scale = doc.viewport().scale_f64() as f32;
    let viewport = (
        doc.viewport().window_size.0 as f32 / scale,
        doc.viewport().window_size.1 as f32 / scale,
    );
    let root_id = doc.root_element().id;
    collect_recursive(
        doc,
        root_id,
        (0.0, 0.0),
        viewport_offset,
        viewport,
        &mut out,
    );
    out
}

fn axis_content_size(node: &Node, axis: ScrollAxis, scale: f32) -> f32 {
    let layout = &node.final_layout;
    let mut content_size = match axis {
        ScrollAxis::Horizontal => layout.content_size.width,
        ScrollAxis::Vertical => layout.content_size.height,
    };

    let Some(element_data) = node.element_data() else {
        return content_size;
    };

    let Some(inline_layout) = element_data.inline_layout_data.as_ref() else {
        return content_size;
    };
    let mut inline_layout = inline_layout.clone();

    let inline_size = if scale.is_finite() && scale > 0.0 {
        match axis {
            ScrollAxis::Horizontal => inline_layout.content_widths().max / scale,
            ScrollAxis::Vertical => inline_layout.layout.height() / scale,
        }
    } else {
        match axis {
            ScrollAxis::Horizontal => inline_layout.content_widths().max,
            ScrollAxis::Vertical => inline_layout.layout.height(),
        }
    };
    content_size = content_size.max(inline_size);

    content_size
}

fn max_scroll_distance(node: &Node, axis: ScrollAxis, scale: f32) -> f32 {
    let content = axis_content_size(node, axis, scale);
    let visible = match axis {
        ScrollAxis::Horizontal => node.final_layout.size.width,
        ScrollAxis::Vertical => node.final_layout.size.height,
    };
    (content - visible).max(0.0)
}

fn collect_recursive(
    doc: &BaseDocument,
    node_id: usize,
    parent_origin: (f32, f32),
    viewport_offset: (f32, f32),
    viewport: (f32, f32),
    out: &mut Vec<ScrollbarRegion>,
) {
    let Some(node) = doc.get_node(node_id) else {
        return;
    };
    let layout = &node.final_layout;
    let abs_x = parent_origin.0 + layout.location.x;
    let abs_y = parent_origin.1 + layout.location.y;
    let size = layout.size;
    let scale = doc.viewport().scale_f64() as f32;

    if let Some(styles) = node.primary_styles() {
        let overflow_x = styles.clone_overflow_x();
        let overflow_y = styles.clone_overflow_y();
        let y_scrollable = matches!(overflow_y, Overflow::Auto | Overflow::Scroll);
        let x_scrollable = matches!(overflow_x, Overflow::Auto | Overflow::Scroll);
        // taffy's `scroll_{width,height}()` return the max scrollable
        // distance (content_size - size, floored at 0). Total content size
        // along the axis is `visible + max_scroll`.
        let max_scroll_y = max_scroll_distance(node, ScrollAxis::Vertical, scale);
        let max_scroll_x = max_scroll_distance(node, ScrollAxis::Horizontal, scale);
        // When both axes scroll, the bars meet in the bottom-right corner;
        // shorten each track by the other bar's thickness so they don't
        // overlap or hide content underneath the corner square.
        let needs_vbar = y_scrollable && max_scroll_y > 0.0;
        let needs_hbar = x_scrollable && max_scroll_x > 0.0;
        let v_inset = if needs_hbar { SCROLLBAR_WIDTH } else { 0.0 };
        let h_inset = if needs_vbar { SCROLLBAR_WIDTH } else { 0.0 };

        if needs_vbar {
            let visible_h = size.height;
            let content_h = visible_h + max_scroll_y;
            let track_w = SCROLLBAR_WIDTH;
            let mut track_x = abs_x + size.width - track_w + viewport_offset.0;
            let mut track_y = abs_y + viewport_offset.1;
            let track_h = (size.height - v_inset).max(0.0);
            if track_h <= 0.0 || track_w <= 0.0 {
                // Zero-sized bars are not paintable/clickable.
                // Skip to the next axis check.
            } else {
                track_x = track_x.clamp(0.0, (viewport.0 - track_w).max(0.0));
                track_y = track_y.clamp(0.0, (viewport.1 - track_h).max(0.0));
                let thumb_len = (visible_h / content_h * track_h)
                    .max(MIN_THUMB_LENGTH)
                    .min(track_h);
                let scroll_ratio = (node.scroll_offset.y as f32 / max_scroll_y).clamp(0.0, 1.0);
                let thumb_y = track_y + (track_h - thumb_len) * scroll_ratio;
                out.push(ScrollbarRegion {
                    node_id,
                    axis: ScrollAxis::Vertical,
                    track: (track_x, track_y, track_w, track_h),
                    thumb: (track_x, thumb_y, track_w, thumb_len),
                    max_scroll: max_scroll_y,
                });
            }
        }

        if needs_hbar {
            let visible_w = size.width;
            let content_w = visible_w + max_scroll_x;
            let track_h = SCROLLBAR_WIDTH;
            let mut track_x = abs_x + viewport_offset.0;
            let mut track_y = abs_y + size.height - track_h + viewport_offset.1;
            let track_w = (size.width - h_inset).max(0.0);
            if track_h <= 0.0 || track_w <= 0.0 {
                // Zero-sized bars are not paintable/clickable.
                // Skip to the next node.
            } else {
                track_x = track_x.clamp(0.0, (viewport.0 - track_w).max(0.0));
                track_y = track_y.clamp(0.0, (viewport.1 - track_h).max(0.0));
                let thumb_len = (visible_w / content_w * track_w)
                    .max(MIN_THUMB_LENGTH)
                    .min(track_w);
                let scroll_ratio = (node.scroll_offset.x as f32 / max_scroll_x).clamp(0.0, 1.0);
                let thumb_x = track_x + (track_w - thumb_len) * scroll_ratio;
                out.push(ScrollbarRegion {
                    node_id,
                    axis: ScrollAxis::Horizontal,
                    track: (track_x, track_y, track_w, track_h),
                    thumb: (thumb_x, track_y, thumb_len, track_h),
                    max_scroll: max_scroll_x,
                });
            }
        }
    }

    let child_origin = (
        abs_x - node.scroll_offset.x as f32,
        abs_y - node.scroll_offset.y as f32,
    );
    for child in node.children.iter().copied() {
        collect_recursive(doc, child, child_origin, viewport_offset, viewport, out);
    }
}

/// Paint scrollbar tracks + thumbs over the document. Called inside the same
/// `VelloImageRenderer::render` closure as [`blitz_paint::paint_scene`], so
/// the bars composite on top of the document content.
///
/// `theme_override` short-circuits the per-node colour heuristic when the
/// host has supplied an explicit theme via `Instance::set_scrollbar_theme`.
pub(crate) fn paint_scrollbars<S: PaintScene>(
    scene: &mut S,
    doc: &BaseDocument,
    regions: &[ScrollbarRegion],
    scale: f64,
    theme_override: Option<ScrollbarColors>,
) {
    for region in regions {
        let colors = theme_override.unwrap_or_else(|| resolve_colors(doc, region.node_id));

        let (tx, ty, tw, th) = region.track;
        let track_rect = RoundedRect::new(
            tx as f64,
            ty as f64,
            (tx + tw) as f64,
            (ty + th) as f64,
            (tw as f64) * 0.5,
        );
        scene.fill(
            Fill::NonZero,
            Affine::scale(scale),
            colors.track,
            None,
            &track_rect,
        );

        let (hx, hy, hw, hh) = region.thumb;
        // Inset the thumb a touch so the track shows through at the edges.
        let inset = 1.5_f64;
        let thumb_rect = RoundedRect::new(
            hx as f64 + inset,
            hy as f64 + inset,
            (hx + hw) as f64 - inset,
            (hy + hh) as f64 - inset,
            ((hw as f64) - 2.0 * inset).max(0.5) * 0.5,
        );
        scene.fill(
            Fill::NonZero,
            Affine::scale(scale),
            colors.thumb,
            None,
            &thumb_rect,
        );
    }
}

fn resolve_colors(doc: &BaseDocument, node_id: usize) -> ScrollbarColors {
    // CSS `scrollbar-color` is gecko-gated in stylo so we can't read it
    // directly from servo-mode computed styles. As a useful approximation,
    // tint the scrollbar from the scroll container's `color` (foreground)
    // value: a translucent track and a more opaque thumb. Hosts that need
    // exact control should call `Instance::set_scrollbar_theme`.
    let Some(node) = doc.get_node(node_id) else {
        return ScrollbarColors::defaults();
    };
    let Some(styles) = node.primary_styles() else {
        return ScrollbarColors::defaults();
    };
    let fg = styles.clone_color();
    let fg_srgb = fg.to_color_space(style::color::ColorSpace::Srgb);
    let bg_srgb = styles
        .clone_background_color()
        .resolve_to_absolute(&fg)
        .to_color_space(style::color::ColorSpace::Srgb);

    let bg_luminance = (0.2126 * bg_srgb.components.0
        + 0.7152 * bg_srgb.components.1
        + 0.0722 * bg_srgb.components.2)
        / 255.0;
    if bg_luminance < 0.25 {
        return ScrollbarColors {
            track: Color::from_rgba8(188, 188, 188, 70),
            thumb: Color::from_rgba8(130, 130, 130, 210),
        };
    }
    if bg_luminance > 0.75 {
        return ScrollbarColors {
            track: Color::from_rgba8(64, 64, 64, 70),
            thumb: Color::from_rgba8(120, 120, 120, 210),
        };
    }

    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let r = to_u8(fg_srgb.components.0);
    let g = to_u8(fg_srgb.components.1);
    let b = to_u8(fg_srgb.components.2);

    // If the foreground color is very dark, a pure-derived scrollbar can disappear
    // on dark scenes (e.g. black-on-black). Use a light track/thumb in that case
    // so the control remains visible by default.
    let luminance = (0.2126 * f32::from(r) + 0.7152 * f32::from(g) + 0.0722 * f32::from(b)) / 255.0;
    if luminance < 0.12 {
        return ScrollbarColors {
            track: Color::from_rgba8(188, 188, 188, 70),
            thumb: Color::from_rgba8(130, 130, 130, 210),
        };
    }

    // If the color is very light, keep contrast by darkening the scrollbar.
    if luminance > 0.88 {
        return ScrollbarColors {
            track: Color::from_rgba8(64, 64, 64, 70),
            thumb: Color::from_rgba8(120, 120, 120, 210),
        };
    }

    ScrollbarColors {
        track: Color::from_rgba8(r, g, b, 40),
        thumb: Color::from_rgba8(r, g, b, 180),
    }
}

/// Hit-test scrollbar regions for the given coordinate.
///
/// Returns the region under the point and whether it's on the thumb (drag
/// initiator) or just the track (page step). `None` if no scrollbar is hit.
pub(crate) enum ScrollbarHit {
    Thumb(ScrollbarRegion),
    Track(ScrollbarRegion),
}

pub(crate) fn hit_scrollbar(regions: &[ScrollbarRegion], x: f32, y: f32) -> Option<ScrollbarHit> {
    for region in regions {
        let (tx, ty, tw, th) = region.track;
        if x < tx || x > tx + tw || y < ty || y > ty + th {
            continue;
        }
        let (hx, hy, hw, hh) = region.thumb;
        let on_thumb = match region.axis {
            ScrollAxis::Vertical => y >= hy && y <= hy + hh,
            ScrollAxis::Horizontal => x >= hx && x <= hx + hw,
        };
        if on_thumb {
            return Some(ScrollbarHit::Thumb(*region));
        }
        return Some(ScrollbarHit::Track(*region));
    }
    None
}

/// Active scrollbar drag in progress. Tracks the node being scrolled and the
/// pointer-to-thumb offset captured at MouseDown so the drag feels like the
/// thumb stays pinned under the cursor.
///
/// The geometry fields are axis-agnostic (track_start / track_length / etc.)
/// so the same struct drives both vertical and horizontal drags;
/// [`ScrollAxis`] tells the caller which pointer coordinate to feed in.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollbarDrag {
    pub node_id: usize,
    pub axis: ScrollAxis,
    /// Distance from the thumb's leading edge to the pointer at MouseDown,
    /// so the thumb's leading edge tracks `pointer - grab_offset`.
    pub grab_offset: f32,
    /// Track leading edge / length captured at drag start. We use these to
    /// translate mouse positions into a scroll ratio without re-resolving
    /// layout while the user drags.
    pub track_start: f32,
    pub track_length: f32,
    pub thumb_length: f32,
    pub max_scroll: f32,
}

impl ScrollbarDrag {
    pub fn from_thumb_hit(region: ScrollbarRegion, pointer_x: f32, pointer_y: f32) -> Self {
        match region.axis {
            ScrollAxis::Vertical => Self {
                node_id: region.node_id,
                axis: ScrollAxis::Vertical,
                grab_offset: pointer_y - region.thumb.1,
                track_start: region.track.1,
                track_length: region.track.3,
                thumb_length: region.thumb.3,
                max_scroll: region.max_scroll,
            },
            ScrollAxis::Horizontal => Self {
                node_id: region.node_id,
                axis: ScrollAxis::Horizontal,
                grab_offset: pointer_x - region.thumb.0,
                track_start: region.track.0,
                track_length: region.track.2,
                thumb_length: region.thumb.2,
                max_scroll: region.max_scroll,
            },
        }
    }

    /// Map the current pointer coordinate along this drag's axis to a target
    /// scroll offset (in CSS px). Pass `pointer_y` for vertical drags and
    /// `pointer_x` for horizontal drags.
    pub fn pointer_to_scroll(&self, pointer: f32) -> f32 {
        let thumb_start = (pointer - self.grab_offset).max(self.track_start);
        let track_room = (self.track_length - self.thumb_length).max(1.0);
        let ratio = ((thumb_start - self.track_start) / track_room).clamp(0.0, 1.0);
        ratio * self.max_scroll
    }
}
