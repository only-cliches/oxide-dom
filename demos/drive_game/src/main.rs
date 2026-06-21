mod game;
mod hud;
mod renderer;

use std::sync::Arc;
use std::time::Instant;

use game::{CarState, InputState};
use hud::Hud;
use renderer::GameRenderer;
use solite::gpu::{BlitContext, BlitDraw, WindowGpu};
use solite::winit::WinitBridge;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

struct App {
    window: Option<Arc<Window>>,
    gpu: Option<WindowGpu>,
    blit: Option<BlitContext>,
    renderer: Option<GameRenderer>,
    hud: Option<Hud>,
    bridge: WinitBridge,
    input: InputState,
    car: CarState,
    last_frame: Instant,
    fps: f32,
    frame_ms: f32,
    occluded: bool,
    // HUD refresh throttling: the car scene renders at the full display rate,
    // but the telemetry HUD — whose per-frame Vello re-render dominates CPU
    // cost — only repaints `hud_refresh_hz` times/second, reusing the last
    // painted texture in between.
    hud_refresh_hz: f32,
    hud_accum_secs: f32,
    hud_draw_cache: Option<BlitDraw>,
}

impl Default for App {
    fn default() -> Self {
        Self {
            window: None,
            gpu: None,
            blit: None,
            renderer: None,
            hud: None,
            bridge: WinitBridge::new(),
            input: InputState::default(),
            car: CarState::default(),
            last_frame: Instant::now(),
            fps: 0.0,
            frame_ms: 0.0,
            occluded: false,
            // 30Hz: the per-frame Vello HUD re-render is the dominant CPU cost,
            // and the telemetry overlay doesn't need 60Hz. Tunable via
            // SOLITE_HUD_HZ (lower = cheaper but choppier HUD; the car scene
            // always runs at the full display rate regardless).
            hud_refresh_hz: std::env::var("SOLITE_HUD_HZ")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(30.0),
            hud_accum_secs: 0.0,
            hud_draw_cache: None,
        }
    }
}

impl App {
    fn request_redraw(&self) {
        if !self.occluded
            && let Some(window) = &self.window
        {
            window.request_redraw();
        }
    }

    fn resize(&mut self, width: u32, height: u32) {
        if let Some(gpu) = &mut self.gpu {
            gpu.resize(width, height);
        }
        if let Some(hud) = &mut self.hud {
            let (logical_width, logical_height) = self.bridge.to_logical_size(width, height);
            hud.resize(logical_width, logical_height, self.bridge.scale_factor());
        }
        // The HUD texture was reallocated; force a repaint next frame.
        self.hud_draw_cache = None;
    }

    fn handle_keyboard(&mut self, event_loop: &ActiveEventLoop, event: &winit::event::KeyEvent) {
        let PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        if code == KeyCode::Escape && event.state == ElementState::Pressed {
            event_loop.exit();
            return;
        }
        if self.input.set_key(code, event.state) {
            self.request_redraw();
        }
    }

    fn redraw(&mut self) {
        if self.occluded {
            return;
        }

        let window = self.window.clone();
        let (Some(gpu), Some(renderer), Some(blit), Some(hud)) = (
            self.gpu.as_mut(),
            self.renderer.as_ref(),
            self.blit.as_ref(),
            self.hud.as_mut(),
        ) else {
            return;
        };

        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        if dt > 0.0 {
            let instant_fps = 1.0 / dt;
            self.fps = if self.fps <= 0.0 {
                instant_fps
            } else {
                self.fps * 0.90 + instant_fps * 0.10
            };
        }

        // Time the actual per-frame work — simulation + HUD + command encoding
        // and submission — excluding the vsync-bound surface acquire and
        // present below. `update_state` shows the previous frame's measurement
        // (a one-frame lag), which is standard for an on-screen frame timer.
        let work_start = Instant::now();

        // Slider input + hot-reload are cheap and stay responsive every frame.
        let _ = hud.maybe_reload(self.car);
        hud.poll_events();
        self.car.update(self.input, dt, hud.max_speed_mps());

        // Repaint the HUD at most `hud_refresh_hz` times/second; reuse the last
        // painted texture in between. The first frame always paints.
        self.hud_accum_secs += dt;
        let hud_interval = 1.0 / self.hud_refresh_hz;
        let refresh_hud = self.hud_draw_cache.is_none() || self.hud_accum_secs >= hud_interval;

        let hud_draw = if refresh_hud {
            self.hud_accum_secs = 0.0;
            hud.update_state(self.car, self.fps, self.frame_ms);
            hud.tick();
            let draw = hud.draw();
            self.hud_draw_cache = Some(draw.clone());
            draw
        } else {
            self.hud_draw_cache.clone().expect("hud cache present")
        };
        let cpu_pre = work_start.elapsed();

        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout => {
                return;
            }
            wgpu::CurrentSurfaceTexture::Occluded => {
                self.occluded = true;
                return;
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                gpu.reconfigure();
                if let Some(window) = &window {
                    window.request_redraw();
                }
                return;
            }
            wgpu::CurrentSurfaceTexture::Validation => return,
        };

