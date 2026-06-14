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
