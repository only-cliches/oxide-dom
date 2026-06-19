// Demonstrates runtime custom-font registration.
//
// Loads the Mozilla "bullet" OTF — a tiny (~5 KB) font bundled with blitz —
// and registers it under the family name "SoliteBullet". The rendered text
// uses CSS `font-family: 'SoliteBullet'` so all glyphs render from the
// registered file. The font has very few code points (mostly bullets and a
// box character `\u{2610}` we use here), but that's enough to demonstrate
// the registration round-trip without depending on system fonts.

use std::path::PathBuf;
use std::sync::Arc;

#[path = "common/args.rs"]
mod args;

use solite::{
    FontFormat, Instance, InstanceConfig,
    capture::capture_texture_to_png,
    gpu::{BlitContext, BlitDraw, present_to_surface},
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

/// The Mozilla bullet font shipped with blitz. The path is fixed in this
/// repo so we can include the bytes at compile time.
const BULLET_FONT_BYTES: &[u8] =
    include_bytes!("../vendor/blitz/packages/blitz-dom/assets/moz-bullet-font.otf");

const COMPONENT: &str = r#"
import { render } from "solite-runtime";

function App() {
  return (
    <div class="root">
      <p class="label">Glyphs from a host-registered font:</p>
      <p class="custom">• ‣ ◦ ■ ☐</p>
    </div>
  );
}

render(() => App(), __SOL_ROOT__);
"#;

const CSS: &str = r#"
.root { padding: 16px; background: #fafafa; }
.label { font-family: system-ui, sans-serif; font-size: 14px; color: #333; }
.custom { font-family: 'SoliteBullet'; font-size: 48px; color: #222; letter-spacing: 8px; }
"#;

struct AppState {
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

impl ApplicationHandler for AppState {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("solite — custom font")
            .with_inner_size(winit::dpi::LogicalSize::new(480u32, 200u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        let gpu = pollster::block_on(init_gpu(window.clone()));

        let (mut instance, _events) = Instance::new(
            InstanceConfig {
                width: 480,
                height: 200,
                device: gpu.device.clone(),
                queue: gpu.queue.clone(),
                stylesheets: vec![CSS.to_string()],
                document_scroll: false,
                base_url: None,
                initial_state: None,
            },
            COMPONENT,
        )
        .expect("create instance");

        // Register the font AFTER the instance is mounted. The synthetic
        // @font-face rule + NetProvider fetch resolve synchronously, and
        // the next `resolve()` (called from `render()`) reflows any text
        // that should now match it.
        let _id = instance.register_font_bytes(
            "SoliteBullet",
            BULLET_FONT_BYTES.to_vec(),
            FontFormat::Opentype,
        );

        self.window = Some(window.clone());
        self.gpu = Some(gpu);
        self.instance = Some(instance);
        window.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                let (Some(instance), Some(gpu)) = (self.instance.as_mut(), self.gpu.as_ref())
                else {
                    return;
                };
                let tick = instance.tick();
                if tick.needs_paint {
                    let view = instance.render().clone();
                    let capture_path = self.capture_path.take();
                    if let Some(path) = capture_path {
                        match capture_texture_to_png(
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
                        &[BlitDraw {
                            view,
                            x: 0,
                            y: 0,
                            width: gpu.config.width,
                            height: gpu.config.height,
                        }],
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
            label: Some("solite-custom-font-device"),
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
        .find(|m| *m == wgpu::CompositeAlphaMode::Opaque)
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
    let mut app = AppState {
        window: None,
        instance: None,
        gpu: None,
        capture_path: args::capture_path_from_cli(),
        capture_done: false,
    };
    event_loop.run_app(&mut app).expect("run");
}
