use anyrender::ImageRenderer;
use anyrender::PaintScene;
use anyrender_vello::VelloImageRenderer;
use blitz_dom::BaseDocument;
use kurbo::{Affine, Rect};
use peniko::Fill;
#[cfg(feature = "gpu")]
use std::sync::Arc;
#[cfg(feature = "gpu")]
use wgpu;

use crate::scrollbar::{ScrollbarColors, ScrollbarRegion};
use crate::spinner::NumberSpinner;

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

/// Renderer: blitz-dom layout + paint → CPU RGBA8 buffer, optionally uploaded to
// a GPU texture when that output is enabled.
pub(crate) struct Painter {
    #[cfg(feature = "gpu")]
    queue: Arc<wgpu::Queue>,
    width: u32,
    height: u32,
    vello: VelloImageRenderer,
    pub(crate) cpu_buffer: Vec<u8>,
    padded_buffer: Vec<u8>,
}

impl Painter {
    #[cfg(feature = "gpu")]
    pub fn new(
        _device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
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

    #[cfg(not(feature = "gpu"))]
    pub fn new(width: u32, height: u32) -> Self {
        let pixel_len = (width * height * 4) as usize;
        Self {
            width,
            height,
            vello: VelloImageRenderer::new(width, height),
            cpu_buffer: vec![0u8; pixel_len],
            padded_buffer: Vec::new(),
        }
    }

    #[cfg(not(feature = "gpu"))]
    pub(crate) fn pixel_bytes(&self) -> &[u8] {
        &self.cpu_buffer
    }

    /// Resolve document layout and paint into `target` (or CPU memory when no
    /// GPU backend is enabled).
    #[cfg(feature = "gpu")]
    pub fn paint(
        &mut self,
        document: &mut BaseDocument,
        scrollbars: &[ScrollbarRegion],
        input_selections: &[InputSelection],
        input_carets: &[InputCaret],
        number_spinners: &[NumberSpinner],
        theme_override: Option<ScrollbarColors>,
        scale: f64,
        target: &wgpu::Texture,
    ) {
        self.cpu_buffer.fill(0);

        self.vello.render(
            |scene| {
                blitz_paint::paint_scene(scene, document, scale, self.width, self.height, 0, 0);
                paint_input_selections(scene, input_selections, scale);
                paint_input_carets(scene, input_carets, scale);
                crate::scrollbar::paint_scrollbars(
                    scene,
                    document,
                    scrollbars,
                    scale,
                    theme_override,
                );
                crate::spinner::paint_number_spinners(scene, number_spinners, scale);
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
            let Some(required_len) = padded_row_bytes.checked_mul(self.height as usize) else {
                return;
            };
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

    #[cfg(not(feature = "gpu"))]
    pub fn paint(
        &mut self,
        document: &mut BaseDocument,
        scrollbars: &[ScrollbarRegion],
        input_selections: &[InputSelection],
        input_carets: &[InputCaret],
        number_spinners: &[NumberSpinner],
        theme_override: Option<ScrollbarColors>,
        scale: f64,
    ) {
        self.cpu_buffer.fill(0);

        self.vello.render(
            |scene| {
                blitz_paint::paint_scene(scene, document, scale, self.width, self.height, 0, 0);
                paint_input_selections(scene, input_selections, scale);
                paint_input_carets(scene, input_carets, scale);
                crate::scrollbar::paint_scrollbars(
                    scene,
                    document,
                    scrollbars,
                    scale,
                    theme_override,
                );
                crate::spinner::paint_number_spinners(scene, number_spinners, scale);
            },
            &mut self.cpu_buffer,
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
