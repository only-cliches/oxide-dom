//! winit integration: forward winit `WindowEvent`s into an [`Instance`].
//!
//! Enabled with the `winit` cargo feature. Hosts that drive an instance from
//! a `winit::application::ApplicationHandler` can replace ~80 lines of
//! per-example boilerplate (keyboard translation, mouse-button translation,
//! cursor tracking, modifier tracking) with:
//!
//! ```ignore
//! use solite::winit::WinitBridge;
//!
//! struct App { instance: Instance, bridge: WinitBridge, /* ... */ }
//!
//! impl ApplicationHandler for App {
//!     fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
//!         let r = self.bridge.handle(&mut self.instance, &ev);
//!         if r.close_requested { el.exit(); }
//!         if r.needs_redraw { /* call window.request_redraw() */ }
//!     }
//! }
//! ```
//!
//! Behaviors handled:
//! - HiDPI: when the host sets the scale factor (via
//!   [`WinitBridge::set_scale_factor`] / [`with_scale_factor`], or
//!   automatically on `ScaleFactorChanged`), the bridge converts physical
//!   pointer/touch positions and `Resized` sizes into the logical pixels the
//!   instance uses. Hosts no longer hand-roll `position / scale`; they can also
//!   call [`WinitBridge::to_logical_size`] / [`scale_factor`] for instance
//!   creation. Defaults to `1.0` (no scaling) so existing 1:1 hosts are
//!   unaffected.
//! - Modifier state (`ModifiersChanged`) is tracked internally so the host
//!   doesn't have to thread it through every key event.
//! - Cursor position (`CursorMoved`) is tracked so mouse-button and wheel
//!   events know where they happened.
//! - Logical key + text → browser `KeyboardEvent.key` translation runs
//!   through [`key_to_string`], which correctly returns `"Tab"` (not `"\t"`)
//!   for named keys.
//! - `MouseInput` is split into `MouseEvent::Down` / `Up` for the focused
//!   button.
//! - `MouseWheel` is forwarded with both line- and pixel-delta units
//!   converted to pixels (with a default 16-px line height).
//! - `CursorLeft` clears hover state.
//! - `Resized` resizes the instance.

