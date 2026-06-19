use std::fs::{create_dir_all, write};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[path = "common/args.rs"]
mod args;
#[path = "common/blit.rs"]
mod blit;
#[path = "common/capture.rs"]
mod capture;

use blit::{BlitContext, BlitDraw};
use blitz_traits::shell::{ClipboardError, ShellProvider};
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_file;
use solite::winit::{WinitBridge, WinitPollScheduler};
use solite::{
    Event, FileWatch, Instance, InstanceConfig, Scene, SourceChangeSummary, StylesheetId,
    SurfaceRect, TickResult,
};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

const PROJECT_NAME: &str = "solite-kitchen-sink";
const TARGET_LABELS: [&str; 3] = ["Left Target", "Center Target", "Right Target"];
// Kitchen-sink assets now live in a source directory so this example is
// editable as normal TSX/CSS without embedding large inline strings.
const KITCHEN_SINK_DIR: &str = "examples/kitchen_sink";
const KITCHEN_SINK_SOURCE: &str = "main.tsx";
const KITCHEN_SINK_STYLE: &str = "styles.css";

struct DemoProject {
    stylesheet_path: PathBuf,
    stylesheet: String,
    source_dir: PathBuf,
    source_file: PathBuf,
    dist_file: PathBuf,
}

struct RenderTargetData {
    label: String,
    rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
    stylesheet_id: Option<StylesheetId>,
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    project: Option<DemoProject>,
    watch: Option<FileWatch>,
    watch_poller: WinitPollScheduler,
    scene: Scene<RenderTargetData>,
    bridge: WinitBridge,
    capture_path: Option<PathBuf>,
    capture_done: bool,
}

struct SystemClipboard;

impl ShellProvider for SystemClipboard {
    fn get_clipboard_text(&self) -> Result<String, ClipboardError> {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.get_text())
            .map_err(|_| ClipboardError)
    }

    fn set_clipboard_text(&self, text: String) -> Result<(), ClipboardError> {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text))
            .map_err(|_| ClipboardError)
    }
}

struct Gpu {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    blit: BlitContext,
}

impl App {
    fn scale_factor(&self) -> f64 {
        self.window
            .as_ref()
            .map_or(1.0, |window| window.scale_factor())
    }

    fn scale_factor_safe(&self) -> f64 {
        let scale = self.scale_factor();
        if scale > 0.0 { scale } else { 1.0 }
    }

    fn to_logical_size(&self, width: u32, height: u32) -> (u32, u32) {
        let scale = self.scale_factor_safe();
        let logical_width = (width as f64 / scale).max(1.0).round() as u32;
        let logical_height = (height as f64 / scale).max(1.0).round() as u32;
        (logical_width, logical_height)
    }

    fn window_logical_size(&self) -> (u32, u32) {
        self.window.as_ref().map_or((640, 420), |window| {
            self.to_logical_size(window.inner_size().width, window.inner_size().height)
        })
    }

    fn target_layout(total_width: u32, target_count: usize) -> Vec<(u32, u32)> {
        let widths = Self::split_target_widths(total_width, target_count);
        let mut layouts = Vec::with_capacity(widths.len());
        let mut x: u32 = 0;
        for width in widths {
            layouts.push((x, width));
            x = x.saturating_add(width);
        }
        layouts
    }

    fn split_target_widths(total_width: u32, target_count: usize) -> Vec<u32> {
        let count = target_count.max(1);
        if total_width <= count as u32 {
            return vec![1; count];
        }

        let remaining = total_width - count as u32;
        let base = remaining / count as u32;
        let extra = (remaining % count as u32) as usize;
        let mut widths = vec![1 + base; count];
        for width in widths.iter_mut().take(extra) {
            *width = width.saturating_add(1);
        }

        widths
    }

