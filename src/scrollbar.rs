//! Scrollbar geometry, painting, and interaction.
//!
//! Blitz tracks scroll offsets on nodes and supports `overflow: auto/scroll`
//! at the layout level, but it doesn't paint scrollbars. We do it here as a
//! post-pass on the Vello scene the document is painted into.
//!
//! The current implementation is deliberately minimal: vertical scrollbars
//! only, fixed width, no transition animations. CSS theming via
//! `scrollbar-color` is layered on top.

use anyrender::PaintScene;
use blitz_dom::BaseDocument;
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

/// Geometry + state for one scrollbar on one scrollable node.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollbarRegion {
    pub node_id: usize,
    /// Track rect: x, y, width, height (in document coordinates).
    pub track: (f32, f32, f32, f32),
    /// Thumb rect: x, y, width, height (in document coordinates).
    pub thumb: (f32, f32, f32, f32),
    /// Maximum scrollable distance along this axis (content - visible).
    pub max_scroll: f32,
}

/// Walk the document and produce scrollbar regions for every node that has
/// overflowing content on the Y axis with `overflow-y: auto | scroll`.
///
/// Returns an empty list if no scrollable nodes are present. The caller is
/// expected to feed this into [`paint_scrollbars`] and use the same list to
/// hit-test mouse input.
pub(crate) fn collect_scrollbar_regions(doc: &BaseDocument) -> Vec<ScrollbarRegion> {
    let mut out = Vec::new();
    let scroll = doc.viewport_scroll();
    let viewport_offset = (-scroll.x as f32, -scroll.y as f32);
    let root_id = doc.root_element().id;
    collect_recursive(doc, root_id, (0.0, 0.0), viewport_offset, &mut out);
    out
}

fn collect_recursive(
    doc: &BaseDocument,
    node_id: usize,
    parent_origin: (f32, f32),
    viewport_offset: (f32, f32),
    out: &mut Vec<ScrollbarRegion>,
) {
    let Some(node) = doc.get_node(node_id) else {
        return;
    };
    let layout = &node.final_layout;
    let abs_x = parent_origin.0 + layout.location.x;
    let abs_y = parent_origin.1 + layout.location.y;
    let size = layout.size;

    // Decide whether this node should show a vertical scrollbar.
    if let Some(styles) = node.primary_styles() {
        let overflow_y = styles.clone_overflow_y();
        let visible_h = size.height;
        let content_h = layout.scroll_height();
        let max_scroll = (content_h - visible_h).max(0.0);
        let scrollable = matches!(overflow_y, Overflow::Auto | Overflow::Scroll);
        if scrollable && max_scroll > 0.0 {
            let track_w = SCROLLBAR_WIDTH;
            let track_x = abs_x + size.width - track_w + viewport_offset.0;
            let track_y = abs_y + viewport_offset.1;
            let track_h = size.height;
            let thumb_len = (visible_h / content_h * track_h)
                .max(MIN_THUMB_LENGTH)
                .min(track_h);
            let scroll_ratio = if max_scroll > 0.0 {
                (node.scroll_offset.y as f32 / max_scroll).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let thumb_y = track_y + (track_h - thumb_len) * scroll_ratio;
            out.push(ScrollbarRegion {
                node_id,
                track: (track_x, track_y, track_w, track_h),
                thumb: (track_x, thumb_y, track_w, thumb_len),
                max_scroll,
            });
        }
    }

    let child_origin = (
        abs_x - node.scroll_offset.x as f32,
        abs_y - node.scroll_offset.y as f32,
    );
    for child in node.children.iter().copied() {
        collect_recursive(doc, child, child_origin, viewport_offset, out);
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
    let srgb = styles
        .clone_color()
        .to_color_space(style::color::ColorSpace::Srgb);
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let r = to_u8(srgb.components.0);
    let g = to_u8(srgb.components.1);
    let b = to_u8(srgb.components.2);
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
        let (_, hy, _, hh) = region.thumb;
        if y >= hy && y <= hy + hh {
            return Some(ScrollbarHit::Thumb(*region));
        }
        return Some(ScrollbarHit::Track(*region));
    }
    None
}

/// Active scrollbar drag in progress. Tracks the node being scrolled and the
/// pointer-to-thumb offset captured at MouseDown so the drag feels like the
/// thumb stays pinned under the cursor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ScrollbarDrag {
    pub node_id: usize,
    /// Distance from the thumb's top edge to the pointer at MouseDown, so
    /// the thumb's top tracks `pointer.y - grab_offset`.
    pub grab_offset: f32,
    /// Track top / height captured at drag start. We use these to translate
    /// mouse positions into a scroll ratio without re-resolving layout while
    /// the user drags.
    pub track_top: f32,
    pub track_height: f32,
    pub thumb_height: f32,
    pub max_scroll: f32,
}

impl ScrollbarDrag {
    pub fn from_thumb_hit(region: ScrollbarRegion, pointer_y: f32) -> Self {
        Self {
            node_id: region.node_id,
            grab_offset: pointer_y - region.thumb.1,
            track_top: region.track.1,
            track_height: region.track.3,
            thumb_height: region.thumb.3,
            max_scroll: region.max_scroll,
        }
    }

    /// Map a current pointer Y position to a target scroll offset (in CSS px).
    pub fn pointer_to_scroll(&self, pointer_y: f32) -> f32 {
        let thumb_top = (pointer_y - self.grab_offset).max(self.track_top);
        let track_room = (self.track_height - self.thumb_height).max(1.0);
        let ratio = ((thumb_top - self.track_top) / track_room).clamp(0.0, 1.0);
        ratio * self.max_scroll
    }
}
