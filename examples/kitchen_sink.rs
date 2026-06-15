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
use oxide_dom::compile_component_file;
use oxide_dom::{
    Event, FileWatch, Instance, InstanceConfig, KeyboardEvent, MouseButton, MouseEvent, Scene,
    SurfaceRect, TickResult,
};
use winit::application::ApplicationHandler;
use winit::event::KeyEvent;
use winit::event::{ElementState, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};
use winit::window::{Window, WindowId};

const PROJECT_NAME: &str = "oxide-dom-kitchen-sink";
const TARGET_LABELS: [&str; 3] = ["Left Target", "Center Target", "Right Target"];
// Kitchen-sink component. Layout, colours, and hover behaviour live in CSS —
// JS is reserved for genuine application state (input values, scrollTop,
// and row count). `:hover` and `:focus` are pure CSS pseudo-classes; no
// `onMouseEnter`/`onMouseLeave` handlers are needed for styling.
const APP_JSX_SOURCE: &str = r#"
import { render } from "oxide-runtime";

function App() {
  const targetLabel = globalThis.state.targetLabel || "Pane";
  const textValue = String(globalThis.state.text || "");
  const numberValue = String(globalThis.state.number || "");
  const rangeValue = String(globalThis.state.range || 50);
  const checkboxValue = Boolean(globalThis.state.checkboxChecked);
  const radioAValue = Boolean(globalThis.state.radioA);

  const renderRows = () => {
    const nodes = [];
    const count = Math.max(1, Number(globalThis.state.rows || 24));
    for (let i = 0; i < count; i++) {
      const stripeClass = i % 2 === 0 ? "row row-even" : "row row-odd";
      nodes.push(
        <div class={stripeClass}>
          {"Row " + (i + 1) + " - hover and scroll me"}
        </div>,
      );
    }
    return nodes;
  };

  return (
    <div class="panel">
      <div class="panel-title">{targetLabel}: Kitchen Sink</div>

      <div class="toolbar">
        <button
          class="btn btn-add"
          onClick={() => {
            const next = Math.max(1, Number(globalThis.state.rows || 24)) + 1;
            globalThis.state.rows = next;
            sendEvent(
              "action",
              JSON.stringify({ type: "rows", target: targetLabel, count: next }),
            );
          }}
        >
          + Add Row
        </button>

        <button
          class="btn btn-clear"
          onClick={() => {
            globalThis.state.rows = 20;
            globalThis.state.text = "";
            globalThis.state.number = "";
            globalThis.state.range = 50;
            globalThis.state.checkboxChecked = false;
            globalThis.state.radioA = false;
            globalThis.state.radioB = false;
            globalThis.state.password = "";
            globalThis.state.selectValue = "";
            sendEvent(
              "action",
              JSON.stringify({ type: "clear", target: targetLabel }),
            );
          }}
        >
          Clear
        </button>
      </div>

      <input
        class="field field-text"
        type="text"
        value={textValue}
        placeholder="Type here..."
        onInput={(event) => {
          globalThis.state.text = event.value;
        }}
      />

      <input
        class="field field-number"
        type="number"
        value={numberValue}
        placeholder="Numeric value"
        min="-100"
        max="100"
        step="0.5"
        onInput={(event) => {
          globalThis.state.number = event.value;
        }}
      />

      <input
        class="field field-range"
        type="range"
        min="0"
        max="100"
        step="5"
        value={rangeValue}
        onInput={(event) => {
          globalThis.state.range = event.value;
        }}
      />

      <div class="inline-fields">
        <input
          class="field field-checkbox"
          type="checkbox"
          checked={checkboxValue}
          onInput={(event) => {
            globalThis.state.checkboxChecked = event.checked;
          }}
        />
        <input
          class="field field-radio"
          type="radio"
          name="sink-mode"
          onInput={(event) => {
            globalThis.state.radioA = event.checked;
            if (event.checked) {
              globalThis.state.radioB = false;
            }
          }}
        />
        <input
          class="field field-radio"
          type="radio"
          name="sink-mode"
          onInput={(event) => {
            globalThis.state.radioB = event.checked;
            if (event.checked) {
              globalThis.state.radioA = false;
            }
          }}
        />
      </div>

      <input
        class="field field-password"
        type="password"
        value={globalThis.state.password || ""}
        placeholder="secret..."
        onInput={(event) => {
          globalThis.state.password = event.value;
        }}
      />

      <select
        class="field field-select"
        value={globalThis.state.selectValue ?? ""}
        onChange={(event) => {
          globalThis.state.selectValue = event.value;
          sendEvent(
            "select",
            JSON.stringify({ target: targetLabel, value: event.value }),
          );
        }}
      >
        <option value="" disabled selected hidden>Choose..</option>
        <option value="option1">First Option</option>
        <option value="option2">Second Option</option>
        <option value="option3">Third Option</option>
        <option value="option4" disabled>Disabled Option</option>
        <option value="option5">Last Option</option>
      </select>

      <div
        class="rows"
        onWheel={(event) => {
          globalThis.state.wheelCount = Number(globalThis.state.wheelCount || 0) + 1;
          sendEvent(
            "wheel",
            JSON.stringify({ target: targetLabel, deltaY: event.deltaY }),
          );
        }}
        onScroll={(event) => {
          globalThis.state.scrollTop = event.scrollTop;
        }}
      >
        {renderRows}
      </div>

      <div class="status">
        {() =>
          `rows=${Math.max(1, Number(globalThis.state.rows || 24))} wheel=${globalThis.state.wheelCount || 0} scrollTop=${globalThis.state.scrollTop || 0} text="${textValue}" number="${numberValue}" range=${rangeValue} checkbox=${checkboxValue ? "on" : "off"} radioA=${radioAValue ? "on" : "off"} radioB=${globalThis.state.radioB ? "on" : "off"} password=${globalThis.state.password || ""} select=${globalThis.state.selectValue ?? ""}`
        }
      </div>
    </div>
  );
}

