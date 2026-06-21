//! Performance benchmark: animate many elements from a single shared state
//! mutation in a tight loop.
//!
//! This drives `globalThis.state.phase` from Rust each frame and lets the
//! Solid component derive every child's position/color from that one value.
//! The benchmark measures:
//! - `tick()` cost (state patch drain + reactivity)
//! - `render()` cost (layout + paint)
//! - total frame cost (tick + render)
//!
//! Usage:
//! `cargo run --example perf_animate_elements --release -- <elements> <frames> <width> <height> <warmup_frames>`
//! Defaults: `4000 600 1200 900 30`.

#[cfg(feature = "jsx-compiler")]
use std::path::Path;
use std::time::{Duration, Instant};

#[path = "common/headless.rs"]
mod headless;

use serde_json::json;
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use solite::{Instance, InstanceConfig};
#[cfg(feature = "gpu")]
use wgpu;

fn parse_arg(index: usize, default: usize) -> usize {
    let mut args = std::env::args().skip(1);
    let value = args.nth(index).and_then(|s| s.parse::<usize>().ok());
    value.unwrap_or(default)
}

fn bench_stats(label: &str, samples: &[Duration], measured_ms: f64) -> String {
    if samples.is_empty() {
        return format!("{label}: no samples");
    }
    let mut samples_us = samples.iter().map(Duration::as_micros).collect::<Vec<_>>();
    samples_us.sort_unstable();
    let sum: f64 = samples_us.iter().map(|v| *v as f64).sum();
    let min = samples_us[0] as f64 / 1000.0;
    let max = samples_us[samples_us.len() - 1] as f64 / 1000.0;
    let mean = sum / samples_us.len() as f64 / 1000.0;
    let median = samples_us[samples_us.len() / 2] as f64 / 1000.0;
    let p90_idx = ((samples_us.len() as f64) * 0.90).floor() as usize;
    let p90_idx = p90_idx.min(samples_us.len() - 1);
    let p99_idx = ((samples_us.len() as f64) * 0.99).floor() as usize;
    let p99_idx = p99_idx.min(samples_us.len() - 1);
    let p90 = samples_us[p90_idx] as f64 / 1000.0;
    let p99 = samples_us[p99_idx] as f64 / 1000.0;
    let fps = if measured_ms > 0.0 {
        samples.len() as f64 / (measured_ms / 1000.0)
    } else {
        0.0
    };

    format!(
        "{label}: min={min:.3}ms p50={median:.3}ms p90={p90:.3}ms p99={p99:.3}ms max={max:.3}ms mean={mean:.3}ms ({fps:.1} fps)"
    )
}

fn build_component(item_count: usize, width: usize, height: usize) -> String {
    let x_limit = (width.saturating_sub(12)).max(1);
    let y_limit = (height.saturating_sub(12)).max(1);
    let component = r#"
import { render } from "solite-runtime";

const ITEM_COUNT = __ITEM_COUNT__;
const ITEMS = Array.from({ length: ITEM_COUNT }, (_, i) => i);

function App() {
    const phase = Number(globalThis.state.phase || 0);
    return (
        <div style="position: relative; width: __WIDTH__px; height: __HEIGHT__px; overflow: hidden; background: #091021;">
            {
                ITEMS.map((i) => {
                    const x = (i * 19 + phase * 11) % __X_LIMIT__;
                    const y = (i * 31 + phase * 7) % __Y_LIMIT__;
                    const size = 6 + (i % 8);
                    const hue = (phase * 3 + i * 17) % 360;
                    return (
                        <div
                            style={
                                "position: absolute; left: " +
                                x +
                                "px; top: " +
                                y +
                                "px; width: " +
                                size +
                                "px; height: " +
                                size +
                                "px; background: hsl(" +
                                hue +
                                ", 85%, 62%);"
                            }
                            />
                    );
                })
            }
        </div>
    );
}

render(() => App(), __SOL_ROOT__);
"#;
    component
        .replace("__ITEM_COUNT__", &item_count.to_string())
        .replace("__WIDTH__", &width.to_string())
        .replace("__HEIGHT__", &height.to_string())
        .replace("__X_LIMIT__", &x_limit.to_string())
        .replace("__Y_LIMIT__", &y_limit.to_string())
}

fn run_benchmark(
    element_count: usize,
    width: usize,
    height: usize,
    frames: usize,
    warmup_frames: usize,
) -> std::io::Result<()> {
    let (device, queue) = pollster::block_on(headless::init_headless_device(
        "solite-perf-bench",
        wgpu::PowerPreference::HighPerformance,
    ));
    let component_source =
        compile_component_source_or_identity(&build_component(element_count, width, height))?;

    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: width as u32,
            height: height as u32,
            #[cfg(feature = "gpu")]
            device: device.clone(),
            #[cfg(feature = "gpu")]
            queue: queue.clone(),
            stylesheets: vec![],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
            scale_factor: 1.0,
        },
        &component_source,
    )
    .expect("create instance");
    let state = instance.state();

    let _ = instance.tick();
    let _ = instance.render();

    let mut tick_samples = Vec::with_capacity(frames);
    let mut render_samples = Vec::with_capacity(frames);
    let mut frame_samples = Vec::with_capacity(frames);

    let mut rendered = 0usize;

    for frame in 0..(frames + warmup_frames) {
        let phase = (frame as i32 % 360) as i32;
        state.set("phase", json!(phase));

        let frame_start = Instant::now();
        let tick_start = Instant::now();
        let _ = instance.tick();
        let tick_ms = tick_start.elapsed();

        let render_start = Instant::now();
        let _ = instance.render();
        let render_ms = render_start.elapsed();

        let frame_ms = frame_start.elapsed();
        if frame >= warmup_frames {
            tick_samples.push(tick_ms);
            render_samples.push(render_ms);
            frame_samples.push(frame_ms);
            rendered += 1;
        }
    }

    let measured_total_ms = frame_samples.iter().map(Duration::as_secs_f64).sum::<f64>() * 1000.0;
    let total = measured_total_ms;
    let avg_total_ms = if rendered > 0 {
        total / rendered as f64
    } else {
        0.0
    };

    println!(
        "benchmark: elements={element_count} viewport={width}x{height} frames={rendered} warmup={warmup_frames}"
    );
    println!("{}", bench_stats("tick", &tick_samples, total));
    println!("{}", bench_stats("render", &render_samples, total));
    println!("{}", bench_stats("frame", &frame_samples, total));
    println!("avg frame over measured samples: {avg_total_ms:.3}ms");

    // Print a cheap completion marker that downstream scripts can detect.
    let final_state = state.get("phase").unwrap_or(json!(0));
    println!("final phase = {final_state}");
    Ok(())
}

#[cfg(feature = "jsx-compiler")]
fn compile_component_source_or_identity(source: &str) -> std::io::Result<String> {
    compile_component_source(Path::new("perf_animate_elements.jsx"), source)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))
}

#[cfg(not(feature = "jsx-compiler"))]
fn compile_component_source_or_identity(_source: &str) -> std::io::Result<String> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "perf_animate_elements example requires the `jsx-compiler` feature",
    ))
}

fn main() {
    let element_count = parse_arg(0, 4_000).max(1);
    let frames = parse_arg(1, 600);
    let width = parse_arg(2, 1200).max(1);
    let height = parse_arg(3, 900).max(1);
    let warmup_frames = parse_arg(4, 30);

    if let Err(err) = run_benchmark(element_count, width, height, frames, warmup_frames) {
        eprintln!("benchmark failed: {err}");
        std::process::exit(1);
    }
}
