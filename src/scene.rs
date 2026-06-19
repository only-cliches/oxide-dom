use crate::{Instance, KeyboardEvent, MouseButton, MouseEvent, TickResult};

/// Stable identifier for a mounted scene surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SurfaceId(usize);

impl SurfaceId {
    pub fn index(self) -> usize {
        self.0
    }
}

/// Logical bounds for an [`Instance`] mounted into a [`Scene`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SurfaceRect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl SurfaceRect {
    pub fn new(x: f32, y: f32, width: f32, height: f32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    pub fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x
            && y >= self.y
            && x < self.x + self.width.max(0.0)
            && y < self.y + self.height.max(0.0)
    }

    pub fn to_local(self, x: f32, y: f32) -> (f32, f32) {
        (x - self.x, y - self.y)
    }
}

/// An [`Instance`] mounted into a [`Scene`].
pub struct SceneSurface<T = ()> {
    pub id: SurfaceId,
    pub rect: SurfaceRect,
    pub instance: Instance,
    pub data: T,
}

/// Multi-instance input router.
///
/// `Scene` owns the global pointer/focus state for a set of independent
/// [`Instance`]s. Hosts dispatch window-level input once, in window
/// coordinates, and the scene performs surface hit testing, coordinate
/// translation, hover leave/enter, focus blur/focus, pointer capture for mouse
/// up, and keyboard routing.
pub struct Scene<T = ()> {
    surfaces: Vec<SceneSurface<T>>,
    hovered: Option<SurfaceId>,
    focused: Option<SurfaceId>,
    pressed: Option<SurfaceId>,
    next_id: usize,
}

impl<T> Default for Scene<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Scene<T> {
    pub fn new() -> Self {
        Self {
            surfaces: Vec::new(),
            hovered: None,
            focused: None,
            pressed: None,
            next_id: 0,
        }
    }

    pub fn add_surface(&mut self, instance: Instance, rect: SurfaceRect, data: T) -> SurfaceId {
        let id = SurfaceId(self.next_id);
        self.next_id += 1;
        self.surfaces.push(SceneSurface {
            id,
            rect,
            instance,
            data,
        });
        id
    }

    pub fn clear(&mut self) {
        self.surfaces.clear();
        self.hovered = None;
        self.focused = None;
        self.pressed = None;
        self.next_id = 0;
    }

    pub fn len(&self) -> usize {
        self.surfaces.len()
    }

    pub fn is_empty(&self) -> bool {
        self.surfaces.is_empty()
    }

    pub fn surfaces(&self) -> &[SceneSurface<T>] {
        &self.surfaces
    }

    pub fn surfaces_mut(&mut self) -> &mut [SceneSurface<T>] {
        &mut self.surfaces
    }

    pub fn hovered_surface(&self) -> Option<SurfaceId> {
        self.hovered
    }

    pub fn focused_surface(&self) -> Option<SurfaceId> {
        self.focused
    }

    pub fn pressed_surface(&self) -> Option<SurfaceId> {
        self.pressed
    }

    pub fn surface_at(&self, x: f32, y: f32) -> Option<SurfaceId> {
        self.surfaces
            .iter()
            .find(|surface| surface.rect.contains(x, y))
            .map(|surface| surface.id)
    }

    pub fn tick(&mut self) -> TickResult {
        let mut result = TickResult::default();
        for surface in &mut self.surfaces {
            result = combine_tick_result(result, surface.instance.tick());
        }
        result
    }

    pub fn dispatch_mouse(&mut self, x: f32, y: f32, event: MouseEvent) -> TickResult {
        let hit = self.surface_at(x, y);
        let mut result = TickResult::default();

        if matches!(event, MouseEvent::Move { .. }) && self.hovered != hit {
            if let Some(previous) = self.hovered {
                result = combine_tick_result(result, self.dispatch_mouse_outside(previous, event));
            }
            self.hovered = hit;
        }

        if let MouseEvent::Down { button, .. } = event {
            self.pressed = hit;
            if button == MouseButton::Left && self.focused != hit {
                if let Some(previous) = self.focused {
                    result = combine_tick_result(
                        result,
                        self.dispatch_mouse_outside(
                            previous,
                            MouseEvent::Down {
                                x: -1.0,
                                y: -1.0,
                                button: MouseButton::Left,
                            },
                        ),
                    );
                }
                self.focused = hit;
            }
        }

        let route_target = match event {
            MouseEvent::Up { .. } => self.pressed.or(hit),
            _ => hit,
        };

        if let Some(target) = route_target {
            result = combine_tick_result(result, self.dispatch_mouse_to(target, x, y, event));
        }

        if matches!(event, MouseEvent::Up { .. }) {
            self.pressed = None;
        }

        result
    }

    pub fn dispatch_key_down(&mut self, event: KeyboardEvent) -> TickResult {
        let Some(target) = self.focused else {
            return TickResult::default();
        };
        let Some(index) = self.surface_index(target) else {
            self.focused = None;
            return TickResult::default();
        };

        self.surfaces[index].instance.dispatch_key_down(event)
    }

    pub fn dispatch_key_up(&mut self, event: KeyboardEvent) -> TickResult {
        let Some(target) = self.focused else {
            return TickResult::default();
        };
        let Some(index) = self.surface_index(target) else {
            self.focused = None;
            return TickResult::default();
        };

        self.surfaces[index].instance.dispatch_key_up(event)
    }

