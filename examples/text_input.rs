use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[path = "common/args.rs"]
mod args;

use blitz_traits::shell::{ClipboardError, ShellProvider};
use serde_json::json;
use solite::{
    Instance, InstanceConfig, KeyboardEvent,
    capture::capture_texture_to_png,
    gpu::{BlitContext, BlitDraw, present_to_surface},
    winit::key_to_string,
};
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, KeyEvent, MouseButton as WinitMouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{ModifiersState, PhysicalKey};
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
import { render } from "solite-runtime";

function App() {
  return (
    <input
      class="field"
      type="text"
      placeholder="Type here..."
      value={globalThis.state.value || ""}
      onInput={(event) => {
        globalThis.state.value = event.value;
      }}
    />
  );
}

render(() => App(), __SOL_ROOT__);
"#;

struct App {
    window: Option<Arc<Window>>,
    instance: Option<Instance>,
    gpu: Option<Gpu>,
    last_mouse: (f32, f32),
    modifiers: ModifiersState,
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
            .with_title("solite: text input")
            .with_inner_size(winit::dpi::LogicalSize::new(320u32, 80u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let gpu = pollster::block_on(init_gpu(window.clone()));

        let (instance_width, instance_height) = (320, 80);

        let component = compile_text_input_component_source(TEXT_INPUT_COMPONENT);
        let (mut instance, _events) = Instance::new(
            InstanceConfig {
                width: instance_width,
                height: instance_height,
                device: gpu.device.clone(),
                queue: gpu.queue.clone(),
                stylesheets: vec![TEXT_INPUT_CSS.to_string()],
                document_scroll: false,
                base_url: None,
                initial_state: self
                    .capture_path
                    .as_ref()
                    .map(|_| json!({ "value": "hello world" })),
            },
            &component,
        )
        .expect("create instance");
        instance.set_shell_provider(Arc::new(SystemClipboard));
        let _ = instance.tick();

        // Drive a focus click + live keystrokes to exercise the reactive
        // text-update path end-to-end before the capture frame is taken.
        if self.capture_path.is_some() {
            // Force a layout pass so hit-testing can resolve the field.
            let _ = instance.render();
            let _ = instance.dispatch_mouse(
                20.0,
                20.0,
                solite::MouseEvent::Down {
                    x: 20.0,
                    y: 20.0,
                    button: solite::MouseButton::Left,
                },
            );
            let _ = instance.dispatch_mouse(
                20.0,
                20.0,
                solite::MouseEvent::Up {
                    x: 20.0,
                    y: 20.0,
                    button: solite::MouseButton::Left,
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
                        solite::MouseEvent::Move {
                            x: self.last_mouse.0,
                            y: self.last_mouse.1,
                        },
                    );
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self.last_mouse;
                let button = match button {
                    WinitMouseButton::Left => Some(solite::MouseButton::Left),
                    WinitMouseButton::Right => Some(solite::MouseButton::Right),
                    WinitMouseButton::Middle => Some(solite::MouseButton::Middle),
                    _ => None,
                };

                if let Some(button) = button {
                    if let Some(instance) = self.instance.as_mut() {
                        if state == ElementState::Pressed && button == solite::MouseButton::Left {
                            println!("text_input: mouse down at ({x:.1}, {y:.1})");
                        }
                        let event = match state {
                            ElementState::Pressed => solite::MouseEvent::Down { x, y, button },
                            ElementState::Released => solite::MouseEvent::Up { x, y, button },
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

            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
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
                        shift_key: self.modifiers.shift_key(),
                        ctrl_key: self.modifiers.control_key(),
                        alt_key: self.modifiers.alt_key(),
                        meta_key: self.modifiers.super_key(),
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
                        shift_key: self.modifiers.shift_key(),
                        ctrl_key: self.modifiers.control_key(),
                        alt_key: self.modifiers.alt_key(),
                        meta_key: self.modifiers.super_key(),
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
            label: Some("solite-text-input-device"),
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
        last_mouse: (8.0, 8.0),
        modifiers: ModifiersState::empty(),
        capture_path: args::capture_path_from_cli(),
        capture_done: false,
    };
    event_loop.run_app(&mut app).expect("run");
}

#[cfg(feature = "jsx-compiler")]
fn compile_text_input_component_source(component_source: &str) -> String {
    compile_component_source(std::path::Path::new("text_input.jsx"), component_source)
        .expect("JSX compile failed")
}

#[cfg(not(feature = "jsx-compiler"))]
fn compile_text_input_component_source(_component_source: &str) -> String {
    panic!("text_input example requires the `jsx-compiler` feature");
}
