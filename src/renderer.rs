use anyrender::ImageRenderer;
use anyrender::PaintScene;
use anyrender_vello::VelloImageRenderer;
use blitz_dom::BaseDocument;
use kurbo::{Affine, Rect};
use peniko::Fill;

use crate::scrollbar::{ScrollbarColors, ScrollbarRegion};

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputCaret {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub color: peniko::Color,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputSelection {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Renderer: blitz-dom layout + paint → CPU RGBA8 buffer → upload to a wgpu texture.
pub(crate) struct Painter {
    queue: std::sync::Arc<wgpu::Queue>,
    width: u32,
    height: u32,
    vello: VelloImageRenderer,
    pub(crate) cpu_buffer: Vec<u8>,
    padded_buffer: Vec<u8>,
}

impl Painter {
    pub fn new(
        _device: std::sync::Arc<wgpu::Device>,
        queue: std::sync::Arc<wgpu::Queue>,
        width: u32,
        height: u32,
    ) -> Self {
        let pixel_len = (width * height * 4) as usize;
        Self {
            queue,
            width,
            height,
            vello: VelloImageRenderer::new(width, height),
            cpu_buffer: vec![0u8; pixel_len],
            padded_buffer: Vec::new(),
        }
    }

    /// Resolve document layout and paint into `target`.
    ///
    /// `document.resolve()` is assumed to have been called already. The
    /// optional `scrollbars` slice is painted as a post-pass on top of the
    /// document content. Select popups are also painted as a post-pass overlay.
    pub fn paint(
        &mut self,
        document: &mut BaseDocument,
        scrollbars: &[ScrollbarRegion],
        input_selections: &[InputSelection],
        input_carets: &[InputCaret],
        theme_override: Option<ScrollbarColors>,
        popups: &[crate::select::SelectPopupGeometry],
        target: &wgpu::Texture,
    ) {
        self.vello.render(
            |scene| {
                blitz_paint::paint_scene(scene, document, 1.0, self.width, self.height, 0, 0);
                paint_input_selections(scene, input_selections, 1.0);
                paint_input_carets(scene, input_carets, 1.0);
                crate::scrollbar::paint_scrollbars(
                    scene,
                    document,
                    scrollbars,
                    1.0,
                    theme_override,
                );
                paint_select_popups(scene, popups, 1.0);
            },
            &mut self.cpu_buffer,
        );

        if self.width == 0 || self.height == 0 {
            return;
        }

        let row_bytes = self.width.saturating_mul(4) as usize;
        let alignment = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as usize;
        let padded_row_bytes = if row_bytes == 0 {
            0
        } else {
            row_bytes.next_multiple_of(alignment)
        };

        let (upload_buffer, bytes_per_row) = if padded_row_bytes == row_bytes {
            (self.cpu_buffer.as_slice(), row_bytes)
        } else {
            let required_len = padded_row_bytes
                .checked_mul(self.height as usize)
                .expect("padded upload buffer too large");
            self.padded_buffer.resize(required_len, 0);
            self.padded_buffer.fill(0);
            for y in 0..self.height as usize {
                let src_start = y * row_bytes;
                let dst_start = y * padded_row_bytes;
                self.padded_buffer[dst_start..(dst_start + row_bytes)]
                    .copy_from_slice(&self.cpu_buffer[src_start..(src_start + row_bytes)]);
            }
            (self.padded_buffer.as_slice(), padded_row_bytes)
        };

        self.queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            upload_buffer,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row as u32),
                rows_per_image: Some(self.height),
            },
            wgpu::Extent3d {
                width: self.width,
                height: self.height,
                depth_or_array_layers: 1,
            },
        );
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.cpu_buffer.resize((width * height * 4) as usize, 0);
        self.padded_buffer.clear();
        self.vello.resize(width, height);
    }
}

fn paint_input_selections<S: PaintScene>(scene: &mut S, selections: &[InputSelection], scale: f64) {
    let color = peniko::Color::from_rgba8(180, 213, 255, 150);
    for selection in selections {
        let rect = Rect::new(
            selection.x as f64,
            selection.y as f64,
            (selection.x + selection.width) as f64,
            (selection.y + selection.height) as f64,
        );
        scene.fill(Fill::NonZero, Affine::scale(scale), color, None, &rect);
    }
}

fn paint_input_carets<S: PaintScene>(scene: &mut S, carets: &[InputCaret], scale: f64) {
    for caret in carets {
        let rect = Rect::new(
            caret.x as f64,
            caret.y as f64,
            (caret.x + caret.width) as f64,
            (caret.y + caret.height) as f64,
        );
        scene.fill(
            Fill::NonZero,
            Affine::scale(scale),
            caret.color,
            None,
            &rect,
        );
    }
}

fn paint_select_popups<S: PaintScene>(
    scene: &mut S,
    popups: &[crate::select::SelectPopupGeometry],
    scale: f64,
) {
    let bg_color = peniko::Color::from_rgba8(255, 255, 255, 255);
    let border_color = peniko::Color::from_rgba8(128, 128, 128, 255);
    let selected_bg = peniko::Color::from_rgba8(200, 220, 255, 255);
    let hover_bg = peniko::Color::from_rgba8(220, 235, 255, 255);
    let text_color = peniko::Color::from_rgba8(0, 0, 0, 255);
    let disabled_text = peniko::Color::from_rgba8(128, 128, 128, 255);

    for popup in popups {
        let option_height = 24.0;
        let vertical_padding = 4.0;
        let popup_height = popup.popup_height();

        // Draw popup background and border
        let popup_rect = Rect::new(
            popup.x as f64,
            popup.y as f64,
            (popup.x + popup.width) as f64,
            (popup.y + popup_height) as f64,
        );
        scene.fill(
            Fill::NonZero,
            Affine::scale(scale),
            bg_color,
            None,
            &popup_rect,
        );

        // Draw border
        let border_width = 1.0;
        let border_rect_top = Rect::new(
            popup.x as f64,
            popup.y as f64,
            (popup.x + popup.width) as f64,
            (popup.y + border_width) as f64,
        );
        scene.fill(
            Fill::NonZero,
            Affine::scale(scale),
            border_color,
            None,
            &border_rect_top,
        );

        // Draw options
        for (i, option) in popup.options.iter().enumerate() {
            let y = popup.y + vertical_padding + (i as f32 * option_height);
            let option_rect = Rect::new(
                popup.x as f64,
                y as f64,
                (popup.x + popup.width) as f64,
                (y + option_height) as f64,
            );

            // Background color based on state
            let bg = if Some(i) == popup.active_index.map(|x| x as usize) {
                hover_bg
            } else if Some(i) == popup.selected_index.map(|x| x as usize) {
                selected_bg
            } else {
                bg_color
            };

            scene.fill(
                Fill::NonZero,
                Affine::scale(scale),
                bg,
                None,
                &option_rect,
            );

            // TODO: Draw option text (requires glyph rendering)
        }
    }
}
