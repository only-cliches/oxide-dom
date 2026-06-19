// Demonstrates `<img>` loading: static image, broken image with `onError`,
// and a dynamic `src` swap driven by a JS button.
//
// Test PNGs are synthesised in memory via the `image` crate (a dev-dep) and
// registered with `Instance::register_image_bytes` so they never touch the
// filesystem. Run with `cargo run --example images` to open a window.

use std::path::PathBuf;
use std::sync::Arc;

#[path = "common/args.rs"]
mod args;

use solite::{
    Instance, InstanceConfig,
    capture::capture_texture_to_png,
    gpu::{BlitContext, BlitDraw, present_to_surface},
};
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId};

// The component:
//   - a "valid" img registered in memory via register_image_bytes
//   - a "broken" img with onError that swaps a label to "load failed"
//   - a "dynamic" img whose src is toggled by a button between red and blue
//
// Images use the `solite-image://` scheme so they're served from the
// in-memory registry without touching the filesystem.
const VALID_URL: &str = "solite-image://valid";
const RED_URL: &str = "solite-image://red";
const BLUE_URL: &str = "solite-image://blue";
const BROKEN_URL: &str = "solite-image://broken"; // intentionally never registered

const COMPONENT: &str = r#"
import { render } from "solite-runtime";

function App() {
  globalThis.state = globalThis.state || {};
  const showRed = Boolean(globalThis.state.showRed);
  const labelText = globalThis.state.labelText || "waiting…";

  return (
    <div class="root">
      <img
        class="tile"
        src={globalThis.__OX_VALID_URL}
        onLoad={(ev) => {
          sendEvent("img:load", { which: "valid", target: ev.target });
        }}
      />

      <img
        class="tile broken"
        src={globalThis.__OX_BROKEN_URL}
        onError={(ev) => {
          sendEvent("img:error", { which: "broken", target: ev.target });
          globalThis.state.labelText = "load failed";
        }}
      />

      <div class="label">{labelText}</div>

      <img
        class="tile"
        src={showRed ? globalThis.__OX_RED_URL : globalThis.__OX_BLUE_URL}
        onLoad={(ev) => {
          sendEvent("img:load", { which: "dynamic", target: ev.target });
        }}
      />

      <button
        class="swap"
        onClick={() => {
          globalThis.state.showRed = !showRed;
        }}
      >
        Swap colour
      </button>
    </div>
  );
}

render(() => App(), __SOL_ROOT__);
"#;

const CSS: &str = r#"
.root { display: flex; flex-direction: row; align-items: center; gap: 16px; padding: 16px; }
.tile { width: 64px; height: 64px; border: 2px solid #444; background: #ddd; }
.tile.broken { border-color: #c33; }
.label { font-family: system-ui, sans-serif; font-size: 14px; color: #333; }
.swap { padding: 4px 10px; }
"#;

fn synth_png_bytes(rgba: [u8; 4]) -> Vec<u8> {
    use image::{ImageBuffer, Rgba};
    let img = ImageBuffer::from_fn(64, 64, |_, _| Rgba(rgba));
    let mut buf = std::io::Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png)
        .expect("encode PNG");
    buf.into_inner()
}

struct AppState {
    window: Option<Arc<Window>>,
    instance: Option<Instance>,
    /// Library-provided winit translation layer. Forwards every
    /// `WindowEvent` straight into the instance — keyboard, mouse,
    /// wheel, modifier tracking, cursor tracking are all handled.
    bridge: solite::winit::WinitBridge,
    events: Option<tokio::sync::mpsc::UnboundedReceiver<solite::Event>>,
    gpu: Option<Gpu>,
    capture_path: Option<PathBuf>,
    capture_done: bool,
    valid_png: Vec<u8>,
    red_png: Vec<u8>,
    blue_png: Vec<u8>,
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
            .with_title("solite — images")
            .with_inner_size(winit::dpi::LogicalSize::new(400u32, 200u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let gpu = pollster::block_on(init_gpu(window.clone()));

        // Bake the URL constants into JS globals before the component evals.
        let preamble = format!(
            "globalThis.__OX_VALID_URL = {VALID_URL:?};\n\
             globalThis.__OX_RED_URL = {RED_URL:?};\n\
             globalThis.__OX_BLUE_URL = {BLUE_URL:?};\n\
             globalThis.__OX_BROKEN_URL = {BROKEN_URL:?};\n"
        );
        let component_source = format!("{preamble}\n{COMPONENT}");
        let component = compile_image_component_source(&component_source);

        // Register in-memory PNGs before mount so the fetch triggered during
        // component construction finds them. The broken URL is intentionally
        // omitted so the img element triggers its onError handler.
        let (instance, events) = Instance::new(
            InstanceConfig {
                width: 400,
                height: 200,
                device: gpu.device.clone(),
                queue: gpu.queue.clone(),
                stylesheets: vec![CSS.to_string()],
                document_scroll: false,
                base_url: None,
                initial_state: None,
                registered_resources: vec![
                    (VALID_URL.to_string(), self.valid_png.clone()),
                    (RED_URL.to_string(), self.red_png.clone()),
                    (BLUE_URL.to_string(), self.blue_png.clone()),
                ],
            },
            &component,
        )
        .expect("create instance");

        self.window = Some(window.clone());
        self.gpu = Some(gpu);
        self.instance = Some(instance);
        self.events = Some(events);
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

                // Drain JS-emitted events so onLoad / onError outcomes
                // print to stdout.
                if let Some(rx) = self.events.as_mut() {
                    while let Ok(ev) = rx.try_recv() {
                        println!("[js event] {} {:?}", ev.name, ev.payload);
                    }
                }

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
            WindowEvent::MouseInput { state, button, .. } => {
                if let Some(instance) = self.instance.as_mut() {
                    use winit::event::ElementState;
                    let btn = match button {
                        winit::event::MouseButton::Left => solite::MouseButton::Left,
                        winit::event::MouseButton::Right => solite::MouseButton::Right,
                        _ => return,
                    };
                    let ev = match state {
                        ElementState::Pressed => solite::MouseEvent::Down {
                            x: 220.0,
                            y: 130.0,
                            button: btn,
                        },
                        ElementState::Released => solite::MouseEvent::Up {
                            x: 220.0,
                            y: 130.0,
                            button: btn,
                        },
                    };
                    instance.dispatch_mouse(220.0, 130.0, ev);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            other => {
                if let Some(instance) = self.instance.as_mut() {
                    let bridge_result = self.bridge.handle(instance, &other);
                    if bridge_result.needs_redraw || bridge_result.jobs_pending {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                    if bridge_result.close_requested {
                        event_loop.exit();
                    }
                }
            }
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
            label: Some("solite-images-device"),
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
        bridge: solite::winit::WinitBridge::new(),
        events: None,
        gpu: None,
        capture_path: args::capture_path_from_cli(),
        capture_done: false,
        valid_png: synth_png_bytes([0x33, 0xaa, 0x33, 0xff]),
        red_png: synth_png_bytes([0xcc, 0x33, 0x33, 0xff]),
        blue_png: synth_png_bytes([0x33, 0x66, 0xcc, 0xff]),
    };
    event_loop.run_app(&mut app).expect("run");
}

#[cfg(feature = "jsx-compiler")]
fn compile_image_component_source(component_source: &str) -> String {
    compile_component_source(std::path::Path::new("App.jsx"), component_source)
        .expect("JSX compile failed")
}

#[cfg(not(feature = "jsx-compiler"))]
fn compile_image_component_source(_component_source: &str) -> String {
    panic!("images example requires the `jsx-compiler` feature");
}