use winit::dpi::PhysicalPosition;
use winit::event::{ElementState, KeyEvent, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::keyboard::{Key, ModifiersState, NamedKey, PhysicalKey};

use crate::events::{KeyboardEvent, MouseButton, MouseEvent, TouchEvent, TouchPhase};
use crate::instance::Instance;
use crate::js::TickResult;
use crate::scene::Scene;
use std::time::{Duration, Instant};

/// Anything that can absorb mouse + keyboard events from the bridge. Both
/// [`Instance`] (single render target) and [`Scene`] (multi-surface
/// composition) implement this so the same bridge can drive either.
pub trait WinitEventTarget {
    fn dispatch_mouse(&mut self, x: f32, y: f32, event: MouseEvent) -> TickResult;
    fn dispatch_key_down(&mut self, event: KeyboardEvent) -> TickResult;
    fn dispatch_key_up(&mut self, event: KeyboardEvent) -> TickResult;
    /// Forward a single-finger touch event. The default implementation maps
    /// touch onto the mouse pipeline (Started→Down, Moved→Move, Ended/Cancelled
    /// →Up) so any target gets basic touch-as-pointer for free. [`Instance`]
    /// overrides this with full tap/pan/momentum gesture handling.
    fn dispatch_touch(&mut self, event: TouchEvent) -> TickResult {
        let TouchEvent { x, y, phase, .. } = event;
        match phase {
            TouchPhase::Started => self.dispatch_mouse(
                x,
                y,
                MouseEvent::Down {
                    x,
                    y,
                    button: MouseButton::Left,
                },
            ),
            TouchPhase::Moved => self.dispatch_mouse(x, y, MouseEvent::Move { x, y }),
            TouchPhase::Ended | TouchPhase::Cancelled => self.dispatch_mouse(
                x,
                y,
                MouseEvent::Up {
                    x,
                    y,
                    button: MouseButton::Left,
                },
            ),
        }
    }
    /// Optional: resize the underlying render target. The default does
    /// nothing — implementations that need bespoke layout (like multi-
    /// surface scenes) should handle Resized themselves.
    fn resize(&mut self, _width: u32, _height: u32) {}
}

impl WinitEventTarget for Instance {
    fn dispatch_mouse(&mut self, x: f32, y: f32, event: MouseEvent) -> TickResult {
        Instance::dispatch_mouse(self, x, y, event)
    }
    fn dispatch_key_down(&mut self, event: KeyboardEvent) -> TickResult {
        Instance::dispatch_key_down(self, event)
    }
    fn dispatch_key_up(&mut self, event: KeyboardEvent) -> TickResult {
        Instance::dispatch_key_up(self, event)
    }
    fn dispatch_touch(&mut self, event: TouchEvent) -> TickResult {
        Instance::dispatch_touch(self, event)
    }
    fn resize(&mut self, width: u32, height: u32) {
        Instance::resize(self, width, height);
    }
}

impl<T> WinitEventTarget for Scene<T> {
    fn dispatch_mouse(&mut self, x: f32, y: f32, event: MouseEvent) -> TickResult {
        Scene::dispatch_mouse(self, x, y, event)
    }
    fn dispatch_key_down(&mut self, event: KeyboardEvent) -> TickResult {
        Scene::dispatch_key_down(self, event)
    }
    fn dispatch_key_up(&mut self, event: KeyboardEvent) -> TickResult {
        Scene::dispatch_key_up(self, event)
    }
    // No `resize` — `Scene` lays out per-surface; the host owns that policy.
}

/// Approximate pixel height of a single line of text. Used to convert
/// `MouseScrollDelta::LineDelta` into pixel deltas for instances that don't
/// distinguish the two.
const LINE_HEIGHT_PX: f32 = 16.0;

/// Stateful bridge that owns modifier + cursor state and forwards winit
/// `WindowEvent`s into an [`Instance`].
///
/// Cheap to construct; one bridge per window. Not `Send`/`Sync` because it
/// holds references to the same instance that lives on the render thread.
pub struct WinitBridge {
    /// Currently-held modifier keys. Updated on `ModifiersChanged`.
    modifiers: ModifiersState,
    /// Last known cursor position in **logical** (scale-divided) coordinates —
    /// the same space the instance lays out in.
    cursor: (f32, f32),
    /// Device pixel ratio. Pointer/touch positions and `Resized` sizes arrive
    /// from winit in physical pixels; the bridge divides by this to hand the
    /// instance logical coordinates. Defaults to `1.0` (no scaling), so hosts
    /// that don't call [`set_scale_factor`](Self::set_scale_factor) keep the
    /// previous 1:1 behavior. Updated automatically on `ScaleFactorChanged`.
    scale: f64,
}

impl Default for WinitBridge {
    fn default() -> Self {
        Self {
            modifiers: ModifiersState::empty(),
            cursor: (0.0, 0.0),
            scale: 1.0,
        }
    }
}

/// Outcome of forwarding a single winit event.
#[derive(Debug, Clone, Copy, Default)]
pub struct WinitForward {
    /// Instance reported that the next paint will change something. Hosts
    /// usually translate this into `Window::request_redraw()`.
    pub needs_redraw: bool,
    /// JS still has queued microtasks. Hosts should schedule another tick
    /// soon (typically same as `request_redraw`).
    pub jobs_pending: bool,
    /// `CloseRequested` was observed. Hosts usually translate this into
    /// `EventLoop::exit()`.
    pub close_requested: bool,
}

impl WinitForward {
    fn from_tick(tick: TickResult) -> Self {
        Self {
            needs_redraw: tick.needs_paint,
            jobs_pending: tick.jobs_pending,
            close_requested: false,
        }
    }
}

/// Reusable scheduler for wake-ups in `ApplicationHandler::about_to_wait`.
///
/// Winit apps that depend on external non-event sources (file watchers,
/// background async work, debuggers) can use this helper to wake the event
/// loop at a fixed cadence while still honoring cursor-blink deadlines.
#[derive(Debug, Clone, Copy)]
pub struct WinitPollScheduler {
    interval: Duration,
    next_poll: Instant,
    enabled: bool,
}

impl WinitPollScheduler {
    pub fn with_interval(interval: Duration) -> Self {
        Self {
            interval,
            next_poll: Instant::now() + interval,
            enabled: false,
        }
    }

    pub fn with_default_interval() -> Self {
        Self::with_interval(Duration::from_millis(50))
    }

    /// Enable/disable the periodic polling path. Disabled scheduler does not
    /// influence control-flow or polling decisions.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Return true when enough idle time has elapsed since the last poll.
    /// Call this from `about_to_wait`, and on a `true` return value run your
    /// non-event-driven poll step (e.g. `FileWatch::poll`).
    pub fn should_poll(&mut self) -> bool {
        if !self.enabled {
            return false;
        }

        let now = Instant::now();
        if now < self.next_poll {
            return false;
        }
        self.next_poll = now + self.interval;
        true
    }

    /// Set the event-loop control flow so that we wake at the earlier of the
    /// next blink deadline and the next scheduled poll tick.
    ///
    /// If the scheduler is disabled and no blink deadline exists, this falls
    /// back to `ControlFlow::Wait`.
    pub fn set_next_wakeup(&mut self, event_loop: &ActiveEventLoop, next_blink: Option<Instant>) {
        if !self.enabled {
            event_loop.set_control_flow(match next_blink {
                Some(deadline) => ControlFlow::WaitUntil(deadline),
                None => ControlFlow::Wait,
            });
            return;
        }

        let deadline = match next_blink {
            Some(blink) => self.next_poll.min(blink),
            None => self.next_poll,
        };
        event_loop.set_control_flow(ControlFlow::WaitUntil(deadline));
    }
}

impl WinitBridge {
    /// Fresh bridge with empty modifier state and a `(0, 0)` cursor.
    pub fn new() -> Self {
        Self::default()
    }

    /// Reflect the host's current modifier mask. Hosts that get
    /// modifier state from somewhere other than `ModifiersChanged`
    /// (touch-bar, virtual keyboard) can set it directly.
    pub fn set_modifiers(&mut self, modifiers: ModifiersState) {
        self.modifiers = modifiers;
    }

    /// Read the currently-held modifier mask.
    pub fn modifiers(&self) -> ModifiersState {
        self.modifiers
    }

    /// Builder form of [`set_scale_factor`](Self::set_scale_factor).
    pub fn with_scale_factor(mut self, scale: f64) -> Self {
        self.set_scale_factor(scale);
        self
    }

    /// Set the device pixel ratio (`window.scale_factor()`). The bridge then
    /// converts incoming physical pointer/touch positions and `Resized` sizes
    /// into the logical pixels the instance uses, so hosts no longer hand-roll
    /// `position / scale`. Non-finite or non-positive values are ignored
    /// (scale stays unchanged). Call this once after creating the window and
    /// whenever the scale changes (the bridge also tracks `ScaleFactorChanged`
    /// automatically when those events are forwarded to [`handle`](Self::handle)).
    pub fn set_scale_factor(&mut self, scale: f64) {
        if scale.is_finite() && scale > 0.0 {
            self.scale = scale;
        }
    }

    /// Current device pixel ratio. Pass this to
    /// [`InstanceConfig::scale_factor`](crate::InstanceConfig) when creating an
    /// instance so its texture is allocated at the right physical resolution.
    pub fn scale_factor(&self) -> f64 {
        self.scale
    }

    /// Convert a physical surface size (e.g. from `WindowEvent::Resized` or
    /// `gpu.config`) into the logical size to pass to
    /// [`Instance::resize`](crate::Instance::resize). Each axis is clamped to a
    /// minimum of 1 and rounded to the nearest pixel.
    pub fn to_logical_size(&self, physical_width: u32, physical_height: u32) -> (u32, u32) {
        (
            ((physical_width as f64) / self.scale).max(1.0).round() as u32,
            ((physical_height as f64) / self.scale).max(1.0).round() as u32,
        )
    }

    /// Convert a physical window-local position into logical (CSS) pixels.
    pub fn to_logical_pos(&self, x: f64, y: f64) -> (f32, f32) {
        ((x / self.scale) as f32, (y / self.scale) as f32)
    }

    /// Read the last known cursor position.
    pub fn cursor(&self) -> (f32, f32) {
        self.cursor
    }

    /// Forward one winit `WindowEvent` into `target`. See module docs.
    pub fn handle(
        &mut self,
        target: &mut impl WinitEventTarget,
        event: &WindowEvent,
    ) -> WinitForward {
        match event {
            WindowEvent::ModifiersChanged(mods) => {
                self.modifiers = mods.state();
                WinitForward::default()
            }
            WindowEvent::CloseRequested => WinitForward {
                close_requested: true,
                ..WinitForward::default()
            },
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.set_scale_factor(*scale_factor);
                WinitForward {
                    needs_redraw: true,
                    ..WinitForward::default()
                }
            }
            WindowEvent::Resized(size) => {
                // The instance lays out in logical pixels; convert before
                // resizing. Hosts that also own a GPU surface still configure
                // it at the physical `size` themselves.
                let (logical_w, logical_h) = self.to_logical_size(size.width, size.height);
                target.resize(logical_w, logical_h);
                WinitForward {
                    needs_redraw: true,
                    ..WinitForward::default()
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = self.to_logical_pos(position.x, position.y);
                let r = target.dispatch_mouse(
                    self.cursor.0,
                    self.cursor.1,
                    MouseEvent::Move {
                        x: self.cursor.0,
                        y: self.cursor.1,
                    },
                );
                WinitForward::from_tick(r)
            }
            WindowEvent::CursorLeft { .. } => {
                // Move pointer offscreen so blitz clears `:hover`.
                let r = target.dispatch_mouse(-1.0, -1.0, MouseEvent::Move { x: -1.0, y: -1.0 });
                WinitForward::from_tick(r)
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(btn) = winit_to_solite_button(*button) else {
                    return WinitForward::default();
                };
                let (x, y) = self.cursor;
                let mouse_event = match state {
                    ElementState::Pressed => MouseEvent::Down { x, y, button: btn },
                    ElementState::Released => MouseEvent::Up { x, y, button: btn },
                };
                let r = target.dispatch_mouse(x, y, mouse_event);
                WinitForward::from_tick(r)
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x * LINE_HEIGHT_PX, y * LINE_HEIGHT_PX),
                    MouseScrollDelta::PixelDelta(PhysicalPosition { x, y }) => {
                        (*x as f32, *y as f32)
                    }
                };
                let (x, y) = self.cursor;
                let r = target.dispatch_mouse(
                    x,
                    y,
                    MouseEvent::Wheel {
                        x,
                        y,
                        delta_x: dx,
                        delta_y: dy,
                    },
                );
                WinitForward::from_tick(r)
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_key_event(target, event),
            WindowEvent::Touch(touch) => {
                let (x, y) = self.to_logical_pos(touch.location.x, touch.location.y);
                self.cursor = (x, y);
                let phase = match touch.phase {
                    winit::event::TouchPhase::Started => TouchPhase::Started,
                    winit::event::TouchPhase::Moved => TouchPhase::Moved,
                    winit::event::TouchPhase::Ended => TouchPhase::Ended,
                    winit::event::TouchPhase::Cancelled => TouchPhase::Cancelled,
                };
                let r = target.dispatch_touch(TouchEvent {
                    id: touch.id,
                    phase,
                    x,
                    y,
                });
                WinitForward::from_tick(r)
            }
            _ => WinitForward::default(),
        }
    }

    fn handle_key_event(
        &mut self,
        target: &mut impl WinitEventTarget,
        event: &KeyEvent,
    ) -> WinitForward {
        let key = key_to_string(&event.logical_key, event.text.as_deref());
        let code = match &event.physical_key {
            PhysicalKey::Code(code) => format!("{code:?}"),
            _ => String::new(),
        };
        let kb_event = KeyboardEvent {
            key,
            code,
            key_code: 0,
            repeat: event.repeat,
            shift_key: self.modifiers.shift_key(),
            ctrl_key: self.modifiers.control_key(),
            alt_key: self.modifiers.alt_key(),
            meta_key: self.modifiers.super_key(),
        };
        let r = match event.state {
            ElementState::Pressed => target.dispatch_key_down(kb_event),
            ElementState::Released => target.dispatch_key_up(kb_event),
        };
        WinitForward::from_tick(r)
    }
}

