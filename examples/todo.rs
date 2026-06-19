use std::sync::Arc;

#[path = "common/args.rs"]
mod args;
#[path = "common/blit.rs"]
mod blit;
#[path = "common/capture.rs"]
mod capture;

use blit::{BlitContext, BlitDraw};
use blitz_traits::shell::{ClipboardError, ShellProvider};
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use solite::winit::WinitBridge;
use solite::{Instance, InstanceConfig, Scene, SurfaceRect};
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalPosition;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::window::{Window, WindowId};

const TODO_JSX: &str = include_str!("todo_app.jsx");

const TODO_CSS: &str = include_str!("todo_app.css");

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

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    bridge: WinitBridge,
    scene: Option<Scene<()>>,
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("solite: todo")
            .with_inner_size(winit::dpi::LogicalSize::new(540u32, 800u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let gpu = pollster::block_on(init_gpu(window.clone()));
        let (width, height) = {
            let scale = window.scale_factor().max(1.0);
            let w = (gpu.config.width as f64 / scale).max(1.0).round() as u32;
            let h = (gpu.config.height as f64 / scale).max(1.0).round() as u32;
            (w, h)
        };

        #[cfg(feature = "jsx-compiler")]
        let todo_js = compile_component_source(std::path::Path::new("App.jsx"), TODO_JSX)
            .expect("JSX compile failed");
        #[cfg(not(feature = "jsx-compiler"))]
        panic!("todo example requires the `jsx-compiler` feature");

        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width,
                height,
                device: gpu.device.clone(),
                queue: gpu.queue.clone(),
                stylesheets: vec![TODO_CSS.to_string()],
                document_scroll: true,
                base_url: None,
            },
            &todo_js,
        )
        .expect("create instance");
        instance.set_shell_provider(Arc::new(SystemClipboard));
        let _ = instance.tick();

        let mut scene = Scene::new();
        scene.add_surface(
            instance,
            SurfaceRect::new(0.0, 0.0, width as f32, height as f32),
            (),
        );

        self.window = Some(window);
        self.gpu = Some(gpu);
        self.scene = Some(scene);

        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::RedrawRequested => {
                if let (Some(gpu), Some(scene)) = (self.gpu.as_ref(), self.scene.as_mut()) {
                    let mut draws = Vec::new();
                    let mut needs_redraw = false;

                    for surface in scene.surfaces_mut() {
                        let tick = surface.instance.tick();
                        needs_redraw = needs_redraw || tick.needs_paint || tick.jobs_pending;
                        let view = surface.instance.render().clone();
                        draws.push(BlitDraw {
                            view,
                            x: 0,
                            y: 0,
                            width: gpu.config.width,
                            height: gpu.config.height,
                        });
                    }

                    if !draws.is_empty() {
                        let _did_redraw = blit::present_to_surface(
                            &gpu.device,
                            &gpu.queue,
                            &gpu.surface,
                            &gpu.config,
                            &gpu.blit,
                            &draws,
                        );

                        if needs_redraw {
                            if let Some(window) = &self.window {
                                window.request_redraw();
                            }
                        }
                    }
                }
            }

            WindowEvent::Resized(size) => {
                if let (Some(gpu), Some(scene), Some(window)) =
                    (self.gpu.as_mut(), self.scene.as_mut(), self.window.as_ref())
                {
                    let scale = window.scale_factor().max(1.0);
                    let width = (size.width as f64 / scale).max(1.0).round() as u32;
                    let height = (size.height as f64 / scale).max(1.0).round() as u32;

                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);

                    for surface in scene.surfaces_mut() {
                        surface.instance.resize(width, height);
                    }

                    window.request_redraw();
                }
            }

            event => {
                if let Some(scene) = self.scene.as_mut() {
                    let scale = self
                        .window
                        .as_ref()
                        .map(|w| w.scale_factor())
                        .unwrap_or(1.0);
                    let translated = match event {
                        WindowEvent::CursorMoved {
                            device_id,
                            position,
                        } => WindowEvent::CursorMoved {
                            device_id,
                            position: PhysicalPosition::new(position.x / scale, position.y / scale),
                        },
                        other => other,
                    };
                    let r = self.bridge.handle(scene, &translated);
                    if r.close_requested {
                        event_loop.exit();
                        return;
                    }
                    if r.needs_redraw || r.jobs_pending {
                        if let Some(window) = &self.window {
                            window.request_redraw();
                        }
                    }
                }
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if let Some(scene) = self.scene.as_ref() {
            let mut next_deadline: Option<std::time::Instant> = None;
            for surface in scene.surfaces() {
                if let Some(deadline) = surface.instance.next_blink_deadline() {
                    next_deadline = Some(match next_deadline {
                        None => deadline,
                        Some(current) => current.min(deadline),
                    });
                }
            }

            if let Some(deadline) = next_deadline {
                event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
            } else {
                event_loop.set_control_flow(ControlFlow::Wait);
            }
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: winit::event::StartCause) {
        if matches!(cause, winit::event::StartCause::ResumeTimeReached { .. }) {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
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
            label: Some("solite-todo-device"),
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
        bridge: WinitBridge::new(),
        scene: None,
    };
    event_loop.run_app(&mut app).expect("run");
}
