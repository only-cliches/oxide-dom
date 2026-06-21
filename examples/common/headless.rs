use std::sync::Arc;

use wgpu;

pub async fn init_headless_device(
    device_label: &str,
    power_preference: wgpu::PowerPreference,
) -> (Arc<wgpu::Device>, Arc<wgpu::Queue>) {
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        unsafe {
            std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
        }
    }

    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::all(),
        ..wgpu::InstanceDescriptor::new_without_display_handle()
    });
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .expect("no adapter available");

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some(device_label),
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
