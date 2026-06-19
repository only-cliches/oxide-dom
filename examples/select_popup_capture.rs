// Headless capture of a <select> with its popup forced open. Used to
// visually verify the popup-overlay rendering path without needing a
// real pointer event.

#[path = "common/args.rs"]
mod args;

use std::path::PathBuf;
use std::sync::Arc;

use solite::{Instance, InstanceConfig, MouseButton, MouseEvent, capture::capture_texture_to_png};
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;

// Mirror `kitchen_sink`'s JSX select pattern: an `onChange` handler that
// writes global state and a controlled `value` binding so this example
// exercises the same update path as reactive JSX-based `<select>` usage.
const COMPONENT: &str = r#"
import { render } from "solite-runtime";

function App() {
  function mkOpt(value, text, disabled) {
    return (
      <option value={value} disabled={disabled}>
        {text}
      </option>
    );
  }

  return (
    <div class="panel">
      <div class="label">Pick one:</div>
      <select
        class="sel"
        value={globalThis.state.selectValue || "a"}
        onChange={(event) => {
          globalThis.state.selectValue = event.value;
        }}
      >
        <option value="" disabled selected hidden>
          Choose...
        </option>
        {mkOpt("a", "Apple", false)}
        {mkOpt("b", "Banana", false)}
        {mkOpt("c", "Cherry", false)}
        {mkOpt("d", "Date (disabled)", true)}
        {mkOpt("e", "Elderberry", false)}
      </select>
    </div>
  );
}

render(() => <App />, __SOL_ROOT__);
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
            label: Some("solite-select-popup-capture"),
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
    let output =
        args::capture_path_from_cli().unwrap_or_else(|| PathBuf::from("captures/select_popup.png"));

    let (device, queue) = pollster::block_on(init_device());
    let component = compile_select_popup_capture_component_source(COMPONENT);
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 420,
            height: 320,
            device: device.clone(),
            queue: queue.clone(),
            stylesheets: vec![CSS.to_string()],
            document_scroll: false,
            base_url: None,
            initial_state: None,
            registered_resources: vec![],
        },
        &component,
    )
    .expect("create instance");

    // Pump the JS once so all bridge calls (including the rebuild that
    // populates the select's options) flush before we force the popup open.
    let _ = instance.tick();
    let _ = instance.render();

    let select_id = instance
        .select_node_ids()
        .first()
        .copied()
        .expect("no <select> registered");

    // Open the select by clicking it (mirrors what handle_select_click does
    // via real pointer input). One frame of tick+render lays out the popup.
    let (sx, sy) = (50.0, 60.0);
    let _ = instance.dispatch_mouse(sx, sy, MouseEvent::Move { x: sx, y: sy });
    let _ = instance.dispatch_mouse(
        sx,
        sy,
        MouseEvent::Down {
            x: sx,
            y: sy,
            button: MouseButton::Left,
        },
    );
    let _ = instance.dispatch_mouse(
        sx,
        sy,
        MouseEvent::Up {
            x: sx,
            y: sy,
            button: MouseButton::Left,
        },
    );
    let _ = instance.tick();
    let _ = instance.render();
    let _ = select_id;

    // Capture the open-popup state (no click).
    let (x, y) = (100.0, 120.0);
    let _ = instance.dispatch_mouse(x, y, MouseEvent::Move { x, y });
    let _ = instance.tick();
    let _ = instance.render();

    if let Err(err) = capture_texture_to_png(&device, &queue, instance.texture(), output.as_path())
    {
        eprintln!("failed to capture frame: {err}");
        std::process::exit(1);
    }
    println!("captured to {}", output.display());
}

#[cfg(feature = "jsx-compiler")]
fn compile_select_popup_capture_component_source(component_source: &str) -> String {
    compile_component_source(std::path::Path::new("select_popup_capture.jsx"), component_source)
        .expect("JSX compile failed")
}

#[cfg(not(feature = "jsx-compiler"))]
fn compile_select_popup_capture_component_source(_component_source: &str) -> String {
    panic!("select_popup_capture example requires the `jsx-compiler` feature");
}