    #[cfg(test)]
    fn is_relevant_source_change(path: &Path, source_dir: &Path) -> bool {
        if !path.starts_with(source_dir) {
            return false;
        }

        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| {
                matches!(
                    ext.to_ascii_lowercase().as_str(),
                    "jsx" | "tsx" | "ts" | "css"
                )
            })
    }

    fn maybe_rebuild(&mut self) -> bool {
        let Some(watch) = self.watch.as_mut() else {
            return false;
        };
        let Some(source_dir) = self.project.as_ref().map(|p| p.source_dir.clone()) else {
            return false;
        };

        let SourceChangeSummary {
            bundle_rebuild,
            css_reload,
        } = watch.poll_source_changes(&source_dir);

        if !bundle_rebuild && !css_reload {
            return false;
        }

        let window_size = if bundle_rebuild {
            Some(self.window_logical_size())
        } else {
            None
        };
        let Some(project) = self.project.as_mut() else {
            return false;
        };

        let refreshed_stylesheet = match std::fs::read_to_string(&project.stylesheet_path) {
            Ok(css) => {
                project.stylesheet = css;
                Some(project.stylesheet.clone())
            }
            Err(err) => {
                eprintln!(
                    "[{PROJECT_NAME}] failed to reload stylesheet: {}; using previous version",
                    err
                );
                Some(project.stylesheet.clone())
            }
        };

        if bundle_rebuild {
            let Some(gpu) = self.gpu.as_ref() else {
                return false;
            };

            if let Err(err) = build_bundle(project) {
                eprintln!("[{PROJECT_NAME}] rebuild failed: {err}");
                return false;
            }

            let (width, height) = window_size.unwrap_or((640, 420));
            let layouts = Self::target_layout(width, TARGET_LABELS.len());
            let stylesheets = vec![refreshed_stylesheet.unwrap_or_default()];
            let dist_file = project.dist_file.clone();

            match mount_targets(
                &dist_file,
                &stylesheets,
                &layouts,
                TARGET_LABELS.as_slice(),
                height,
                &gpu.device,
                &gpu.queue,
            ) {
                Ok(mut scene) => {
                    for surface in scene.surfaces_mut() {
                        let _ = surface.instance.tick();
                    }
                    self.scene = scene;
                }
                Err(err) => {
                    eprintln!("[{PROJECT_NAME}] failed to remount targets: {err}");
                }
            }
            return true;
        }

        if css_reload {
            let stylesheet = refreshed_stylesheet.unwrap_or_default();
            for surface in self.scene.surfaces_mut() {
                let stylesheet_id = surface
                    .instance
                    .upsert_stylesheet(surface.data.stylesheet_id, &stylesheet);
                surface.data.stylesheet_id = Some(stylesheet_id);
                let _ = surface.instance.tick();
            }
            return true;
        }

        false
    }

    fn drain_events(&mut self) {
        for surface in self.scene.surfaces_mut() {
            while let Ok(event) = surface.data.rx.try_recv() {
                println!(
                    "[{} {PROJECT_NAME}] {} {}",
                    surface.data.label, event.name, event.payload
                );
            }
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title(format!("{PROJECT_NAME} demo"))
            .with_inner_size(winit::dpi::LogicalSize::new(840u32, 440u32));

        let window = match Arc::new(event_loop.create_window(attrs).expect("window")) {
            window => window,
        };

        let gpu = pollster::block_on(init_gpu(window.clone()));

        let project = match create_demo_project() {
            Ok(project) => project,
            Err(err) => {
                eprintln!("[{PROJECT_NAME}] setup failed: {err}");
                return;
            }
        };

        if let Err(err) = build_bundle(&project) {
            eprintln!("[{PROJECT_NAME}] initial build failed: {err}");
            return;
        }

        let (width, height) = self.to_logical_size(gpu.config.width, gpu.config.height);
        let layouts = Self::target_layout(width, TARGET_LABELS.len());
        let stylesheets = vec![project.stylesheet.clone()];
        let mut scene = match mount_targets(
            &project.dist_file,
            &stylesheets,
            &layouts,
            TARGET_LABELS.as_slice(),
            height,
            &gpu.device,
            &gpu.queue,
        ) {
            Ok(scene) => scene,
            Err(err) => {
                eprintln!("[{PROJECT_NAME}] mount failed: {err}");
                return;
            }
        };
        for surface in scene.surfaces_mut() {
            let _ = surface.instance.tick();
        }

        let watch = match Instance::watch_files(&project.source_dir) {
            Ok(watch) => watch,
            Err(err) => {
                eprintln!("[{PROJECT_NAME}] failed to watch source: {err}");
                return;
            }
        };

        self.window = Some(window);
        self.gpu = Some(gpu);
        self.project = Some(project);
        self.watch = Some(watch);
        self.watch_poller.set_enabled(true);
        self.scene = scene;

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::Resized(size) => {
                let scale = self.scale_factor_safe();
                let width = size.width.max(1);
                let height = size.height.max(1);
                let logical_width = (width as f64 / scale).max(1.0).round() as u32;
                let logical_height = (height as f64 / scale).max(1.0).round() as u32;

                if let (Some(gpu), Some(window)) = (self.gpu.as_mut(), self.window.as_ref()) {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);

                    let layouts = Self::target_layout(logical_width, self.scene.len());
                    for (surface, (x, width)) in self
                        .scene
                        .surfaces_mut()
                        .iter_mut()
                        .zip(layouts.into_iter())
                    {
                        surface.rect =
                            SurfaceRect::new(x as f32, 0.0, width as f32, logical_height as f32);
                        surface.instance.resize(width, logical_height);
                    }

                    window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                if self.gpu.is_none() {
                    return;
                }

                let _ = self.maybe_rebuild();

                let capture_request = if self.capture_done {
                    None
                } else {
                    self.capture_path.take()
                };

                let mut result = TickResult::default();
                let mut draws = Vec::new();

                // The blit pass clears the whole surface every frame, so we
                // must re-blit every target each redraw — otherwise a target
                // whose instance had nothing new to paint (needs_paint=false)
                // would be wiped by the clear, leaving only the targets that
                // changed visible.
                let scale = self.scale_factor_safe();
                for surface in self.scene.surfaces_mut() {
                    let target_result = surface.instance.tick();
                    let draw_x = {
                        let value = (surface.rect.x as f64) * scale;
                        value.round().max(0.0) as u32
                    };
                    let draw_width = {
                        let start = (surface.rect.x as f64) * scale;
                        let end = ((surface.rect.x + surface.rect.width) as f64) * scale;
                        let width = end.round() as u32;
                        let start = start.round() as u32;
                        width.saturating_sub(start).max(1)
                    };
                    let draw_height = {
                        let value = (surface.instance.size().1 as f64) * scale;
                        value.round().max(0.0) as u32
                    };
                    let view = surface.instance.render().clone();
                    draws.push(BlitDraw {
                        view,
                        x: draw_x,
                        y: 0,
                        width: draw_width,
                        height: draw_height,
                    });
                    result = combine_tick_result(result, target_result);
                }

                if let Some(path) = capture_request {
                    if let Some(gpu) = &self.gpu {
                        let mut any_failed = false;
                        for surface in self.scene.surfaces() {
                            let label = surface.data.label.replace(' ', "_");
                            let destination = capture::build_capture_path(&path, Some(&label));
                            match capture::capture_texture_to_png(
                                &gpu.device,
                                &gpu.queue,
                                surface.instance.texture(),
                                &destination,
                            ) {
                                Ok(()) => {
                                    println!(
                                        "Captured target \"{}\" to {}",
                                        surface.data.label,
                                        destination.display()
                                    );
                                }
                                Err(err) => {
                                    eprintln!(
                                        "Failed to capture target \"{}\": {err}",
                                        surface.data.label
                                    );
                                    any_failed = true;
                                    break;
                                }
                            }
                        }
                        if any_failed {
                            self.capture_path = Some(path);
                        } else {
                            self.capture_done = true;
                        }
                    } else {
                        self.capture_path = Some(path);
                    }
                }

                if !draws.is_empty() {
                    if let Some(gpu) = &self.gpu {
                        let need_redraw = present_to_surface(
                            &gpu.device,
                            &gpu.queue,
                            &gpu.surface,
                            &gpu.config,
                            &gpu.blit,
                            &draws,
                        );
                        if need_redraw {
                            if let Some(window) = &self.window {
                                window.request_redraw();
                            }
                        }
                    }
                }

                if result.jobs_pending {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }

                self.drain_events();
                if self.capture_done {
                    event_loop.exit();
                    return;
                }
            }

            // Mouse / keyboard / modifier / wheel / close events are forwarded
            // through the bridge. CursorMoved positions arrive in physical
            // pixels; convert to logical so they line up with the surface
            // rects (which are tracked in logical coordinates).
            other => {
                let scale = self.scale_factor_safe();
                let translated = match other {
                    WindowEvent::CursorMoved {
                        device_id,
                        position,
                    } => WindowEvent::CursorMoved {
                        device_id,
                        position: PhysicalPosition::new(position.x / scale, position.y / scale),
                    },
                    other => other,
                };
                let r = self.bridge.handle(&mut self.scene, &translated);
                if r.close_requested {
                    event_loop.exit();
                    return;
                }
                if (r.needs_redraw || r.jobs_pending)
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }
        }
    }

    /// Called by winit when the loop is about to block waiting for events.
    /// Blink-enabled inputs need periodic wake-ups so the caret can repaint.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let mut next_blink: Option<std::time::Instant> = None;
        if self.watch_poller.should_poll()
            && self.maybe_rebuild()
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }

        for surface in self.scene.surfaces() {
            if let Some(deadline) = surface.instance.next_blink_deadline() {
                next_blink = match next_blink {
                    Some(existing) => Some(existing.min(deadline)),
                    None => Some(deadline),
                };
            }
        }

        self.watch_poller.set_next_wakeup(event_loop, next_blink);
    }

    /// When the timer fires (no actual window event), winit calls
    /// `new_events(StartCause::ResumeTimeReached)` — turn that into a redraw
    /// request so the cursor toggle path runs.
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if matches!(cause, winit::event::StartCause::ResumeTimeReached { .. })
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }
}