render(() => App(), __OX_ROOT__);
"#;

// CSS shared by every kitchen-sink instance. All hover/focus visual logic
// lives here — JS only sets a class name. Note: `:nth-child` isn't fully
// supported by Blitz, so row stripes use explicit `row-even`/`row-odd` classes
// assigned at render time.
const APP_CSS: &str = r#"
.panel {
    display: block;
    width: 360px;
    padding: 10px;
    background: #182238;
    color: #f0f4ff;
    border: 1px solid #3a4f74;
}
.panel-title {
    margin-bottom: 10px;
    font: 700 16px/1.2 system-ui, sans-serif;
}
.toolbar {
    margin-bottom: 10px;
}

/* Buttons — :hover swaps the background. No JS handlers required.
   NOTE: `transition:` would register an animation set entry per node. The
   current Blitz snapshot doesn't clear that entry when the node is removed
   (see process_removed_subtree in blitz-dom/src/mutator.rs), so dynamic
   subtrees that use transitions will panic in resolve_stylist on the next
   restyle after a removal. Drop `transition:` until that's fixed upstream. */
.btn {
    display: inline-block;
    padding: 8px 10px;
    border-radius: 7px;
    cursor: pointer;
    font-size: 13px;
    color: #f3f7ff;
}
.btn-add {
    margin-right: 8px;
    border: 1px solid #7fb5ff;
    background: #1f3b5f;
}
.btn-add:hover  { background: #5b8cfa; color: #ffffff; }
.btn-add:active { background: #406fc9; }

.btn-clear {
    border: 1px solid #c28aff;
    background: #3c225f;
}
.btn-clear:hover  { background: #7e45d8; color: #ffffff; }
.btn-clear:active { background: #5d2fa3; }

/* Text field — :focus drives the outline. */
.field {
    display: block;
    width: 336px;
    min-height: 1em;
    padding: 4px;
    margin-bottom: 10px;
    font-size: 17px;
    font-family: monospace;
    color: #ffffff;
    background: #0f1723;
    border: 1px solid #4f6282;
    outline: 1px solid transparent;
}
.field:focus { outline: 2px solid #80b0ff; }

.inline-fields {
    display: flex;
    gap: 10px;
    margin-bottom: 10px;
    align-items: center;
}

/* text / number / password all share the same single-line appearance */
.field-text,
.field-number,
.field-password {
    width: 336px;
}

/* Select dropdown styling */
select.field-select {
    width: 336px;
    cursor: pointer;
}

/* Range slider: strip away the box — the renderer paints its own
   track + thumb using the CSS `color` as the accent. */
.field-range {
    width: 336px;
    height: 28px;
    min-height: 28px;
    padding: 0 4px;
    margin-bottom: 10px;
    background: transparent;
    border: none;
    outline: none;
    color: #5b8cfa;
    cursor: pointer;
}

/* Checkbox and radio: fixed square, accent colour drives the painted widget.
   box-sizing keeps the rendered border-box at the declared size — without it
   `padding` adds 6px to each axis and the radios become 28x28 rounded
   rectangles instead of 22x22 squares. */
input.field-checkbox,
input.field-radio {
    box-sizing: border-box;
    width: 22px;
    height: 22px;
    min-height: 22px;
    min-width: 22px;
    padding: 3px;
    margin: 0;
    color: #5b8cfa;
    cursor: pointer;
    flex: 0 0 auto;
}
/* UA stylesheet sets a fixed 14px border-radius for radios, which is enough
   for the 14px UA size but reads as a pill once we resize the box. Half the
   box (11px) collapses the corners into a circle. */
input.field-checkbox {
    border-radius: 0;
}
input.field-radio {
    border-radius: 11px;
}

/* Scrollable row list — alternating stripes + per-row :hover. */
.rows {
    display: block;
    width: 352px;
    height: 190px;
    overflow: auto;
    border: 1px solid #4f6282;
    background: #0f1420;
}
.row {
    display: block;
    padding: 7px 10px;
    font: 12px/1.35 monospace;
    color: #cfdaef;
    border-bottom: 1px solid #25314a;
    outline: 1px solid transparent;
}
.row-even { background: #141b2b; }
.row-odd  { background: #101725; }
.row:hover {
    background: #224e6f;
    color: #ffffff;
    outline: 1px solid #7dd3fc;
}

.status {
    margin-top: 8px;
    font-size: 11px;
    font-family: monospace;
    color: #b7c3e0;
}
"#;

struct DemoProject {
    source_dir: PathBuf,
    dist_file: PathBuf,
}

struct RenderTargetData {
    label: String,
    rx: tokio::sync::mpsc::UnboundedReceiver<Event>,
}

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    project: Option<DemoProject>,
    watch: Option<FileWatch>,
    scene: Scene<RenderTargetData>,
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

    fn to_logical_pos(&self, x: f64, y: f64) -> (f32, f32) {
        let scale = self.scale_factor_safe();
        ((x / scale) as f32, (y / scale) as f32)
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

    fn maybe_rebuild(&mut self) {
        let (Some(project), Some(watch), Some(gpu)) = (
            self.project.as_ref(),
            self.watch.as_mut(),
            self.gpu.as_ref(),
        ) else {
            return;
        };

        let mut needs_rebuild = false;
        while let Some(path) = watch.poll() {
            if Self::is_relevant_source_change(&path, &project.source_dir) {
                needs_rebuild = true;
            }
        }

        if !needs_rebuild {
            return;
        }

        if let Err(err) = build_bundle(project) {
            eprintln!("[{PROJECT_NAME}] rebuild failed: {err}");
            return;
        }

        let (width, height) = self.window.as_ref().map_or((640, 420), |w| {
            self.to_logical_size(w.inner_size().width, w.inner_size().height)
        });
        let layouts = Self::target_layout(width, TARGET_LABELS.len());

        match mount_targets(
            &project.dist_file,
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
        let mut scene = match mount_targets(
            &project.dist_file,
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
        self.scene = scene;

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

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

            WindowEvent::CursorMoved { position, .. } => {
                self.last_mouse = self.to_logical_pos(position.x, position.y);

                let event = MouseEvent::Move {
                    x: self.last_mouse.0,
                    y: self.last_mouse.1,
                };
                let result = self
                    .scene
                    .dispatch_mouse(self.last_mouse.0, self.last_mouse.1, event);

                if result.needs_paint {
                    if let Some(window) = &self.window {
                        window.request_redraw();
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self.last_mouse;

                let browser_button = match button {
                    winit::event::MouseButton::Left => Some(MouseButton::Left),
                    winit::event::MouseButton::Right => Some(MouseButton::Right),
                    winit::event::MouseButton::Middle => Some(MouseButton::Middle),
                    _ => None,
                };
                let Some(button) = browser_button else {
                    return;
                };

                let event = match state {
                    ElementState::Pressed => MouseEvent::Down { x, y, button },
                    ElementState::Released => MouseEvent::Up { x, y, button },
                };

                let result = self.scene.dispatch_mouse(x, y, event);
                if result.needs_paint
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(modifiers) => {
                self.modifiers = modifiers.state();
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let (x, y) = self.last_mouse;
                let (delta_x, delta_y) = match delta {
                    MouseScrollDelta::LineDelta(dx, dy) => (dx * 40.0, dy * 40.0),
                    MouseScrollDelta::PixelDelta(position) => {
                        (position.x as f32, position.y as f32)
                    }
                };

                let result = self.scene.dispatch_mouse(
                    x,
                    y,
                    MouseEvent::Wheel {
                        x,
                        y,
                        delta_x,
                        delta_y,
                    },
                );
                if result.needs_paint
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                if self.gpu.is_none() {
                    return;
                }

                self.maybe_rebuild();

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
                let key = key_to_string(&logical_key, text.as_deref());
                let code = match physical_key {
                    PhysicalKey::Code(code) => format!("{code:?}"),
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
                let result = self.scene.dispatch_key_down(event);
                println!(
                    "[{PROJECT_NAME}] keyboard down key={} repeat={repeat}",
                    event_key
                );
                if (result.needs_paint || result.jobs_pending)
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
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
                let key = key_to_string(&logical_key, text.as_deref());
                let code = match physical_key {
                    PhysicalKey::Code(code) => format!("{code:?}"),
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
                let result = self.scene.dispatch_key_up(event);
                println!(
                    "[{PROJECT_NAME}] keyboard up key={} repeat={repeat}",
                    event_key
                );
                if (result.needs_paint || result.jobs_pending)
                    && let Some(window) = &self.window
                {
                    window.request_redraw();
                }
            }

            _ => {}
        }
    }

    /// Called by winit when the loop is about to block waiting for events.
    /// Blink-enabled inputs need periodic wake-ups so the caret can repaint.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let mut next_blink: Option<std::time::Instant> = None;
        for surface in self.scene.surfaces() {
            if let Some(deadline) = surface.instance.next_blink_deadline() {
                next_blink = match next_blink {
                    Some(existing) => Some(existing.min(deadline)),
                    None => Some(deadline),
                };
            }
        }

        if let Some(deadline) = next_blink {
            event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
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

fn create_demo_project() -> io::Result<DemoProject> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();

    let root = std::env::temp_dir().join(format!("{PROJECT_NAME}-{nanos}"));
    let source_dir = root.join("src");
    create_dir_all(&source_dir)?;
    create_dir_all(root.join("dist"))?;

    write(source_dir.join("App.jsx"), APP_JSX_SOURCE)?;

    Ok(DemoProject {
        source_dir,
        dist_file: root.join("dist/App.js"),
    })
}

fn build_bundle(project: &DemoProject) -> io::Result<()> {
    #[cfg(feature = "jsx-compiler")]
    {
        create_dir_all(&project.dist_file.parent().expect("dist parent"))?;
        let source_path = project.source_dir.join("App.jsx");
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
            "render(() => App(), __OX_ROOT__);",
            &format!("{seed}render(() => App(), __OX_ROOT__);"),
        );

        let (instance, rx) = Instance::new(
            InstanceConfig {
                width,
                height,
                device: Arc::clone(device),
                queue: Arc::clone(queue),
                stylesheets: vec![APP_CSS.to_string()],
                document_scroll: false,
            },
            &seeded_source,
        );
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
            label: Some("oxide-dom-kitchen-device"),
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
        scene: Scene::new(),
        last_mouse: (0.0, 0.0),
        modifiers: ModifiersState::empty(),
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
            &root.join("project/src/App.jsx"),
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
