// Headless capture of a <select> with its popup forced open. Used to
// visually verify the popup-overlay rendering path without needing a
// real pointer event.

#[path = "common/args.rs"]
mod args;
#[path = "common/capture.rs"]
mod capture;

use std::path::PathBuf;
use std::sync::Arc;

use oxide_dom::{Instance, InstanceConfig, MouseButton, MouseEvent};

// Hand-built via bridge primitives. We mirror what the kitchen_sink JSX
// produces for `<select value={state.x} onChange={...}>`: an onChange
// handler that writes state (triggering a reactive value re-set on the
// select itself) so we can exercise the same path that panics under winit.
const COMPONENT: &str = r#"
import { createEffect, render } from "oxide-runtime";

function App() {
  const panel = __ox_createElement("div");
  __ox_setProperty(panel, "className", "panel");

  const label = __ox_createElement("div");
  __ox_setProperty(label, "className", "label");
  __ox_insertNode(label, __ox_createTextNode("Pick one:"), null);
  __ox_insertNode(panel, label, null);

  const sel = __ox_createElement("select");
  __ox_setProperty(sel, "className", "sel");
  __ox_setProperty(sel, "onChange", (event) => {
    globalThis.state.selectValue = event.value;
  });

  function mkOpt(value, text, disabled) {
    const opt = __ox_createElement("option");
    __ox_setProperty(opt, "value", value);
    if (disabled) __ox_setProperty(opt, "disabled", "");
    __ox_insertNode(opt, __ox_createTextNode(text), null);
    return opt;
  }

  // Leading sentinel: empty value, disabled, selected, hidden — the
  // canonical placeholder pattern. The popup should NOT render this row
  // but the closed display should read "Choose…" until the user picks.
  function mkSentinel() {
    const opt = __ox_createElement("option");
    __ox_setProperty(opt, "value", "");
    __ox_setProperty(opt, "disabled", "");
    __ox_setProperty(opt, "selected", "");
    __ox_setProperty(opt, "hidden", "");
    __ox_insertNode(opt, __ox_createTextNode("Choose..."), null);
    return opt;
  }
  __ox_insertNode(sel, mkSentinel(), null);
  __ox_insertNode(sel, mkOpt("a", "Apple", false), null);
  __ox_insertNode(sel, mkOpt("b", "Banana", false), null);
  __ox_insertNode(sel, mkOpt("c", "Cherry", false), null);
  __ox_insertNode(sel, mkOpt("d", "Date (disabled)", true), null);
  __ox_insertNode(sel, mkOpt("e", "Elderberry", false), null);

  // Reactive `value` mirroring globalThis.state.selectValue, matching what
  // the JSX compiler emits for `value={globalThis.state.selectValue || ...}`.
  createEffect(() => __ox_setProperty(sel, "value", globalThis.state.selectValue || "a"));

  __ox_insertNode(panel, sel, null);
  return panel;
}

render(() => App(), __OX_ROOT__);
"#;

const CSS: &str = r#"
.panel {
    display: block;
    padding: 20px;
    background: #182238;
    color: #f0f4ff;
    width: 360px;
    font: 16px system-ui, sans-serif;
}
.label { margin-bottom: 8px; }
.sel {
    display: block;
    width: 336px;
    min-height: 32px;
    padding: 6px 8px;
    background: #0f1723;
    color: #ffffff;
    border: 1px solid #4f6282;
}
"#;

async fn init_device() -> (Arc<wgpu::Device>, Arc<wgpu::Queue>) {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        }
    }
    let wgpu_instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = wgpu_instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .expect("no adapter");
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("oxide-dom-select-popup-capture"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            memory_hints: wgpu::MemoryHints::default(),
            trace: wgpu::Trace::Off,
        })
        .await
        .expect("device");
    (Arc::new(device), Arc::new(queue))
}

fn main() {
    let output = args::capture_path_from_cli()
        .unwrap_or_else(|| PathBuf::from("captures/select_popup.png"));

    let (device, queue) = pollster::block_on(init_device());
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 420,
            height: 320,
            device: device.clone(),
            queue: queue.clone(),
            stylesheets: vec![CSS.to_string()],
            document_scroll: false,
        },
        COMPONENT,
    );

    // Pump the JS once so all bridge calls (including the rebuild that
    // populates the select's options) flush before we force the popup open.
    let _ = instance.tick();
    let _ = instance.render();

    let select_id = instance.select_node_ids().first().copied().expect("no <select> registered");

    // Open the select by clicking it (mirrors what handle_select_click does
    // via real pointer input). One frame of tick+render lays out the popup.
    let (sx, sy) = (50.0, 60.0);
    let _ = instance.dispatch_mouse(sx, sy, MouseEvent::Move { x: sx, y: sy });
    let _ = instance.dispatch_mouse(sx, sy, MouseEvent::Down { x: sx, y: sy, button: MouseButton::Left });
    let _ = instance.dispatch_mouse(sx, sy, MouseEvent::Up { x: sx, y: sy, button: MouseButton::Left });
    let _ = instance.tick();
    let _ = instance.render();
    let _ = select_id;

    // Capture the open-popup state (no click).
    let (x, y) = (100.0, 120.0);
    let _ = instance.dispatch_mouse(x, y, MouseEvent::Move { x, y });
    let _ = instance.tick();
    let _ = instance.render();

    if let Err(err) = capture::capture_texture_to_png(
        &device,
        &queue,
        instance.texture(),
        output.as_path(),
    ) {
        eprintln!("failed to capture frame: {err}");
        std::process::exit(1);
    }
    println!("captured to {}", output.display());
}