/// Screen-reader bridge: a thin wrapper over [`accesskit_winit::Adapter`] that
/// pushes solite's enriched accessibility tree to the OS and routes assistive-
/// technology action requests back into an [`Instance`].
///
/// The host owns it (the adapter needs the winit `Window` + an event-loop
/// proxy). Construct it **before the window is first shown** (create the window
/// invisible, build the adapter, then make it visible). Typical wiring:
///
/// ```ignore
/// // A user-event type the adapter can post into:
/// enum UserEvent { A11y(accesskit_winit::Event) }
/// impl From<accesskit_winit::Event> for UserEvent {
///     fn from(e: accesskit_winit::Event) -> Self { UserEvent::A11y(e) }
/// }
///
/// // On window creation:
/// let mut a11y = A11yAdapter::new(event_loop, &window, proxy.clone());
///
/// // In window_event, before handling: feed winit events to the adapter,
/// // and after a tick that paints: a11y.update(&instance).
/// a11y.process_event(&window, &event);
///
/// // In user_event: route AccessKit requests into the instance.
/// if let UserEvent::A11y(e) = user_event {
///     if let Some(_tick) = a11y.handle_window_event(&mut instance, &e.window_event) {
///         window.request_redraw();
///     }
/// }
/// ```
#[cfg(feature = "a11y")]
pub struct A11yAdapter {
    adapter: accesskit_winit::Adapter,
}