        let render_start = Instant::now();
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = gpu
            .context
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("drive game frame encoder"),
            });

        renderer.render(
            &gpu.context.queue,
            &mut encoder,
            &view,
            gpu.config.width,
            gpu.config.height,
            self.car,
        );
        blit.encode_blit_to_view(
            &gpu.context.device,
            &mut encoder,
            &view,
            gpu.config.width,
            gpu.config.height,
            &[hud_draw],
            wgpu::LoadOp::Load,
        );

        gpu.context.queue.submit([encoder.finish()]);

        // Total CPU work this frame: simulation/HUD + command encoding/submit.
        let instant_ms = (cpu_pre + render_start.elapsed()).as_secs_f32() * 1000.0;
        self.frame_ms = if self.frame_ms <= 0.0 {
            instant_ms
        } else {
            self.frame_ms * 0.90 + instant_ms * 0.10
        };

        frame.present();
        if let Some(window) = &window {
            window.request_redraw();
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("Solite Drive Game")
            .with_inner_size(winit::dpi::LogicalSize::new(1280u32, 720u32));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));
        self.bridge.set_scale_factor(window.scale_factor());

        let size = window.inner_size();
        let gpu = pollster::block_on(WindowGpu::new(window.clone(), size.width, size.height))
            .expect("window gpu");
        let blit = BlitContext::new(&gpu.context.device, gpu.config.format);
        let renderer = GameRenderer::new(&gpu.context.device, gpu.config.format);
        let (logical_width, logical_height) = self.bridge.to_logical_size(size.width, size.height);
        let hud = match Hud::new(
            gpu.context.device.clone(),
            gpu.context.queue.clone(),
            logical_width,
            logical_height,
            self.bridge.scale_factor(),
            self.car,
        ) {
            Ok(hud) => hud,
            Err(err) => {
                eprintln!("[drive-game] failed to mount HUD: {err}");
                return;
            }
        };

        self.window = Some(window);
        self.gpu = Some(gpu);
        self.blit = Some(blit);
        self.renderer = Some(renderer);
        self.hud = Some(hud);
        self.last_frame = Instant::now();
        self.request_redraw();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match &event {
            WindowEvent::CloseRequested => {
                event_loop.exit();
                return;
            }
            WindowEvent::KeyboardInput { event, .. } => {
                self.handle_keyboard(event_loop, event);
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.bridge.set_scale_factor(*scale_factor);
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    self.resize(size.width, size.height);
                }
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = *occluded;
                if !*occluded {
                    self.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                if *focused {
                    self.occluded = false;
                    self.request_redraw();
                }
            }
            WindowEvent::Resized(size) => {
                self.resize(size.width, size.height);
                self.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                self.redraw();
                return;
            }
            _ => {}
        }

        if let Some(hud) = &mut self.hud {
            let response = self.bridge.handle(hud, &event);
            if response.needs_redraw || response.jobs_pending {
                self.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        self.request_redraw();
    }
}

fn main() {
    let event_loop = EventLoop::new().expect("event loop");
    let mut app = App::default();
    event_loop.run_app(&mut app).expect("run app");
}
