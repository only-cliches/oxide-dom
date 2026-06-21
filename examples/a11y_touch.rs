//! Reference integration for touchscreen controls + accessibility.
//!
//! A single [`Instance`] driven through [`WinitBridge`] (so touch events get
//! the full tap / pan / flick-momentum gesture handling in
//! `Instance::dispatch_touch`) with a [`A11yAdapter`] wired to
//! `accesskit_winit` so screen readers see a live, enriched tree and can drive
//! the controls.
//!
//! Run it and try:
//! - **Touch / trackpad**: tap the button/checkbox to activate, drag the
//!   slider, flick the long list and watch momentum coast.
//! - **Screen reader** (VoiceOver / NVDA / Orca): focus moves announce roles
//!   and labels; AT "increment"/"click" actions drive the controls.
//!
//! `--capture <path>` renders one frame to a PNG and exits (for smoke tests).

use std::path::PathBuf;
use std::sync::Arc;

#[path = "common/gpu.rs"]
mod gpu;

use serde_json::json;
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use solite::winit::{A11yAdapter, WinitBridge, accesskit_winit};
use solite::{
    Instance, InstanceConfig,
    capture::{capture_path_from_cli, capture_texture_to_png},
    gpu::{BlitContext, BlitDraw, present_to_surface},
};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const CSS: &str = r#"
body { font-family: sans-serif; color: #e8e8f0; background: #20222e; }
.bar { display: flex; gap: 12px; align-items: center; padding: 12px; }
button {
  font-size: 18px; padding: 10px 16px; border-radius: 8px;
  background: #3b5bdb; color: white; border: none;
}
input[type=text] {
  font-size: 18px; padding: 8px; width: 180px;
  background: #2b2d3a; color: #f8f8ff; outline: 1px solid #444;
}
.row { padding: 10px 14px; border-bottom: 1px solid #2e3140; font-size: 16px; }
"#;

// Built with the raw element API rather than JSX attributes for `aria-*` so the
// example needs no special compiler support for hyphenated props.
const COMPONENT: &str = r#"
import { render } from "solite-runtime";

function Row(i) {
  const p = __sol_createElement("p");
  __sol_setProperty(p, "class", "row");
  __sol_insertNode(p, __sol_createTextNode("Scrollable row " + i + " — flick me"), null);
  return p;
}

function App() {
  const root = __sol_createElement("div");

  const bar = __sol_createElement("div");
  __sol_setProperty(bar, "class", "bar");

  const button = __sol_createElement("button");
  __sol_setProperty(button, "aria-label", "Increment the counter");
  const label = __sol_createTextNode("Count: 0");
  __sol_insertNode(button, label, null);
  __sol_setProperty(button, "onClick", () => {
    const n = (globalThis.state.count || 0) + 1;
    globalThis.state.count = n;
    __sol_setText(label, "Count: " + n);
  });

  const check = __sol_createElement("input");
  __sol_setProperty(check, "type", "checkbox");
  __sol_setProperty(check, "aria-label", "Enable feature");

  const slider = __sol_createElement("input");
  __sol_setProperty(slider, "type", "range");
  __sol_setProperty(slider, "min", "0");
  __sol_setProperty(slider, "max", "100");
  __sol_setProperty(slider, "value", "50");
  __sol_setProperty(slider, "aria-label", "Volume");

  const name = __sol_createElement("input");
  __sol_setProperty(name, "type", "text");
  __sol_setProperty(name, "placeholder", "name");
  __sol_setProperty(name, "aria-label", "Your name");

  __sol_insertNode(bar, button, null);
  __sol_insertNode(bar, check, null);
  __sol_insertNode(bar, slider, null);
  __sol_insertNode(bar, name, null);
  __sol_insertNode(root, bar, null);

  for (let i = 1; i <= 40; i++) {
    __sol_insertNode(root, Row(i), null);
  }
  return root;
}

render(() => App(), __SOL_ROOT__);
"#;

type Gpu = gpu::Gpu;

struct App {
    window: Option<Arc<Window>>,
    instance: Option<Instance>,
    gpu: Option<Gpu>,
    bridge: WinitBridge,
    a11y: Option<A11yAdapter>,
    proxy: winit::event_loop::EventLoopProxy<accesskit_winit::Event>,
    capture_path: Option<PathBuf>,
    capture_done: bool,
}

impl App {
    fn request_redraw(&self) {
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }
}

impl ApplicationHandler<accesskit_winit::Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        // The AccessKit adapter must be created while the window is still
        // invisible, then the window is shown.
        let attrs = Window::default_attributes()
            .with_title("solite: touch + accessibility")
            .with_inner_size(winit::dpi::LogicalSize::new(360u32, 520u32))
            .with_visible(false);
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let gpu = pollster::block_on(gpu::init_gpu(window.clone(), "solite-a11y-touch"));
        // The bridge owns the scale factor; the physical→logical math lives in
        // the library.
        self.bridge.set_scale_factor(window.scale_factor());
        let (w, h) = self
            .bridge
            .to_logical_size(gpu.config.width, gpu.config.height);

        let component = compile(COMPONENT);
        let (mut instance, _events) = Instance::new(
            InstanceConfig {
                width: w,
                height: h,
                #[cfg(feature = "gpu")]
                device: gpu.device.clone(),
                #[cfg(feature = "gpu")]
                queue: gpu.queue.clone(),
                stylesheets: vec![CSS.to_string()],
                document_scroll: true,
                base_url: None,
                initial_state: Some(json!({ "count": 0 })),
                registered_resources: vec![],
                scale_factor: self.bridge.scale_factor(),
            },
            &component,
        )
        .expect("create instance");
        let _ = instance.tick();

        let a11y = A11yAdapter::new(event_loop, &window, self.proxy.clone());
        window.set_visible(true);

        self.window = Some(window);
        self.gpu = Some(gpu);
        self.instance = Some(instance);
        self.a11y = Some(a11y);
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Feed every window event to the AccessKit adapter first.
        if let (Some(a11y), Some(window)) = (self.a11y.as_mut(), self.window.as_ref()) {
            a11y.process_event(window, &event);
        }

        match event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
            }

            WindowEvent::RedrawRequested => {
                let (Some(instance), Some(gpu)) = (self.instance.as_mut(), self.gpu.as_ref())
                else {
                    return;
                };
                let tick = instance.tick();
                let view = instance.render().clone();

                if let Some(path) = self.capture_path.take().filter(|_| !self.capture_done) {
                    match capture_texture_to_png(&gpu.device, &gpu.queue, instance.texture(), &path)
                    {
                        Ok(()) => {
                            println!("Captured frame to {}", path.display());
                            self.capture_done = true;
                        }
                        Err(err) => {
                            eprintln!("capture failed: {err}");
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

                // Publish the (possibly changed) accessibility tree.
                if let Some(a11y) = self.a11y.as_mut() {
                    if let Some(instance) = self.instance.as_ref() {
                        a11y.update(instance);
                    }
                }

                if need_redraw || tick.jobs_pending {
                    self.request_redraw();
                }
                if self.capture_done {
                    event_loop.exit();
                }
            }

            WindowEvent::Resized(size) => {
                if let Some(window) = self.window.as_ref() {
                    self.bridge.set_scale_factor(window.scale_factor());
                }
                let (logical_w, logical_h) = self.bridge.to_logical_size(size.width, size.height);
                if let (Some(instance), Some(gpu)) = (self.instance.as_mut(), self.gpu.as_mut()) {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);
                    instance.resize(logical_w, logical_h);
                }
                self.request_redraw();
            }

            // Forward pointer/keyboard/touch through the bridge, converting
            // the logical space the instance uses (the bridge does the
            // physical→logical conversion via its scale factor).
            other => {
                let Some(instance) = self.instance.as_mut() else {
                    return;
                };
                let r = self.bridge.handle(instance, &other);
                if r.close_requested {
                    event_loop.exit();
                    return;
                }
                if r.needs_redraw || r.jobs_pending {
                    self.request_redraw();
                }
            }
        }
    }

    /// Route AccessKit action requests (from a screen reader) into the instance.
    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: accesskit_winit::Event) {
        if let (Some(a11y), Some(instance)) = (self.a11y.as_mut(), self.instance.as_mut()) {
            if let Some(tick) = a11y.handle_window_event(instance, &event.window_event) {
                if tick.needs_paint || tick.jobs_pending {
                    self.request_redraw();
                }
            }
        }
    }

    /// Wake to keep the caret blinking and momentum scrolling coasting.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let flow = self
            .instance
            .as_ref()
            .and_then(|instance| instance.next_wake_deadline())
            .map_or(ControlFlow::Wait, ControlFlow::WaitUntil);
        event_loop.set_control_flow(flow);
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if matches!(cause, winit::event::StartCause::ResumeTimeReached { .. }) {
            self.request_redraw();
        }
    }
}

#[cfg(feature = "jsx-compiler")]
fn compile(source: &str) -> String {
    compile_component_source(std::path::Path::new("a11y_touch.jsx"), source)
        .expect("compile failed")
}

#[cfg(not(feature = "jsx-compiler"))]
fn compile(_source: &str) -> String {
    panic!("a11y_touch example requires the `jsx-compiler` feature");
}

fn main() {
    let event_loop = EventLoop::<accesskit_winit::Event>::with_user_event()
        .build()
        .expect("event loop");
    let proxy = event_loop.create_proxy();
    let mut app = App {
        window: None,
        instance: None,
        gpu: None,
        bridge: WinitBridge::new(),
        a11y: None,
        proxy,
        capture_path: capture_path_from_cli(),
        capture_done: false,
    };
    event_loop.run_app(&mut app).expect("run");
}