#[cfg(feature = "a11y")]
impl A11yAdapter {
    /// Create the adapter for `window`, posting AccessKit events into the
    /// winit event loop via `proxy`. Must be called before the window is shown.
    pub fn new<T>(
        event_loop: &ActiveEventLoop,
        window: &winit::window::Window,
        proxy: winit::event_loop::EventLoopProxy<T>,
    ) -> Self
    where
        T: From<accesskit_winit::Event> + Send + 'static,
    {
        Self {
            adapter: accesskit_winit::Adapter::with_event_loop_proxy(event_loop, window, proxy),
        }
    }

    /// Forward a winit window event to the adapter. Call for every window
    /// event, before the application handles it.
    pub fn process_event(&mut self, window: &winit::window::Window, event: &WindowEvent) {
        self.adapter.process_event(window, event);
    }

    /// Push the instance's current accessibility tree if an AT is attached.
    /// Cheap when no screen reader is active (the closure isn't called).
    pub fn update(&mut self, instance: &Instance) {
        self.adapter
            .update_if_active(|| instance.accessibility_tree());
    }

    /// Handle one [`accesskit_winit::WindowEvent`] (delivered via the user-event
    /// channel). Applies action requests to `instance`, returning the resulting
    /// [`TickResult`] when one was performed (so the host can request a redraw).
    pub fn handle_window_event(
        &mut self,
        instance: &mut Instance,
        event: &accesskit_winit::WindowEvent,
    ) -> Option<TickResult> {
        match event {
            accesskit_winit::WindowEvent::InitialTreeRequested => {
                self.adapter
                    .update_if_active(|| instance.accessibility_tree());
                None
            }
            accesskit_winit::WindowEvent::ActionRequested(request) => {
                let tick = instance.perform_accessibility_action(request);
                self.adapter
                    .update_if_active(|| instance.accessibility_tree());
                Some(tick)
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => None,
        }
    }
}

/// Translate winit's `Key` (and optional `text`) into the string value
/// expected in [`KeyboardEvent::key`].
///
/// Named keys (Tab, Enter, Escape, Backspace, Delete, ArrowLeft, …) take
/// precedence over winit's `text` field because winit fills `text` with the
/// control character for them (`"\t"`, `"\r"`, `"\x1b"`, …). The browser
/// `KeyboardEvent.key` contract (which `Instance::dispatch_key` matches
/// against) wants the WHATWG name instead.
pub fn key_to_string(logical_key: &Key, text: Option<&str>) -> String {
    if let Key::Named(named) = logical_key {
        return match named {
            // The browser spec emits Space as the literal " ".
            NamedKey::Space => " ".to_string(),
            _ => format!("{named:?}"),
        };
    }
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        return text.to_string();
    }
    match logical_key {
        Key::Character(text) => text.to_string(),
        Key::Named(named) => format!("{named:?}"),
        Key::Unidentified(_) => "Unidentified".to_string(),
        Key::Dead(Some(c)) => c.to_string(),
        Key::Dead(None) => String::new(),
    }
}

