// M1 example: 200×200 hello-world via Solid + Blitz.
// No JSX — the component uses bridge globals directly.

use std::path::PathBuf;
use std::sync::Arc;

#[path = "common/args.rs"]
mod args;
#[path = "common/blit.rs"]
mod blit;
#[path = "common/capture.rs"]
mod capture;

use blit::{BlitContext, BlitDraw};
use oxide_dom::{Instance, InstanceConfig};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

// Styling lives in CSS. The component just attaches a class name; the rule
// matches and Blitz computes the look. Click anywhere in the window to see
// the `:active` pseudo-class swap the background.
const HELLO_COMPONENT: &str = r#"
import { render } from "oxide-runtime";

function App() {
  const wrapper = __ox_createElement("div");
  __ox_setProperty(wrapper, "className", "hello");
  __ox_insertNode(wrapper, __ox_createTextNode("Hello from Solid"), null);
  return wrapper;
}

render(() => App(), __OX_ROOT__);
"#;

const HELLO_CSS: &str = r#"
.hello {
    width: 100%;
    height: 100%;
    background: #008000;
    color: #ffffff;
    padding: 12px;
    font-size: 24px;
    font-family: system-ui, sans-serif;
}
.hello:hover  { background: #006400; }
.hello:active { background: #003c00; }
"#;

struct App {
    window: Option<Arc<Window>>,
    instance: Option<Instance>,
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("oxide-dom")
            .with_inner_size(winit::dpi::LogicalSize::new(200u32, 200u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let gpu = pollster::block_on(init_gpu(window.clone()));

        let (instance, _events) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device: gpu.device.clone(),
                queue: gpu.queue.clone(),
                stylesheets: vec![HELLO_CSS.to_string()],
            },
            HELLO_COMPONENT,
        );

        self.window = Some(window);
        self.gpu = Some(gpu);
        self.instance = Some(instance);

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                let capture_path = self.capture_path.take();
                let (Some(instance), Some(gpu)) = (self.instance.as_mut(), self.gpu.as_ref())
                else {
                    if let Some(path) = capture_path {
                        self.capture_path = Some(path);
                    }
                    return;
                };

                let tick = instance.tick();
                if tick.needs_paint {
                    let view = instance.render().clone();
                    if let Some(path) = capture_path {
                        match capture::capture_texture_to_png(
                            &gpu.device,
                            &gpu.queue,
                            instance.texture(),
                            &path,
                        ) {
                            Ok(()) => {
                                println!("Captured frame to {}", path.display());
                                self.capture_done = true;
                            }
                            Err(err) => {
                                eprintln!("Failed to capture frame: {err}");
                                self.capture_path = Some(path);
                            }
                        }
                    }
                    let need_redraw = present_to_surface(
                        &gpu.device,
                        &gpu.queue,
                        &gpu.surface,
                        &gpu.config,
                        &gpu.blit,
                        &view,
                    );
                    if need_redraw {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
                if tick.jobs_pending {
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
                if self.capture_done {
                    event_loop.exit();
                }
            }
            WindowEvent::Resized(size) => {
                if let (Some(instance), Some(gpu)) = (self.instance.as_mut(), self.gpu.as_mut()) {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);
                    instance.resize(gpu.config.width, gpu.config.height);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            _ => {}
        }
    }
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
            label: Some("oxide-dom-device"),
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
        instance: None,
        gpu: None,
        capture_path: args::capture_path_from_cli(),
        capture_done: false,
    };
    event_loop.run_app(&mut app).expect("run");
}

fn present_to_surface(
    device: &Arc<wgpu::Device>,
    queue: &Arc<wgpu::Queue>,
    surface: &wgpu::Surface<'static>,
    config: &wgpu::SurfaceConfiguration,
    blit: &BlitContext,
    view: &wgpu::TextureView,
) -> bool {
    blit::present_to_surface(
        device,
        queue,
        surface,
        config,
        blit,
        &[BlitDraw {
            view: view.clone(),
            x: 0,
            y: 0,
            width: config.width,
            height: config.height,
        }],
    )
}
