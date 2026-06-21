use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use solite::gpu::BlitDraw;
use solite::winit::WinitEventTarget;
use solite::workflow::{ReloadAction, SourceProject, SourceProjectWatch};
use solite::{
    Event, Instance, InstanceConfig, KeyboardEvent, MouseEvent, TickResult, VirtualSourceFile,
};
use tokio::sync::mpsc;

use crate::game::CarState;

/// Default forward speed cap in mph, matching the slider's starting position.
pub const DEFAULT_MAX_SPEED_MPH: f32 = 50.0;
const MPH_PER_MPS: f32 = 2.236_936;

#[cfg(not(debug_assertions))]
#[allow(dead_code)]
mod ui_bundle {
    include!(concat!(env!("OUT_DIR"), "/drive_game_bundle.rs"));
}

pub struct Hud {
    project: SourceProject,
    watch: Option<SourceProjectWatch>,
    instance: Instance,
    events: mpsc::UnboundedReceiver<Event>,
    device: Arc<wgpu::Device>,
    queue: Arc<wgpu::Queue>,
    width: u32,
    height: u32,
    scale_factor: f64,
    max_speed_mph: f32,
}

impl Hud {
    pub fn new(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        width: u32,
        height: u32,
        scale_factor: f64,
        car: CarState,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let project = SourceProject::new(ui_dir());
        let max_speed_mph = DEFAULT_MAX_SPEED_MPH;
        let config = Self::config(
            device.clone(),
            queue.clone(),
            width,
            height,
            scale_factor,
            car,
            max_speed_mph,
        );
        let (instance, events) = mount_project(&project, config)?;

        #[cfg(debug_assertions)]
        let watch = Some(project.watch()?);
        #[cfg(not(debug_assertions))]
        let watch = None;

        Ok(Self {
            project,
            watch,
            instance,
            events,
            device,
            queue,
            width,
            height,
            scale_factor,
            max_speed_mph,
        })
    }

    /// Forward speed cap in m/s, as set by the HUD slider.
    pub fn max_speed_mps(&self) -> f32 {
        self.max_speed_mph / MPH_PER_MPS
    }

    /// Drain queued JS events, applying slider-driven changes. Call once per
    /// frame before stepping the simulation so physics sees the latest cap.
    pub fn poll_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            if event.name == "maxSpeed" {
                if let Some(value) = event.payload.as_f64() {
                    // Range must match the slider bounds (MIN_MPH/MAX_MPH) in ui/index.tsx.
                    self.max_speed_mph = (value as f32).clamp(20.0, 50.0);
                }
            }
        }
    }

    pub fn resize(&mut self, width: u32, height: u32, scale_factor: f64) {
        self.width = width.max(1);
        self.height = height.max(1);
        self.scale_factor = scale_factor;
        self.instance.resize(self.width, self.height);
    }

    pub fn update_state(&mut self, car: CarState, fps: f32) {
        let state = self.instance.state();
        state.set("mode", json!(mode_label()));
        state.set("speedMph", json!(car.speed_mph()));
        state.set("headingDeg", json!(car.heading_degrees()));
        state.set("steering", json!(car.steering));
        state.set("throttle", json!(car.throttle));
        state.set("brake", json!(car.brake));
        state.set("x", json!(car.x));
        state.set("y", json!(car.y));
        state.set("fps", json!(fps));
        // Echo the authoritative cap back so the slider fill/handle track it
        // through the Rust→JS reactive path, regardless of where it changed.
        state.set("maxSpeed", json!(self.max_speed_mph));
    }

    pub fn maybe_reload(&mut self, car: CarState) -> bool {
        let Some(watch) = self.watch.as_ref() else {
            return false;
        };

        match watch.poll() {
            ReloadAction::None => false,
            ReloadAction::CssChanged(_) | ReloadAction::Remount => {
                let config = Self::config(
                    self.device.clone(),
                    self.queue.clone(),
                    self.width,
                    self.height,
                    self.scale_factor,
                    car,
                    self.max_speed_mph,
                );
                match mount_project(&self.project, config) {
                    Ok((instance, events)) => {
                        self.instance = instance;
                        self.events = events;
                        true
                    }
                    Err(err) => {
                        eprintln!("[drive-game] HUD reload failed: {err}");
                        false
                    }
                }
            }
        }
    }

    pub fn tick(&mut self) {
        let _ = self.instance.tick();
        self.poll_events();
    }

    pub fn draw(&mut self) -> BlitDraw {
        let view = self.instance.render().clone();
        let scale = self.scale_factor;
        BlitDraw {
            view,
            x: 0,
            y: 0,
            width: ((self.width as f64) * scale).round().max(1.0) as u32,
            height: ((self.height as f64) * scale).round().max(1.0) as u32,
        }
    }

    fn config(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
        width: u32,
        height: u32,
        scale_factor: f64,
        car: CarState,
        max_speed_mph: f32,
    ) -> InstanceConfig {
        let mut initial_state = car.telemetry_json(mode_label());
        if let Some(obj) = initial_state.as_object_mut() {
            obj.insert("maxSpeed".to_string(), json!(max_speed_mph));
        }
        InstanceConfig {
            width: width.max(1),
            height: height.max(1),
            scale_factor,
            device,
            queue,
            stylesheets: ui_stylesheets(),
            document_scroll: false,
            base_url: Some(format!("file://{}/", ui_dir().display())),
            initial_state: Some(initial_state),
            registered_resources: Vec::new(),
        }
    }
}

