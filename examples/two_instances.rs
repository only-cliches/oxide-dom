// M4 example: two Instances sharing one wgpu Device + Queue.
//
// Each Instance owns its own BaseDocument, JsContext, and output texture, but
// they both render using the same host-provided wgpu device — exactly as
// described in the plan.  The window composites them side-by-side.
//
// Keyboard: press R to resize both instances to half their current width.

use std::path::PathBuf;
use std::sync::Arc;

#[path = "common/args.rs"]
mod args;

use solite::{
    Instance, InstanceConfig,
    capture::{build_capture_path, capture_texture_to_png},
    gpu::{BlitContext, BlitDraw, present_to_surface},
};
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowId};

const COMP_A: &str = r#"
import { render } from "solite-runtime";
function App() {
  return <div class="panel panel-blue">Instance A</div>;
}
render(() => App(), __SOL_ROOT__);
"#;

const COMP_B: &str = r#"
import { render } from "solite-runtime";
function App() {
  return <div class="panel panel-purple">Instance B</div>;
}
render(() => App(), __SOL_ROOT__);
"#;

// Shared stylesheet — same string handed to both instances so they pick up
// identical base styles plus a per-instance accent. `:hover` is pure CSS;
// no JS handler is needed to swap colours under the mouse.
const SHARED_CSS: &str = r#"
.panel {
    color: white;
    padding: 12px;
    font-size: 14px;
}
.panel-blue          { background: #1e3a5f; }
.panel-blue:hover    { background: #2a5388; }
.panel-purple        { background: #3a1e5f; }
.panel-purple:hover  { background: #532a86; }
"#;

struct TwoApp {
    window: Option<Arc<Window>>,
    a: Option<Instance>,
    b: Option<Instance>,
    gpu: Option<Gpu>,
    capture_path: Option<PathBuf>,
    capture_done: bool,
}

struct Gpu {
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    blit: BlitContext,
}

impl ApplicationHandler for TwoApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("solite: two instances")
            .with_inner_size(winit::dpi::LogicalSize::new(400u32, 200u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let gpu = pollster::block_on(init_gpu(window.clone()));

        // Both instances share the same Device + Queue via Arc::clone, and
        // both boot with the same CSS so the panels stay visually consistent.
        let component_a = compile_two_instances_component_source(COMP_A);
        let component_b = compile_two_instances_component_source(COMP_B);
        let (a, _rx_a) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device: gpu.device.clone(),
                queue: gpu.queue.clone(),
                stylesheets: vec![SHARED_CSS.to_string()],
                document_scroll: false,
                base_url: None,
                initial_state: None,
            registered_resources: vec![],
            },
            &component_a,
        )
        .expect("create first instance");
        let (b, _rx_b) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device: gpu.device.clone(),
                queue: gpu.queue.clone(),
                stylesheets: vec![SHARED_CSS.to_string()],
                document_scroll: false,
                base_url: None,
                initial_state: None,
            registered_resources: vec![],
            },
            &component_b,
        )
        .expect("create second instance");

        self.window = Some(window);
        self.gpu = Some(gpu);
        self.a = Some(a);
        self.b = Some(b);
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Space),
                        state: ElementState::Pressed,
                        ..
                    },
                ..
            } => {
                // Toggle resize: swap between 200×200 and 150×150 to demonstrate M4.
                if let (Some(a), Some(b)) = (self.a.as_mut(), self.b.as_mut()) {
                    let (w, _h) = a.size();
                    let new = if w == 200 { 150 } else { 200 };
                    a.resize(new, new);
                    b.resize(new, new);
                    println!("Resized both instances to {new}×{new}");
                }
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                let (Some(a), Some(b)) = (self.a.as_mut(), self.b.as_mut()) else {
                    return;
                };

                let tick_a = a.tick();
                let tick_b = b.tick();
                let mut draws = Vec::new();
                let window_width = self
                    .window
                    .as_ref()
                    .map_or(1, |window| window.inner_size().width);
                let half_width = window_width / 2;
                let viewport_height = if let Some(gpu) = self.gpu.as_ref() {
                    gpu.config.height
                } else {
                    1
                };
                let view_a = a.render().clone();
                let view_b = b.render().clone();

                draws.push(BlitDraw {
                    view: view_a,
                    x: 0,
                    y: 0,
                    width: half_width.max(1),
                    height: viewport_height,
                });
                draws.push(BlitDraw {
                    view: view_b,
                    x: half_width,
                    y: 0,
                    width: window_width.saturating_sub(half_width).max(1),
                    height: viewport_height,
                });

                if let Some(path) = self.capture_path.take().filter(|_| !self.capture_done) {
                    if let Some(gpu) = &self.gpu {
                        let mut failure: Option<String> = None;
                        for (label, instance) in [("instance-a", &*a), ("instance-b", &*b)] {
                            let destination = build_capture_path(&path, Some(label));
                            match capture_texture_to_png(
                                &gpu.device,
                                &gpu.queue,
                                instance.texture(),
                                &destination,
                            ) {
                                Ok(()) => {
                                    println!("Captured {label} to {}", destination.display());
                                }
                                Err(err) => {
                                    eprintln!("Failed to capture {label}: {err}");
                                    failure = Some(err);
                                    break;
                                }
                            }
                        }

                        if failure.is_some() {
                            self.capture_path = Some(path);
                        } else {
                            self.capture_done = true;
                        }
                    } else {
                        self.capture_path = Some(path);
                    }
                }

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

                if tick_a.jobs_pending || tick_b.jobs_pending {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }

                if self.capture_done {
                    event_loop.exit();
                }
            }

            WindowEvent::Resized(size) => {
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);
                }
                if let (Some(a), Some(b), Some(window)) =
                    (self.a.as_mut(), self.b.as_mut(), &self.window)
                {
                    let width = size.width.max(2);
                    let half_width = width / 2;
                    a.resize(half_width, size.height.max(1));
                    b.resize(width.saturating_sub(half_width), size.height.max(1));
                    window.request_redraw();
                }
            }
            _ => {}
        }
    }
}

#[cfg(feature = "jsx-compiler")]
fn compile_two_instances_component_source(component_source: &str) -> String {
    compile_component_source(std::path::Path::new("two_instances.jsx"), component_source)
        .expect("JSX compile failed")
}

#[cfg(not(feature = "jsx-compiler"))]
fn compile_two_instances_component_source(_component_source: &str) -> String {
    panic!("two_instances example requires the `jsx-compiler` feature");
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
            label: Some("solite-two"),
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
    let mut app = TwoApp {
        window: None,
        a: None,
        b: None,
        gpu: None,
        capture_path: args::capture_path_from_cli(),
        capture_done: false,
    };
    event_loop.run_app(&mut app).expect("run");
}