    fn surface_index(&self, id: SurfaceId) -> Option<usize> {
        self.surfaces.iter().position(|surface| surface.id == id)
    }

    fn dispatch_mouse_to(
        &mut self,
        id: SurfaceId,
        global_x: f32,
        global_y: f32,
        event: MouseEvent,
    ) -> TickResult {
        let Some(index) = self.surface_index(id) else {
            return TickResult::default();
        };

        let surface = &mut self.surfaces[index];
        let (local_x, local_y) = surface.rect.to_local(global_x, global_y);
        let local_event = translate_mouse_event(event, local_x, local_y);
        surface
            .instance
            .dispatch_mouse(local_x, local_y, local_event)
    }

    fn dispatch_mouse_outside(&mut self, id: SurfaceId, event: MouseEvent) -> TickResult {
        let Some(index) = self.surface_index(id) else {
            return TickResult::default();
        };

        let outside = translate_mouse_event(event, -1.0, -1.0);
        self.surfaces[index]
            .instance
            .dispatch_mouse(-1.0, -1.0, outside)
    }
}

fn translate_mouse_event(event: MouseEvent, x: f32, y: f32) -> MouseEvent {
    match event {
        MouseEvent::Move { .. } => MouseEvent::Move { x, y },
        MouseEvent::Down { button, .. } => MouseEvent::Down { x, y, button },
        MouseEvent::Up { button, .. } => MouseEvent::Up { x, y, button },
        MouseEvent::Wheel {
            delta_x, delta_y, ..
        } => MouseEvent::Wheel {
            x,
            y,
            delta_x,
            delta_y,
        },
    }
}

fn combine_tick_result(a: TickResult, b: TickResult) -> TickResult {
    TickResult {
        needs_paint: a.needs_paint || b.needs_paint,
        jobs_pending: a.jobs_pending || b.jobs_pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InstanceConfig, StateHandle};
    use serde_json::json;
    use std::sync::Arc;

    const TWO_INPUT_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const root = __sol_createElement("div");

          const first = __sol_createElement("input");
          __sol_setProperty(first, "style", "display:block; width:90px; height:24px;");
          __sol_setProperty(first, "onFocus", () => {
            globalThis.state.focused = "first";
          });

          const second = __sol_createElement("input");
          __sol_setProperty(second, "style", "display:block; width:90px; height:24px;");
          __sol_setProperty(second, "onFocus", () => {
            globalThis.state.focused = "second";
          });

          __sol_insertNode(root, first, null);
          __sol_insertNode(root, second, null);
          return root;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    async fn make_test_device() -> (Arc<wgpu::Device>, Arc<wgpu::Queue>) {
        if cfg!(target_os = "linux") {
            if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
                unsafe {
                    std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
                }
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
            .expect("no adapter available for test");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("solite-scene-test"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                experimental_features: wgpu::ExperimentalFeatures::disabled(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            })
            .await
            .expect("request device");

        (Arc::new(device), Arc::new(queue))
    }

    fn test_device() -> (Arc<wgpu::Device>, Arc<wgpu::Queue>) {
        pollster::block_on(make_test_device())
    }

    fn tab_key() -> KeyboardEvent {
        KeyboardEvent {
            key: "Tab".into(),
            code: "Tab".into(),
            key_code: 9,
            repeat: false,
            shift_key: false,
            ctrl_key: false,
            alt_key: false,
            meta_key: false,
        }
    }

    fn make_input_instance(
        device: Arc<wgpu::Device>,
        queue: Arc<wgpu::Queue>,
    ) -> (Instance, StateHandle) {
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 100,
                height: 80,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
                initial_state: None,
            registered_resources: vec![],
            },
            TWO_INPUT_COMPONENT,
        );
        let _ = instance.render();
        let state = instance.state();
        (instance, state)
    }

    #[test]
    fn tab_only_moves_focus_within_last_clicked_surface() {
        let (device, queue) = test_device();
        let (instance_a, state_a) = make_input_instance(device.clone(), queue.clone());
        let (instance_b, state_b) = make_input_instance(device, queue);

        let mut scene = Scene::new();
        let surface_a = scene.add_surface(instance_a, SurfaceRect::new(0.0, 0.0, 100.0, 80.0), ());
        let surface_b =
            scene.add_surface(instance_b, SurfaceRect::new(150.0, 0.0, 100.0, 80.0), ());

        let _ = scene.dispatch_mouse(
            160.0,
            10.0,
            MouseEvent::Down {
                x: 160.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        assert_eq!(scene.focused_surface(), Some(surface_b));
        assert_eq!(state_a.get("focused"), None);
        assert_eq!(state_b.get("focused"), Some(json!("first")));

        let _ = scene.dispatch_key_down(tab_key());

        assert_eq!(scene.focused_surface(), Some(surface_b));
        assert_eq!(state_a.get("focused"), None);
        assert_eq!(state_b.get("focused"), Some(json!("second")));

        let _ = scene.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        assert_eq!(scene.focused_surface(), Some(surface_a));
        assert_eq!(state_a.get("focused"), Some(json!("first")));

        let _ = scene.dispatch_key_down(tab_key());

        assert_eq!(state_a.get("focused"), Some(json!("second")));
        assert_eq!(state_b.get("focused"), Some(json!("second")));
    }
}