impl WinitEventTarget for Hud {
    fn dispatch_mouse(&mut self, x: f32, y: f32, event: MouseEvent) -> TickResult {
        self.instance.dispatch_mouse(x, y, event)
    }

    fn dispatch_key_down(&mut self, event: KeyboardEvent) -> TickResult {
        self.instance.dispatch_key_down(event)
    }

    fn dispatch_key_up(&mut self, event: KeyboardEvent) -> TickResult {
        self.instance.dispatch_key_up(event)
    }

    fn resize(&mut self, width: u32, height: u32) {
        self.resize(width, height, self.scale_factor);
    }
}

fn mount_project(
    project: &SourceProject,
    config: InstanceConfig,
) -> Result<(Instance, mpsc::UnboundedReceiver<Event>), Box<dyn std::error::Error>> {
    #[cfg(debug_assertions)]
    {
        Ok(project.mount_bundle(config, compile_ui_modules()?)?)
    }

    #[cfg(not(debug_assertions))]
    {
        Ok(project.mount_bundle(config, ui_bundle::modules())?)
    }
}

#[cfg(debug_assertions)]
fn compile_ui_modules() -> Result<Vec<VirtualSourceFile>, solite::bundle::BundleError> {
    let generated = solite::bundle::generate(&ui_dir())?;
    Ok(generated
        .modules
        .into_iter()
        .map(|module| VirtualSourceFile {
            path: module.path,
            source: module.source,
        })
        .collect())
}

fn mode_label() -> &'static str {
    #[cfg(debug_assertions)]
    {
        "live reload"
    }

    #[cfg(not(debug_assertions))]
    {
        "bundled"
    }
}

fn ui_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("ui")
}

