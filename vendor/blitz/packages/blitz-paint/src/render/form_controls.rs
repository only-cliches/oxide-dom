use super::ElementCx;
use crate::color::{Color, ToColorColor as _};
use anyrender::PaintScene;
use blitz_dom::{LocalName, local_name};
use kurbo::{Affine, BezPath, Cap, Circle, Join, Point, Rect, RoundedRect, Stroke};
use peniko::Fill;
use style::dom::TElement as _;

impl ElementCx<'_, '_> {
    pub(super) fn draw_input(&self, scene: &mut impl PaintScene) {
        if self.node.local_name() != "input" {
            return;
        }

        let type_attr = self.node.attr(local_name!("type"));
        let disabled = self.node.attr(local_name!("disabled")).is_some();

        // TODO this should be coming from css accent-color, but I couldn't find how to retrieve it
        let accent_color = if disabled {
            Color::from_rgba8(209, 209, 209, 255)
        } else {
            self.style.clone_color().as_srgb_color()
        };

        match type_attr {
            Some("checkbox") | Some("radio") => {
                let Some(checked) = self.element.checkbox_input_checked() else {
                    return;
                };
                if !checked {
                    return;
                }

                let current_color = self.style.clone_color();
                let background_color = self
                    .style
                    .get_background()
                    .background_color
                    .resolve_to_absolute(&current_color)
                    .as_srgb_color();
                let background_color = if background_color == Color::TRANSPARENT {
                    Color::WHITE
                } else {
                    background_color
                };

                match type_attr {
                    Some("checkbox") => {
                        draw_checkbox(
                            scene,
                            self.frame.border_box_path(),
                            self.frame.border_box,
                            self.transform,
                            accent_color,
                        );
                    }
                    Some("radio") => {
                        draw_radio_button(
                            scene,
                            self.frame.border_box,
                            self.transform,
                            accent_color,
                            background_color,
                        );
                    }
                    _ => {}
                }
            }
            Some("range") => {
                let min: f64 = self
                    .node
                    .attr(LocalName::from("min"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let max: f64 = self
                    .node
                    .attr(LocalName::from("max"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(100.0);
                let value: f64 = self
                    .node
                    .attr(LocalName::from("value"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(min);
                let fraction = if max > min {
                    ((value - min) / (max - min)).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                draw_range_slider(
                    scene,
                    fraction,
                    self.frame.content_box,
                    self.transform,
                    accent_color,
                    self.scale,
                );
            }
            _ => {}
        }
    }
}

fn draw_checkbox(
    scene: &mut impl PaintScene,
    border_box_path: BezPath,
    border_box: Rect,
    transform: Affine,
    accent_color: Color,
) {
    scene.fill(
        Fill::NonZero,
        transform,
        accent_color,
        None,
        &border_box_path,
    );

    // Draw the tick mark centred inside the frame.
    let s = border_box.width().min(border_box.height());
    let cx = (border_box.x0 + border_box.x1) / 2.0;
    let cy = (border_box.y0 + border_box.y1) / 2.0;
    let r = s * 0.32;

    let mut path = BezPath::new();
    path.move_to((cx - r * 0.70, cy + r * 0.10));
    path.line_to((cx - r * 0.10, cy + r * 0.70));
    path.line_to((cx + r * 0.80, cy - r * 0.60));

    let stroke_w = (s * 0.12).max(1.5);
    let style = Stroke {
        width: stroke_w,
        join: Join::Round,
        miter_limit: 10.0,
        start_cap: Cap::Round,
        end_cap: Cap::Round,
        dash_pattern: Default::default(),
        dash_offset: 0.0,
    };

    scene.stroke(&style, transform, Color::WHITE, None, &path);
}

fn draw_radio_button(
    scene: &mut impl PaintScene,
    border_box: Rect,
    transform: Affine,
    accent_color: Color,
    background_color: Color,
) {
    let center = border_box.center();
    let outer_radius = border_box.width().min(border_box.height()) / 2.0;
    let outer_ring = Circle::new(center, outer_radius);
    let gap = Circle::new(center, outer_radius * 0.75);
    let inner_circle = Circle::new(center, outer_radius * 0.5);

    scene.fill(Fill::NonZero, transform, accent_color, None, &outer_ring);
    scene.fill(Fill::NonZero, transform, background_color, None, &gap);
    scene.fill(Fill::NonZero, transform, accent_color, None, &inner_circle);
}

fn draw_range_slider(
    scene: &mut impl PaintScene,
    fraction: f64,
    content_box: Rect,
    transform: Affine,
    accent_color: Color,
    scale: f64,
) {
    const TRACK_COLOR: Color = Color::from_rgba8(79, 98, 130, 255);

    let cy = (content_box.y0 + content_box.y1) / 2.0;
    // content_box is in physical pixels; caps must scale with DPI so the thumb
    // keeps the same apparent CSS-pixel size at any device pixel ratio.
    let thumb_r = (content_box.height() / 2.0).min(8.0 * scale).max(3.0 * scale);
    let track_h = (thumb_r * 0.45).max(2.0 * scale);

    // Clamp track horizontally so the thumb never extends outside content_box.
    let x0 = content_box.x0 + thumb_r;
    let x1 = content_box.x1 - thumb_r;
    let thumb_cx = x0 + (x1 - x0).max(0.0) * fraction;

    // Inactive track (full width behind the thumb).
    let track_rect = Rect::new(x0, cy - track_h / 2.0, x1, cy + track_h / 2.0);
    let track_rr = RoundedRect::from_rect(track_rect, track_h / 2.0);
    scene.fill(Fill::NonZero, transform, TRACK_COLOR, None, &track_rr);

    // Active portion (left of thumb).
    if thumb_cx > x0 {
        let active_rect = Rect::new(x0, cy - track_h / 2.0, thumb_cx, cy + track_h / 2.0);
        let active_rr = RoundedRect::from_rect(active_rect, track_h / 2.0);
        scene.fill(Fill::NonZero, transform, accent_color, None, &active_rr);
    }

    // Thumb circle.
    let thumb = Circle::new(Point::new(thumb_cx, cy), thumb_r);
    scene.fill(Fill::NonZero, transform, accent_color, None, &thumb);

    // Small white inner dot for depth.
    let inner_r = (thumb_r * 0.38).max(1.5);
    let inner = Circle::new(Point::new(thumb_cx, cy), inner_r);
    scene.fill(Fill::NonZero, transform, Color::WHITE, None, &inner);
}
