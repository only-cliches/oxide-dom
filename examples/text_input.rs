use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[path = "common/gpu.rs"]
mod gpu;

use blitz_traits::shell::{ClipboardError, ShellProvider};
use serde_json::json;
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use solite::{
    Instance, InstanceConfig, KeyboardEvent,
    capture::{capture_path_from_cli, capture_texture_to_png},
    gpu::{BlitDraw, present_to_surface},
    winit::{WinitBridge, key_to_string},
};
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
    /// Held only for its HiDPI conversion helpers (`to_logical_pos` /
    /// `to_logical_size`); this example forwards events manually.
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

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("solite: text input")
            .with_inner_size(winit::dpi::LogicalSize::new(320u32, 80u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let gpu = pollster::block_on(gpu::init_gpu(window.clone(), "solite-text-input-device"));
        self.bridge.set_scale_factor(window.scale_factor());

        let (instance_width, instance_height) = (320, 80);

        let component = compile_text_input_component_source(TEXT_INPUT_COMPONENT);
        let (mut instance, _events) = Instance::new(
            InstanceConfig {
                width: instance_width,
                height: instance_height,
                #[cfg(feature = "gpu")]
                device: gpu.device.clone(),
                #[cfg(feature = "gpu")]
                queue: gpu.queue.clone(),
                stylesheets: vec![TEXT_INPUT_CSS.to_string()],
                document_scroll: false,
                base_url: None,
                initial_state: self
                    .capture_path
                    .as_ref()
                    .map(|_| json!({ "value": "hello world" })),
                registered_resources: vec![],
                scale_factor: 1.0,
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
                if let Some(window) = self.window.as_ref() {
                    self.bridge.set_scale_factor(window.scale_factor());
                }
                let (logical_width, logical_height) =
                    self.bridge.to_logical_size(size.width, size.height);

                if let (Some(instance), Some(gpu)) = (self.instance.as_mut(), self.gpu.as_mut()) {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);
                    instance.resize(logical_width, logical_height);
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                let (x, y) = self.bridge.to_logical_pos(position.x, position.y);
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

fn main() {
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App {
        window: None,
        instance: None,
        gpu: None,
        last_mouse: (8.0, 8.0),
        modifiers: ModifiersState::empty(),
        bridge: WinitBridge::new(),
        capture_path: capture_path_from_cli(),
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