fn ui_stylesheets() -> Vec<String> {
    #[cfg(debug_assertions)]
    {
        ["styles.css", "Bar.css"]
            .into_iter()
            .map(|name| {
                std::fs::read_to_string(ui_dir().join(name)).unwrap_or_else(|err| {
                    eprintln!("[drive-game] failed to read HUD stylesheet {name}: {err}");
                    String::new()
                })
            })
            .collect()
    }

    #[cfg(not(debug_assertions))]
    {
        vec![
            include_str!("../ui/styles.css").to_string(),
            include_str!("../ui/Bar.css").to_string(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solite::gpu::GpuContext;

    #[test]
    fn mounts_live_ui_headlessly() {
        let gpu = pollster::block_on(GpuContext::headless()).expect("headless gpu");
        let mut hud = Hud::new(
            gpu.device.clone(),
            gpu.queue.clone(),
            640,
            360,
            1.0,
            CarState::default(),
        )
        .expect("mount hud");

        hud.update_state(CarState::default(), 0.0);
        hud.tick();
    }

    #[test]
    fn slider_press_updates_max_speed() {
        use solite::winit::WinitEventTarget;
        use solite::MouseButton;

        let gpu = pollster::block_on(GpuContext::headless()).expect("headless gpu");
        let width = 1280;
        let height = 720;
        let mut hud = Hud::new(
            gpu.device.clone(),
            gpu.queue.clone(),
            width,
            height,
            1.0,
            CarState::default(),
        )
        .expect("mount hud");
        hud.update_state(CarState::default(), 60.0);
        hud.tick();
        let _ = hud.draw(); // force a layout pass so hit-testing has geometry

        // Starts at the default cap.
        assert!((hud.max_speed_mph - DEFAULT_MAX_SPEED_MPH).abs() < 0.01);

        // Press the slider at its vertical midpoint. Card is anchored top-right
        // (right: 26, width: 96); the hit layer spans x∈[1180,1232]. The track
        // runs y∈[98,268], so the midpoint maps to the middle of the 20–50 mph
        // range, i.e. ~35 mph.
        let x = (width as f32) - 26.0 - 48.0;
        let y = (98.0 + 268.0) / 2.0;
        hud.dispatch_mouse(x, y, MouseEvent::Down { x, y, button: MouseButton::Left });
        hud.poll_events();

        assert!(
            (hud.max_speed_mph - 35.0).abs() <= 1.5,
            "expected ~35 mph from midpoint press, got {}",
            hud.max_speed_mph
        );
    }

    #[test]
    fn slider_drag_tracks_pointer_off_the_track() {
        use solite::winit::WinitEventTarget;
        use solite::MouseButton;

        let gpu = pollster::block_on(GpuContext::headless()).expect("headless gpu");
        let width = 1280;
        let height = 720;
        let mut hud = Hud::new(
            gpu.device.clone(),
            gpu.queue.clone(),
            width,
            height,
            1.0,
            CarState::default(),
        )
        .expect("mount hud");
        hud.update_state(CarState::default(), 60.0);
        hud.tick();
        let _ = hud.draw();

        // Press the slider near its top (~58 mph) to begin a drag.
        let sx = (width as f32) - 26.0 - 48.0;
        hud.dispatch_mouse(sx, 108.0, MouseEvent::Down { x: sx, y: 108.0, button: MouseButton::Left });
        hud.poll_events();
        // Let the full-screen capture overlay mount.
        hud.update_state(CarState::default(), 60.0);
        hud.tick();
        let _ = hud.draw();

        // Move the pointer far from the slider — below and to the left. Without
        // the capture overlay this would miss the track and drop the drag; with
        // it, the value follows to the bottom of the range (20 mph).
        hud.dispatch_mouse(220.0, 600.0, MouseEvent::Move { x: 220.0, y: 600.0 });
        hud.poll_events();
        assert!(
            (hud.max_speed_mph - 20.0).abs() <= 1.0,
            "off-track drag should track to the range floor, got {}",
            hud.max_speed_mph
        );

        // Releasing ends the drag: a later move no longer changes the value.
        hud.dispatch_mouse(220.0, 600.0, MouseEvent::Up { x: 220.0, y: 600.0, button: MouseButton::Left });
        hud.poll_events();
        hud.update_state(CarState::default(), 60.0);
        hud.tick();
        let _ = hud.draw();
        hud.dispatch_mouse(sx, 108.0, MouseEvent::Move { x: sx, y: 108.0 });
        hud.poll_events();
        assert!(
            (hud.max_speed_mph - 20.0).abs() <= 1.0,
            "after release, moves should not change the value, got {}",
            hud.max_speed_mph
        );
    }

    // Render the HUD to a texture and return tightly-packed RGBA pixels.
    #[cfg(test)]
    fn render_hud_pixels(
        hud: &mut Hud,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        blit: &solite::gpu::BlitContext,
        car: CarState,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        hud.update_state(car, 60.0);
        hud.tick();
        let draw = hud.draw();

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("hud"),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        blit.encode_blit_to_view(
            device,
            &mut encoder,
            &view,
            width,
            height,
            &[draw],
            wgpu::LoadOp::Clear(wgpu::Color::BLACK),
        );

        let unpadded = width * 4;
        let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        );
        queue.submit([encoder.finish()]);
        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let data = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded as usize]);
        }
        pixels
    }

    #[test]
    fn slider_handle_tracks_value() {
        let gpu = pollster::block_on(GpuContext::headless()).expect("headless gpu");
        let device = gpu.device.clone();
        let queue = gpu.queue.clone();
        let width = 1280u32;
        let height = 720u32;
        let blit = solite::gpu::BlitContext::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut hud = Hud::new(device.clone(), queue.clone(), width, height, 1.0, CarState::default())
            .expect("mount hud");

        hud.max_speed_mph = 50.0;
        let high = render_hud_pixels(&mut hud, &device, &queue, &blit, CarState::default(), width, height);
        hud.max_speed_mph = 20.0;
        let low = render_hud_pixels(&mut hud, &device, &queue, &blit, CarState::default(), width, height);

        // Count differing pixels inside the slider card region. If the fill and
        // handle are reactive, the two renders differ substantially there.
        let mut diff = 0u32;
        for y in 90..260u32 {
            for x in 1150..1265u32 {
                let i = ((y * width + x) * 4) as usize;
                if high[i] != low[i] || high[i + 1] != low[i + 1] || high[i + 2] != low[i + 2] {
                    diff += 1;
                }
            }
        }
        assert!(
            diff > 800,
            "slider did not shift with value: only {diff} differing pixels between max=50 and max=20"
        );
    }

    // Composite the game scene + HUD overlay to a PNG for eyeballing the design.
    // Ignored by default; run with:
    //   cargo test -p solite-drive-game composite -- --ignored --nocapture
    #[test]
    #[ignore]
    fn composite_to_png() {
        use crate::renderer::GameRenderer;
        use solite::gpu::BlitContext;

        let gpu = pollster::block_on(GpuContext::headless()).expect("headless gpu");
        let device = gpu.device.clone();
        let queue = gpu.queue.clone();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let width: u32 = 1280;
        let height: u32 = 720;

        // Reversing car (gear R, brake/reverse lights lit) with the steering
        // turned, to exercise the new physics + HUD state in one shot.
        let car = CarState {
            x: 12.0,
            y: -7.0,
            heading: 0.6,
            speed: -5.5,
            steering: -0.55,
            throttle: 0.0,
            brake: 1.0,
        };

        let mut hud = Hud::new(device.clone(), queue.clone(), width, height, 1.0, car)
            .expect("mount hud");
        hud.update_state(car, 60.0);
        hud.tick();
        let _ = hud.draw(); // layout pass so the slider press hit-tests

        // Drag the max-speed slider down to ~32 mph.
        {
            use solite::winit::WinitEventTarget;
            use solite::MouseButton;
            let x = (width as f32) - 26.0 - 48.0;
            let y = 215.0;
            hud.dispatch_mouse(x, y, MouseEvent::Down { x, y, button: MouseButton::Left });
            hud.poll_events();
        }
        hud.update_state(car, 60.0);
        hud.tick();
        let hud_draw = hud.draw();

        let renderer = GameRenderer::new(&device, format);
        let blit = BlitContext::new(&device, format);

        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("composite"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        renderer.render(&queue, &mut encoder, &view, width, height, car);
        blit.encode_blit_to_view(
            &device,
            &mut encoder,
            &view,
            width,
            height,
            &[hud_draw],
            wgpu::LoadOp::Load,
        );

        let bpp = 4u32;
        let unpadded = width * bpp;
        let padded = unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
            * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let data = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded as usize]);
        }
        let img: image::RgbaImage =
            image::ImageBuffer::from_raw(width, height, pixels).expect("image buffer");
        let out = std::path::Path::new("/tmp/drive_game_composite.png");
        img.save(out).expect("save png");
        println!("wrote {}", out.display());

        // Zoomed crop of the telemetry card for legibility checks.
        let tele = image::imageops::crop_imm(&img, 0, 0, 420, 380).to_image();
        let tele = image::imageops::resize(
            &tele,
            840,
            760,
            image::imageops::FilterType::Nearest,
        );
        tele.save("/tmp/drive_game_tele.png").expect("save tele");

        // Zoomed crop of the controls + minimap (right side).
        let side = image::imageops::crop_imm(&img, 900, 0, 380, 720).to_image();
        let side = image::imageops::resize(
            &side,
            760,
            1440,
            image::imageops::FilterType::Nearest,
        );
        side.save("/tmp/drive_game_side.png").expect("save side");
    }
}
