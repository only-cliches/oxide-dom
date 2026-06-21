use std::sync::Arc;

#[path = "common/gpu.rs"]
mod gpu;

use blitz_traits::shell::{ClipboardError, ShellProvider};
#[cfg(feature = "jsx-compiler")]
use solite::compile_component_source;
use solite::{
    Instance, InstanceConfig, Scene, SurfaceRect,
    gpu::{BlitDraw, present_to_surface},
    winit::WinitBridge,
};
use winit::application::ApplicationHandler;
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

type Gpu = gpu::Gpu;

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    bridge: WinitBridge,
    scene: Option<Scene<()>>,
}

impl App {
    fn logical_size(&self, width: u32, height: u32) -> (u32, u32) {
        self.bridge.to_logical_size(width, height)
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("solite: todo")
            .with_inner_size(winit::dpi::LogicalSize::new(540u32, 800u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let gpu = pollster::block_on(gpu::init_gpu(window.clone(), "solite-todo-device"));
        // The bridge owns the scale factor and the physical→logical math.
        self.bridge.set_scale_factor(window.scale_factor());
        let (width, height) = self.logical_size(gpu.config.width, gpu.config.height);

        let todo_js = todo_component_source();

        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width,
                height,
                #[cfg(feature = "gpu")]
                device: gpu.device.clone(),
                #[cfg(feature = "gpu")]
                queue: gpu.queue.clone(),
                stylesheets: vec![TODO_CSS.to_string()],
                document_scroll: true,
                base_url: None,
                initial_state: None,
                registered_resources: vec![],
                scale_factor: self.bridge.scale_factor(),
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
                        let _did_redraw = present_to_surface(
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
                if let Some(window) = self.window.as_ref() {
                    self.bridge.set_scale_factor(window.scale_factor());
                }
                let (width, height) = self.logical_size(size.width, size.height);
                if let (Some(gpu), Some(scene), Some(window)) =
                    (self.gpu.as_mut(), self.scene.as_mut(), self.window.as_ref())
                {
                    gpu.config.width = size.width.max(1);
                    gpu.config.height = size.height.max(1);
                    gpu.surface.configure(&gpu.device, &gpu.config);

                    for surface in scene.surfaces_mut() {
                        surface.rect = SurfaceRect::new(0.0, 0.0, width as f32, height as f32);
                        surface.instance.resize(width, height);
                    }

                    window.request_redraw();
                }
            }

            event => {
                if let Some(scene) = self.scene.as_mut() {
                    let r = self.bridge.handle(scene, &event);
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

#[cfg(feature = "jsx-compiler")]
fn todo_component_source() -> String {
    compile_component_source(std::path::Path::new("App.jsx"), TODO_JSX).expect("JSX compile failed")
}

#[cfg(not(feature = "jsx-compiler"))]
fn todo_component_source() -> String {
    panic!("todo example requires the `jsx-compiler` feature");
}
