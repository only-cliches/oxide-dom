use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[path = "common/args.rs"]
mod args;
#[path = "common/blit.rs"]
mod blit;
#[path = "common/capture.rs"]
mod capture;

use blit::{BlitContext, BlitDraw};
use oxide_dom::{Instance, InstanceConfig, KeyboardEvent};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton as WinitMouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

const TEXT_INPUT_CSS: &str = r#"
.field {
    display: block;
    width: 280px;
    height: 42px;
    padding: 8px;
    background: #2b2d3a;
    color: #f8f8ff;
    font-family: monospace;
    font-size: 24px;
    outline: 1px solid transparent;
}
.field:hover  { background: #34374a; }
.field:focus  { outline: 2px solid #80b0ff; background: #34374a; }
"#;

const TEXT_INPUT_COMPONENT: &str = r#"
import { render } from "oxide-runtime";

function App() {
  const field = __ox_createElement("input");
  __ox_setProperty(field, "className", "field");
  __ox_setProperty(field, "type", "text");
  __ox_setProperty(field, "placeholder", "Type here...");
  __ox_setProperty(field, "value", globalThis.state.value || "");
  __ox_setProperty(field, "onInput", (event) => {
    globalThis.state.value = event.value;
  });

  return field;
}

render(() => App(), __OX_ROOT__);
"#;

struct App {
    window: Option<Arc<Window>>,
    instance: Option<Instance>,
    gpu: Option<Gpu>,
    last_mouse: (f32, f32),
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

impl App {
    fn to_logical_pos(&self, x: f64, y: f64) -> (f32, f32) {
        let scale = self
            .window
            .as_ref()
            .map_or(1.0, |window| window.scale_factor());
        let scale = if scale > 0.0 { scale } else { 1.0 };
        ((x / scale) as f32, (y / scale) as f32)
    }

    fn to_logical_size(&self, width: u32, height: u32) -> (u32, u32) {
        let scale = self
            .window
            .as_ref()
            .map_or(1.0, |window| window.scale_factor());
        let scale = if scale > 0.0 { scale } else { 1.0 };
        (
            (width as f64 / scale).max(1.0).round() as u32,
            (height as f64 / scale).max(1.0).round() as u32,
        )
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("oxide-dom: text input")
            .with_inner_size(winit::dpi::LogicalSize::new(320u32, 80u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let gpu = pollster::block_on(init_gpu(window.clone()));

        let (instance_width, instance_height) = (320, 80);

        // When capturing, seed initial state before App() runs so the PNG
        // shows real text rather than an empty field.
        let component_source = if self.capture_path.is_some() {
            TEXT_INPUT_COMPONENT.replace(
                "render(() => App(), __OX_ROOT__);",
                r#"
globalThis.state.value = "hello world";
render(() => App(), __OX_ROOT__);
"#,
            )
        } else {
            TEXT_INPUT_COMPONENT.to_string()
        };

        let (mut instance, _events) = Instance::new(
            InstanceConfig {
                width: instance_width,
                height: instance_height,
                device: gpu.device.clone(),
                queue: gpu.queue.clone(),
                stylesheets: vec![TEXT_INPUT_CSS.to_string()],
            },
            &component_source,
        );
        let _ = instance.tick();

        // Drive a focus click + live keystrokes to exercise the reactive
        // text-update path end-to-end before the capture frame is taken.
        if self.capture_path.is_some() {
            // Force a layout pass so hit-testing can resolve the field.
            let _ = instance.render();
            let _ = instance.dispatch_mouse(
                20.0,
                20.0,
                oxide_dom::MouseEvent::Down {
                    x: 20.0,
                    y: 20.0,
                    button: oxide_dom::MouseButton::Left,
                },
            );
            let _ = instance.dispatch_mouse(
                20.0,
                20.0,
                oxide_dom::MouseEvent::Up {
                    x: 20.0,
                    y: 20.0,
                    button: oxide_dom::MouseButton::Left,
                },
            );
            for ch in "!".chars() {
                let _ = instance.dispatch_key_down(KeyboardEvent {
                    key: ch.to_string(),
                    code: String::new(),
                    key_code: 0,
                    repeat: false,
                    shift_key: false,
                    ctrl_key: false,
                    alt_key: false,
                    meta_key: false,
                });
            }
            let _ = instance.tick();
        }

        self.window = Some(window);
        self.gpu = Some(gpu);
        self.instance = Some(instance);
        self.last_mouse = (8.0, 8.0);

        if let Some(window) = &self.window {
            window.request_redraw();
        }
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
                    if let Some(path) = self.capture_path.take().filter(|_| !self.capture_done) {
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
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
                if self.capture_done {
                    event_loop.exit();
                }
            }

            WindowEvent::Resized(size) => {
                let scale = self
                    .window
                    .as_ref()
                    .map_or(1.0, |window| window.scale_factor())
                    .max(1.0);
                let width = size.width.max(1);
                let height = size.height.max(1);
                let logical_width = (width as f64 / scale).max(1.0).round() as u32;
                let logical_height = (height as f64 / scale).max(1.0).round() as u32;

                if let (Some(instance), Some(gpu)) = (self.instance.as_mut(), self.gpu.as_mut()) {
                    gpu.config.width = width;
                    gpu.config.height = height;
                    gpu.surface.configure(&gpu.device, &gpu.config);
                    instance.resize(logical_width, logical_height);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = self.to_logical_pos(position.x, position.y);
                self.last_mouse = (x, y);

                if let Some(instance) = self.instance.as_mut() {
                    let _ = instance.dispatch_mouse(
                        self.last_mouse.0,
                        self.last_mouse.1,
                        oxide_dom::MouseEvent::Move {
                            x: self.last_mouse.0,
                            y: self.last_mouse.1,
                        },
                    );
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self.last_mouse;
                let button = match button {
                    WinitMouseButton::Left => Some(oxide_dom::MouseButton::Left),
                    WinitMouseButton::Right => Some(oxide_dom::MouseButton::Right),
                    WinitMouseButton::Middle => Some(oxide_dom::MouseButton::Middle),
                    _ => None,
                };

                if let Some(button) = button {
                    if let Some(instance) = self.instance.as_mut() {
                        if state == ElementState::Pressed && button == oxide_dom::MouseButton::Left
                        {
                            println!("text_input: mouse down at ({x:.1}, {y:.1})");
                        }
                        let event = match state {
                            ElementState::Pressed => oxide_dom::MouseEvent::Down { x, y, button },
                            ElementState::Released => oxide_dom::MouseEvent::Up { x, y, button },
                        };
                        let tick = instance.dispatch_mouse(x, y, event);
                        if tick.needs_paint {
                            if let Some(window) = &self.window {
                                window.request_redraw();
                            }
                        }
                    }
                }
            }

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        state: ElementState::Pressed,
                        logical_key,
                        physical_key,
                        text,
                        repeat,
                        ..
                    },
                ..
            } => {
                if let Some(instance) = self.instance.as_mut() {
                    let key = key_to_string(&logical_key, text.as_deref());
                    let code = match physical_key {
                        PhysicalKey::Code(code) => format!("{:?}", code),
                        _ => String::new(),
                    };
                    let event = KeyboardEvent {
                        key,
                        code,
                        key_code: 0,
                        repeat,
                        shift_key: false,
                        ctrl_key: false,
                        alt_key: false,
                        meta_key: false,
                    };
                    let event_key = event.key.clone();
                    let result = instance.dispatch_key_down(event);
                    println!("text_input: keyboard down key={event_key} repeat={repeat}");
                    if result.needs_paint || result.jobs_pending {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }

            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        state: ElementState::Released,
                        logical_key,
                        physical_key,
                        text,
                        repeat,
                        ..
                    },
                ..
            } => {
                if let Some(instance) = self.instance.as_mut() {
                    let key = key_to_string(&logical_key, text.as_deref());
                    let code = match physical_key {
                        PhysicalKey::Code(code) => format!("{:?}", code),
                        _ => String::new(),
                    };
                    let event = KeyboardEvent {
                        key,
                        code,
                        key_code: 0,
                        repeat,
                        shift_key: false,
                        ctrl_key: false,
                        alt_key: false,
                        meta_key: false,
                    };
                    let event_key = event.key.clone();
                    let result = instance.dispatch_key_up(event);
                    println!("text_input: keyboard up key={event_key} repeat={repeat}");
                    if result.needs_paint || result.jobs_pending {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Drive the cursor-blink timer: while the field is focused, schedule the
    /// next wake-up 500ms after the last toggle so the redraw runs even when
    /// the user is idle.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(instance) = self.instance.as_ref() else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };
        let Some(next_blink) = instance.next_blink_deadline() else {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        };

        let now = Instant::now();
        if next_blink <= now {
            event_loop.set_control_flow(ControlFlow::WaitUntil(now));
        } else {
            event_loop.set_control_flow(ControlFlow::WaitUntil(next_blink));
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if matches!(cause, winit::event::StartCause::ResumeTimeReached { .. })
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }
}

fn key_to_string(logical_key: &Key, text: Option<&str>) -> String {
    if let Some(text) = text.filter(|text| !text.is_empty()) {
        if text != "\u{8}" {
            return text.to_string();
        }
    }
    if let Key::Named(named) = logical_key {
        if let NamedKey::Space = named {
            return " ".to_string();
        }
        return format!("{named:?}");
    }

    match logical_key {
        Key::Character(text) => text.to_string(),
        Key::Named(named) => format!("{named:?}"),
        Key::Unidentified(_) => "Unidentified".to_string(),
        Key::Dead(Some(c)) => c.to_string(),
        Key::Dead(None) => String::new(),
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
            label: Some("oxide-dom-text-input-device"),
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

fn main() {
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App {
        window: None,
        instance: None,
        gpu: None,
        last_mouse: (8.0, 8.0),
        capture_path: args::capture_path_from_cli(),
        capture_done: false,
    };
    event_loop.run_app(&mut app).expect("run");
}
