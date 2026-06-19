use image::{ImageBuffer, Rgba};
use std::path::Path;
use std::sync::mpsc;

const RGBA_BYTES_PER_PIXEL: u32 = 4;

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

    let image = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(width, height, rgba)
        .ok_or_else(|| "capture_texture_to_png: failed to build image".to_string())?;
    image
        .save(path)
        .map_err(|err| format!("capture_texture_to_png: save failed: {err}"))?;

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
