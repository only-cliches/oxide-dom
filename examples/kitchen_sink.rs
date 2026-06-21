use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[path = "common/gpu.rs"]
mod gpu;

use blitz_traits::shell::{ClipboardError, ShellProvider};
use serde_json::json;
use solite::winit::{WinitBridge, WinitPollScheduler};
use solite::{
    Event, InstanceConfig, Scene, SurfaceRect, TickResult,
    capture::{build_capture_path, capture_path_from_cli, capture_texture_to_png},
    gpu::{BlitDraw, present_to_surface},
    workflow::{ReloadAction, SourceProject, SourceProjectWatch},
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

const PROJECT_NAME: &str = "solite-kitchen-sink";
const TARGET_LABELS: [&str; 3] = ["Left Target", "Center Target", "Right Target"];
const BIRDS_URL: &str = "solite-image://birds";
// Kitchen-sink assets now live in a source directory so this example is
// editable as normal TSX/CSS without embedding large inline strings.
const KITCHEN_SINK_DIR: &str = "examples/kitchen_sink";

struct DemoProject {
    source: SourceProject,
    birds_bytes: Vec<u8>,
}

struct RenderTargetData {
    label: String,
    rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    project: Option<DemoProject>,
    watch: Option<SourceProjectWatch>,
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

type Gpu = gpu::Gpu;

impl App {
    /// Current logical size of the window, via the bridge's scale factor (the
    /// physical→logical math lives in the library now).
    fn window_logical_size(&self) -> (u32, u32) {
        self.window.as_ref().map_or((640, 420), |window| {
            let size = window.inner_size();
            self.bridge.to_logical_size(size.width, size.height)
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
        let Some(watch) = self.watch.as_ref() else {
            return false;
        };

        match watch.poll() {
            ReloadAction::None => false,
            ReloadAction::Remount => {
                let Some(gpu) = self.gpu.as_ref() else {
                    return false;
                };
                let Some(project) = self.project.as_ref() else {
                    return false;
                };
                let (width, height) = self.window_logical_size();
                let layouts = Self::target_layout(width, TARGET_LABELS.len());
                let scale_factor = self.bridge.scale_factor();

                match mount_targets(
                    &project.source,
                    &layouts,
                    TARGET_LABELS.as_slice(),
                    height,
                    scale_factor,
                    &gpu.device,
                    &gpu.queue,
                    &project.birds_bytes,
                ) {
                    Ok(mut scene) => {
                        for surface in scene.surfaces_mut() {
                            let _ = surface.instance.tick();
                        }
                        self.scene = scene;
                        true
                    }
                    Err(err) => {
                        eprintln!("[{PROJECT_NAME}] failed to remount targets: {err}");
                        false
                    }
                }
            }
            ReloadAction::CssChanged(paths) => {
                let Some(project) = self.project.as_ref() else {
                    return false;
                };
                let mut changed = false;
                for surface in self.scene.surfaces_mut() {
                    if project
                        .source
                        .reload_imported_css(&mut surface.instance, &paths)
                    {
                        let _ = surface.instance.tick();
                        changed = true;
                    }
                }
                changed
            }
        }
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

        let gpu = pollster::block_on(gpu::init_gpu(window.clone(), "solite-kitchen-device"));

        let project = match create_demo_project() {
            Ok(project) => project,
            Err(err) => {
                eprintln!("[{PROJECT_NAME}] setup failed: {err}");
                return;
            }
        };

        self.bridge.set_scale_factor(window.scale_factor());
        let (width, height) = self
            .bridge
            .to_logical_size(gpu.config.width, gpu.config.height);
        let scale_factor = self.bridge.scale_factor();
        let layouts = Self::target_layout(width, TARGET_LABELS.len());
        let mut scene = match mount_targets(
            &project.source,
            &layouts,
            TARGET_LABELS.as_slice(),
            height,
            scale_factor,
            &gpu.device,
            &gpu.queue,
            &project.birds_bytes,
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

        let watch = match project.source.watch() {
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
                if let Some(window) = self.window.as_ref() {
                    self.bridge.set_scale_factor(window.scale_factor());
                }
                let (logical_width, logical_height) =
                    self.bridge.to_logical_size(size.width, size.height);

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
                let scale = self.bridge.scale_factor();
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
                            let destination = build_capture_path(&path, Some(&label));
                            match capture_texture_to_png(
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

            // Mouse / keyboard / modifier / wheel / touch / close events are
            // forwarded through the bridge, which converts physical pointer
            // positions to logical using its scale factor.
            other => {
                let r = self.bridge.handle(&mut self.scene, &other);
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

fn create_demo_project() -> io::Result<DemoProject> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source_dir = manifest_dir.join(KITCHEN_SINK_DIR);
    if !source_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("missing example source directory: {}", source_dir.display()),
        ));
    }

    let birds_bytes = std::fs::read(manifest_dir.join("examples/birds.jpg"))?;

    Ok(DemoProject {
        source: SourceProject::new(source_dir),
        birds_bytes,
    })
}

fn mount_targets(
    source: &SourceProject,
    layouts: &[(u32, u32)],
    labels: &[&'static str],
    height: u32,
    scale_factor: f64,
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
    birds_bytes: &[u8],
) -> io::Result<Scene<RenderTargetData>> {
    let mut scene = Scene::new();

    for (index, &(x, width)) in layouts.iter().enumerate() {
        let label = labels.get(index).copied().unwrap_or("Target");

        let (instance, rx) = source
            .mount_live(InstanceConfig {
                width,
                height,
                #[cfg(feature = "gpu")]
                device: Arc::clone(device),
                #[cfg(feature = "gpu")]
                queue: Arc::clone(queue),
                stylesheets: vec![],
                document_scroll: true,
                base_url: None,
                initial_state: Some(json!({
                    "targetLabel": label,
                    "targetIndex": index,
                    "rows": 24,
                    "text": "",
                    "number": 50,
                    "range": 50,
                    "checkboxChecked": false,
                    "radioA": false,
                    "radioB": false,
                    "password": "",
                })),
                registered_resources: vec![(BIRDS_URL.to_string(), birds_bytes.to_vec())],
                scale_factor,
            })
            .map_err(|err| io::Error::other(err.to_string()))?;
        instance.set_shell_provider(Arc::new(SystemClipboard));

        scene.add_surface(
            instance,
            SurfaceRect::new(x as f32, 0.0, width as f32, height as f32),
            RenderTargetData {
                label: label.to_string(),
                rx,
            },
        );
    }

    Ok(scene)
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
        capture_path: capture_path_from_cli(),
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