fn winit_to_solite_button(button: winit::event::MouseButton) -> Option<MouseButton> {
    match button {
        winit::event::MouseButton::Left => Some(MouseButton::Left),
        winit::event::MouseButton::Right => Some(MouseButton::Right),
        winit::event::MouseButton::Middle => Some(MouseButton::Middle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::keyboard::SmolStr;

    #[test]
    fn default_bridge_is_scale_1_and_identity() {
        let bridge = WinitBridge::new();
        assert_eq!(bridge.scale_factor(), 1.0);
        assert_eq!(bridge.to_logical_pos(40.0, 80.0), (40.0, 80.0));
        assert_eq!(bridge.to_logical_size(640, 480), (640, 480));
    }

    #[test]
    fn scale_factor_divides_positions_and_sizes() {
        let bridge = WinitBridge::new().with_scale_factor(2.0);
        assert_eq!(bridge.to_logical_pos(40.0, 80.0), (20.0, 40.0));
        // 1281 physical / 2 = 640.5 → rounds to 641.
        assert_eq!(bridge.to_logical_size(1280, 1281), (640, 641));
    }

    #[test]
    fn set_scale_factor_ignores_invalid_values() {
        let mut bridge = WinitBridge::new().with_scale_factor(1.5);
        bridge.set_scale_factor(0.0);
        bridge.set_scale_factor(-2.0);
        bridge.set_scale_factor(f64::NAN);
        assert_eq!(
            bridge.scale_factor(),
            1.5,
            "bad scales leave the value unchanged"
        );
    }

    #[test]
    fn to_logical_size_clamps_to_minimum_one() {
        let bridge = WinitBridge::new().with_scale_factor(4.0);
        assert_eq!(bridge.to_logical_size(0, 2), (1, 1));
    }

    #[test]
    fn key_to_string_returns_whatwg_name_for_named_keys() {
        // The bug we hit in the kitchen sink: winit fills `text` with a
        // control character for named keys. The translator must ignore it
        // for named variants, otherwise Tab navigation silently fails.
        assert_eq!(key_to_string(&Key::Named(NamedKey::Tab), Some("\t")), "Tab");
        assert_eq!(
            key_to_string(&Key::Named(NamedKey::Enter), Some("\r")),
            "Enter"
        );
        assert_eq!(
            key_to_string(&Key::Named(NamedKey::Escape), Some("\x1b")),
            "Escape"
        );
        assert_eq!(
            key_to_string(&Key::Named(NamedKey::Backspace), Some("\u{8}")),
            "Backspace"
        );
        assert_eq!(
            key_to_string(&Key::Named(NamedKey::ArrowLeft), None),
            "ArrowLeft"
        );
    }

    #[test]
    fn key_to_string_returns_literal_space_for_space_key() {
        assert_eq!(key_to_string(&Key::Named(NamedKey::Space), Some(" ")), " ");
    }

    #[test]
    fn key_to_string_prefers_text_for_character_keys() {
        // Letter keys should use text so shifted (`"A"`) and IME-composed
        // variants come through.
        let key = Key::Character(SmolStr::new_inline("a"));
        assert_eq!(key_to_string(&key, Some("A")), "A");
    }

    #[test]
    fn winit_button_left_maps_to_solite_left() {
        assert_eq!(
            winit_to_solite_button(winit::event::MouseButton::Left),
            Some(MouseButton::Left)
        );
        assert_eq!(
            winit_to_solite_button(winit::event::MouseButton::Middle),
            Some(MouseButton::Middle)
        );
        assert!(
            winit_to_solite_button(winit::event::MouseButton::Other(7)).is_none(),
            "extra buttons fall back to None"
        );
    }
}
