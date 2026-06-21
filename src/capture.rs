//! Optional PNG capture helper for host-initiated screenshots.
//!
//! This module is intentionally small and focused on use-cases where a host needs
//! to snapshot a GPU texture to disk during headless/offscreen rendering or test
//! capture workflows.

use std::path::{Path, PathBuf};

const RGBA_BYTES_PER_PIXEL: u32 = 4;

/// Parse a `--capture <path>` / `--capture=<path>` CLI flag, with a fallback
/// to the `SOLITE_CAPTURE` environment variable.
#[allow(dead_code)]
pub fn capture_path_from_cli() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(rest) = arg.strip_prefix("--capture=") {
            return Some(PathBuf::from(rest));
        }
        if arg == "--capture" || arg == "-c" {
            return args.next().map(PathBuf::from);
        }
    }

    std::env::var_os("SOLITE_CAPTURE").map(PathBuf::from)
}

#[cfg(feature = "gpu")]
use std::sync::mpsc;

/// Copy `texture` into `path` as a PNG file.
#[cfg(feature = "gpu")]
pub fn capture_texture_to_png(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    path: &Path,
) -> Result<(), String> {
    let width = texture.width();
    let height = texture.height();
    if width == 0 || height == 0 {
        return Err("capture_texture_to_png: texture has zero dimensions".to_string());
    }

    if texture.format() != wgpu::TextureFormat::Rgba8Unorm {
        return Err(format!(
            "capture_texture_to_png: unsupported format {:?}",
            texture.format()
        ));
    }

    let unpadded_bytes_per_row = width
        .checked_mul(RGBA_BYTES_PER_PIXEL)
        .ok_or_else(|| "capture_texture_to_png: row too large".to_string())?;

    let bytes_per_row = unpadded_bytes_per_row.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let bytes_per_image = u64::from(bytes_per_row)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "capture_texture_to_png: image too large".to_string())?;

    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("solite capture staging"),
        size: bytes_per_image,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("solite capture encoder"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(bytes_per_row),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    queue.submit([encoder.finish()]);

    let slice = buffer.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });

    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|err| format!("capture_texture_to_png: poll failed: {err:?}"))?;
    match rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(format!("capture_texture_to_png: map failed: {err:?}")),
        Err(err) => return Err(format!("capture_texture_to_png: map wait failed: {err}")),
    }

    let mapped = slice.get_mapped_range();
    let mut rgba = Vec::with_capacity(
        (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(RGBA_BYTES_PER_PIXEL as usize))
            .ok_or_else(|| "capture_texture_to_png: image too large".to_string())?,
    );
    let row_stride = bytes_per_row as usize;
    let row_bytes = width.saturating_mul(RGBA_BYTES_PER_PIXEL) as usize;
    for y in 0..height as usize {
        let src_start = y * row_stride;
        let src_end = src_start + row_bytes;
        rgba.extend_from_slice(&mapped[src_start..src_end]);
    }

    drop(mapped);
    buffer.unmap();

    let image = image::ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)
        .ok_or_else(|| "capture_texture_to_png: failed to build image".to_string())?;
    image
        .save(path)
        .map_err(|err| format!("capture_texture_to_png: save failed: {err}"))?;

    Ok(())
}

/// Copy CPU pixel bytes (`Rgba8`) into `path` as a PNG file.
pub fn capture_buffer_to_png(
    width: u32,
    height: u32,
    image_rgba8: &[u8],
    path: &Path,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("capture_buffer_to_png: frame has zero dimensions".to_string());
    }

    let expected = width
        .checked_mul(height)
        .and_then(|px| px.checked_mul(RGBA_BYTES_PER_PIXEL))
        .ok_or_else(|| "capture_buffer_to_png: image too large".to_string())?;
    if image_rgba8.len() != expected as usize {
        return Err(format!(
            "capture_buffer_to_png: expected {expected} bytes, got {}",
            image_rgba8.len()
        ));
    }

    let image = image::ImageBuffer::<image::Rgba<u8>, Vec<u8>>::from_raw(
        width,
        height,
        image_rgba8.to_vec(),
    )
    .ok_or_else(|| "capture_buffer_to_png: failed to build image".to_string())?;
    image
        .save(path)
        .map_err(|err| format!("capture_buffer_to_png: save failed: {err}"))?;

    Ok(())
}

#[allow(dead_code)]
pub fn build_capture_path(base_path: &Path, suffix: Option<&str>) -> std::path::PathBuf {
    let parent = base_path.parent().unwrap_or_else(|| Path::new("."));
    let stem = base_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("capture");
    let suffix = suffix.unwrap_or("");
    let extension = base_path
        .extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .unwrap_or("png");

    if suffix.is_empty() {
        parent.join(format!("{stem}.{extension}"))
    } else {
        parent.join(format!("{stem}-{suffix}.{extension}"))
    }
}