fn combine_tick_result(a: TickResult, b: TickResult) -> TickResult {
    TickResult {
        needs_paint: a.needs_paint || b.needs_paint,
        jobs_pending: a.jobs_pending || b.jobs_pending,
    }
}

fn present_to_surface(
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
    surface: &wgpu::Surface<'static>,
    config: &wgpu::SurfaceConfiguration,
    blit: &BlitContext,
    draws: &[BlitDraw],
) -> bool {
    blit::present_to_surface(device, queue, surface, config, blit, draws)
}

fn create_demo_project() -> io::Result<DemoProject> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.join(KITCHEN_SINK_DIR);
    if !source_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("missing example source directory: {}", source_dir.display()),
        ));
    }

    let source_file = source_dir.join(KITCHEN_SINK_SOURCE);
    if !source_file.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "missing kitchen sink source file: {}",
                source_file.display()
            ),
        ));
    }

    let stylesheet_path = source_dir.join(KITCHEN_SINK_STYLE);
    if !stylesheet_path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "missing kitchen sink stylesheet: {}",
                stylesheet_path.display()
            ),
        ));
    }
    let stylesheet = std::fs::read_to_string(&stylesheet_path)?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();

    let root = std::env::temp_dir().join(format!("{PROJECT_NAME}-{nanos}"));
    create_dir_all(root.join("dist"))?;

    Ok(DemoProject {
        stylesheet,
        stylesheet_path: stylesheet_path.clone(),
        source_dir,
        source_file,
        dist_file: root.join("dist/App.js"),
    })
}

