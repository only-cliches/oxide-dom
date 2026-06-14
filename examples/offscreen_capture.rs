// Headless render sanity check for the examples.
//
// Unlike the winit examples, this runs without a window and captures a single
// frame directly from the instance output texture to a PNG.

#[path = "common/args.rs"]
mod args;
#[path = "common/capture.rs"]
mod capture;

use std::path::PathBuf;
use std::sync::Arc;

use oxide_dom::{Instance, InstanceConfig};

// All visual styling lives in CSS, registered through `InstanceConfig.stylesheets`.
// The component itself only chooses which `class` each element wears.
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
    color: white;
    padding: 16px;
    background: #1e1e2e;
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
        .expect("no adapter available for headless capture");

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("oxide-dom-offscreen-capture"),
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
        .unwrap_or_else(|| PathBuf::from("/tmp/oxide-dom-headless-capture.png"));

    let (device, queue) = pollster::block_on(init_device());
    let (mut instance, _rx) = Instance::new(
        InstanceConfig {
            width: 200,
            height: 200,
            device: device.clone(),
            queue: queue.clone(),
            stylesheets: vec![HELLO_CSS.to_string()],
        },
        HELLO_COMPONENT,
    );

    let _ = instance.tick();
    let _ = instance.render();
    if let Err(err) =
        capture::capture_texture_to_png(&device, &queue, instance.texture(), output.as_path())
    {
        eprintln!("failed to capture frame: {err}");
        std::process::exit(1);
    }

    println!("captured to {}", output.display());
}
