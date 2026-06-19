//! Geometry, painting, and hit-testing for `<input type="number">` spinner
//! buttons (the up/down arrows on the right edge of a number field).
//!
//! The spinner is drawn as a post-pass overlay on top of the blitz scene,
//! following the same pattern as scrollbars. Geometry is collected after each
//! layout resolve and stored on the [`Instance`] for reuse during hit-testing.

use anyrender::PaintScene;
use blitz_dom::BaseDocument;
use kurbo::{Affine, BezPath, Rect};
use peniko::{Color, Fill};

/// Width of the spinner column in CSS pixels.
pub(crate) const SPINNER_WIDTH: f32 = 16.0;

/// Button geometry for one `<input type="number">` spinner column.
/// All coordinates are in document space (as returned by `absolute_position`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct NumberSpinner {
    pub node_id: usize,
    /// `(x, y, width, height)` of the up-arrow button (top half).
    pub up_button: (f32, f32, f32, f32),
    /// `(x, y, width, height)` of the down-arrow button (bottom half).
    pub down_button: (f32, f32, f32, f32),
}

/// Which half of a spinner was clicked.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SpinnerHit {
    Up(usize),
    Down(usize),
}

/// Compute spinner geometry for the given number-input node IDs.
///
/// Call after `doc.resolve()` so `final_layout` and `absolute_position` are
/// current. Only nodes that are large enough to paint visible buttons are
/// included.
pub(crate) fn collect_number_spinners(
    doc: &BaseDocument,
    node_ids: &[usize],
) -> Vec<NumberSpinner> {
    let mut out = Vec::with_capacity(node_ids.len());
    for &node_id in node_ids {
        let Some(node) = doc.get_node(node_id) else {
            continue;
        };
        let abs = node.absolute_position(0.0, 0.0);
        let w = node.final_layout.size.width;
        let h = node.final_layout.size.height;
        if w <= SPINNER_WIDTH || h < 4.0 {
            continue;
        }
        // Snap every coordinate to an integer pixel boundary so Vello renders
        // crisp edges instead of anti-aliasing across pixel pairs.
        let bx = (abs.x + w - SPINNER_WIDTH).round();
        let by = abs.y.round();
        let half_h = (h / 2.0).floor();
        let bw = SPINNER_WIDTH; // already an integer constant
        out.push(NumberSpinner {
            node_id,
            up_button: (bx, by, bw, half_h),
            down_button: (bx, by + half_h, bw, h - half_h),
        });
    }
    out
}

/// Hit-test `(x, y)` in document coordinates against all spinner buttons.
pub(crate) fn hit_spinner(spinners: &[NumberSpinner], x: f32, y: f32) -> Option<SpinnerHit> {
    for &spinner in spinners {
        let (ux, uy, uw, uh) = spinner.up_button;
        if x >= ux && x < ux + uw && y >= uy && y < uy + uh {
            return Some(SpinnerHit::Up(spinner.node_id));
        }
        let (dx, dy, dw, dh) = spinner.down_button;
        if x >= dx && x < dx + dw && y >= dy && y < dy + dh {
            return Some(SpinnerHit::Down(spinner.node_id));
        }
    }
    None
}

/// Paint spinner buttons as an overlay on the document scene.
///
/// Called inside the same `VelloImageRenderer::render` closure as
/// `blitz_paint::paint_scene`, so the buttons composite on top of the input.
pub(crate) fn paint_number_spinners<S: PaintScene>(
    scene: &mut S,
    spinners: &[NumberSpinner],
    scale: f64,
) {
    let bg = Color::from_rgba8(210, 210, 215, 220);
    let arrow = Color::from_rgba8(70, 70, 75, 255);
    let divider = Color::from_rgba8(160, 160, 165, 200);

    for &spinner in spinners {
        let (bx, by, bw, bh_up) = spinner.up_button;
        let bh_down = spinner.down_button.3;
        let full_h = bh_up + bh_down;

        // Round to integers once; all derived positions stay on the grid.
        let (bx, by, bw, full_h) = (
            bx.round() as f64,
            by.round() as f64,
            bw.round() as f64,
            full_h.round() as f64,
        );
        let bh_up = bh_up.round() as f64;

        // Spinner column background.
        let bg_rect = Rect::new(bx, by, bx + bw, by + full_h);
        scene.fill(Fill::NonZero, Affine::scale(scale), bg, None, &bg_rect);

        // 1 px divider between the two halves.
        let mid_y = by + bh_up;
        let div_rect = Rect::new(bx, mid_y, bx + bw, mid_y + 1.0);
        scene.fill(Fill::NonZero, Affine::scale(scale), divider, None, &div_rect);

        // Up arrow ▲ — center snapped to nearest 0.5px for odd-sized buttons.
        {
            let cx = (bx + bw / 2.0).round();
            let cy = (by + bh_up / 2.0).round();
            let half = (bw.min(bh_up) * 0.28).clamp(2.5, 5.0);
            let mut path = BezPath::new();
            path.move_to((cx, cy - half));
            path.line_to((cx - half * 1.3, cy + half * 0.7));
            path.line_to((cx + half * 1.3, cy + half * 0.7));
            path.close_path();
            scene.fill(Fill::NonZero, Affine::scale(scale), arrow, None, &path);
        }

        // Down arrow ▼.
        {
            let bh_down = bh_down.round() as f64;
            let cx = (bx + bw / 2.0).round();
            let cy = (mid_y + bh_down / 2.0).round();
            let half = (bw.min(bh_down) * 0.28).clamp(2.5, 5.0);
            let mut path = BezPath::new();
            path.move_to((cx, cy + half));
            path.line_to((cx - half * 1.3, cy - half * 0.7));
            path.line_to((cx + half * 1.3, cy - half * 0.7));
            path.close_path();
            scene.fill(Fill::NonZero, Affine::scale(scale), arrow, None, &path);
        }
    }
}