fn build_bundle(project: &DemoProject) -> io::Result<()> {
    #[cfg(feature = "jsx-compiler")]
    {
        create_dir_all(&project.dist_file.parent().expect("dist parent"))?;
        let source_path = &project.source_file;
        let compiled = compile_component_file(&source_path).map_err(io::Error::other)?;
        write(&project.dist_file, compiled)?;
        Ok(())
    }
    #[cfg(not(feature = "jsx-compiler"))]
    {
        let _ = project;
        Err(io::Error::other(
            "kitchen_sink JSX bundling requires the `jsx-compiler` feature",
        ))
    }
}

fn mount_targets(
    compiled_path: &Path,
    stylesheets: &[String],
    layouts: &[(u32, u32)],
    labels: &[&'static str],
    height: u32,
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
) -> io::Result<Scene<RenderTargetData>> {
    let mut scene = Scene::new();

    let bundle_source = std::fs::read_to_string(compiled_path)?;

    for (index, &(x, width)) in layouts.iter().enumerate() {
        let label = labels.get(index).copied().unwrap_or("Target");

        // The JSX reads state via `globalThis.state.X` once during App()'s
        // first invocation, so seeding via state.set() after mount doesn't
        // reach the rendered DOM. Inject the per-instance state directly into
        // the module so the values are present before App() runs.
        let seed = format!(
            "globalThis.state.targetLabel = {label};\n\
             globalThis.state.targetIndex = {index};\n\
             globalThis.state.rows = 24;\n\
             globalThis.state.text = \"\";\n\
             globalThis.state.number = \"\";\n\
             globalThis.state.range = 50;\n\
             globalThis.state.checkboxChecked = false;\n\
             globalThis.state.radioA = false;\n\
             globalThis.state.radioB = false;\n\
             globalThis.state.password = \"\";\n",
            label = serde_json::to_string(label).unwrap(),
            index = index,
        );
        let seeded_source = bundle_source.replace(
            "render(() => App(), __SOL_ROOT__);",
            &format!("{seed}render(() => App(), __SOL_ROOT__);"),
        );

        let (instance, rx) = Instance::new(
            InstanceConfig {
                width,
                height,
                device: Arc::clone(device),
                queue: Arc::clone(queue),
                stylesheets: vec![],
                document_scroll: true,
                base_url: None,
            },
            &seeded_source,
        )
        .expect("create instance");
        let mut instance = instance;
        let stylesheet_id = stylesheets
            .first()
            .map(|stylesheet| instance.add_stylesheet(stylesheet));
        instance.set_shell_provider(Arc::new(SystemClipboard));

        scene.add_surface(
            instance,
            SurfaceRect::new(x as f32, 0.0, width as f32, height as f32),
            RenderTargetData {
                label: label.to_string(),
                rx,
                stylesheet_id,
            },
        );
    }

    Ok(scene)
}

async fn init_gpu(window: Arc<Window>) -> Gpu {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let surface = instance.create_surface(window.clone()).expect("surface");

    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::None,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        })
        .await
        .expect("adapter");

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("solite-kitchen-device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        })
        .await
        .expect("device");

    let size = window.inner_size();
    let caps = surface.get_capabilities(&adapter);
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| f.is_srgb())
        .unwrap_or(caps.formats[0]);
    let alpha_mode = caps
        .alpha_modes
        .iter()
        .copied()
        .find(|mode| *mode == wgpu::CompositeAlphaMode::Opaque)
        .or_else(|| caps.alpha_modes.first().copied())
        .unwrap_or(wgpu::CompositeAlphaMode::Opaque);

    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        width: size.width.max(1),
        height: size.height.max(1),
        present_mode: wgpu::PresentMode::AutoVsync,
        alpha_mode,
        view_formats: vec![],
        desired_maximum_frame_latency: 2,
    };

    surface.configure(&device, &config);
    let blit = BlitContext::new(&device, config.format);

    Gpu {
        device: Arc::new(device),
        queue: Arc::new(queue),
        surface,
        config,
        blit,
    }
}

fn main() {
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App {
        window: None,
        gpu: None,
        project: None,
        watch: None,
        watch_poller: WinitPollScheduler::with_default_interval(),
        scene: Scene::new(),
        bridge: WinitBridge::new(),
        capture_path: args::capture_path_from_cli(),
        capture_done: false,
    };
    event_loop.run_app(&mut app).expect("run");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_change_filter_matches_jsx() {
        let root = Path::new("/tmp/does-not-exist");
        assert!(App::is_relevant_source_change(
            &root.join("project/src/main.tsx"),
            &root.join("project/src"),
        ));
        assert!(App::is_relevant_source_change(
            &root.join("project/src/styles.css"),
            &root.join("project/src"),
        ));
        assert!(!App::is_relevant_source_change(
            &root.join("project/node_modules/pkg/index.js"),
            &root.join("project/src"),
        ));
    }

    #[test]
    fn split_target_widths_evenly_distributes_remainder() {
        let widths = App::split_target_widths(11, 3);
        assert_eq!(widths, vec![4, 4, 3]);
        assert_eq!(widths.iter().sum::<u32>(), 11);
    }
}
