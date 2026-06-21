// Headless render sanity check for the examples.
//
// Unlike the winit examples, this runs without a window and captures a single
// frame directly to a PNG from the instance output (CPU or GPU path).

use std::path::PathBuf;

#[cfg(not(feature = "gpu"))]
use solite::capture::capture_buffer_to_png;
#[cfg(feature = "gpu")]
use solite::capture::capture_texture_to_png;
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use solite::{Instance, InstanceConfig, capture::capture_path_from_cli};
#[cfg(feature = "gpu")]
use wgpu;

// All visual styling lives in CSS, registered through `InstanceConfig.stylesheets`.
// The component itself only chooses which `class` each element wears.
const HELLO_COMPONENT: &str = r#"
import { render } from "solite-runtime";

function App() {
  return <div class="hello">Hello from Solid</div>;
}

render(() => App(), __SOL_ROOT__);
"#;

const HELLO_CSS: &str = r#"
.hello {
    color: white;
    padding: 16px;
    background: #1e1e2e;
}
"#;

#[cfg(feature = "gpu")]
#[path = "common/headless.rs"]
mod headless;

fn main() {
    let output = capture_path_from_cli()
        .unwrap_or_else(|| PathBuf::from("/tmp/solite-headless-capture.png"));

    #[cfg(feature = "gpu")]
    {
        use std::sync::Arc;
        let (device, queue) = pollster::block_on(headless::init_headless_device(
            "solite-offscreen-capture",
            wgpu::PowerPreference::LowPower,
        ));
        let component = compile_offscreen_capture_component_source(HELLO_COMPONENT);
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device: Arc::clone(&device),
                queue: Arc::clone(&queue),
                stylesheets: vec![HELLO_CSS.to_string()],
                document_scroll: false,
                base_url: None,
                initial_state: None,
                registered_resources: vec![],
                scale_factor: 1.0,
            },
            &component,
        )
        .expect("create instance");
        let _ = instance.tick();
        let _ = instance.render();
        if let Err(err) =
            capture_texture_to_png(&device, &queue, instance.texture(), output.as_path())
        {
            eprintln!("failed to capture frame: {err}");
            std::process::exit(1);
        }
    }

    #[cfg(not(feature = "gpu"))]
    {
        let component = compile_offscreen_capture_component_source(HELLO_COMPONENT);
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                stylesheets: vec![HELLO_CSS.to_string()],
                document_scroll: false,
                base_url: None,
                initial_state: None,
                registered_resources: vec![],
                scale_factor: 1.0,
            },
            &component,
        )
        .expect("create instance");

        let _ = instance.tick();
        let pixel_bytes = instance.render();
        if let Err(err) = capture_buffer_to_png(200, 200, pixel_bytes, output.as_path()) {
            eprintln!("failed to capture frame: {err}");
            std::process::exit(1);
        }
    }

    println!("captured to {}", output.display());
}

#[cfg(feature = "jsx-compiler")]
fn compile_offscreen_capture_component_source(component_source: &str) -> String {
    compile_component_source(
        std::path::Path::new("offscreen_capture.jsx"),
        component_source,
    )
    .expect("JSX compile failed")
}

#[cfg(not(feature = "jsx-compiler"))]
fn compile_offscreen_capture_component_source(_component_source: &str) -> String {
    panic!("offscreen_capture example requires the `jsx-compiler` feature");
}
