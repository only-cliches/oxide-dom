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

use crate::events::{KeyboardEvent, MouseButton, MouseEvent};
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
#[derive(Default)]
pub struct WinitBridge {
    /// Currently-held modifier keys. Updated on `ModifiersChanged`.
    modifiers: ModifiersState,
    /// Last known cursor position in window-local coordinates.
    cursor: (f32, f32),
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
            WindowEvent::Resized(size) => {
                target.resize(size.width.max(1), size.height.max(1));
                WinitForward {
                    needs_redraw: true,
                    ..WinitForward::default()
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = (position.x as f32, position.y as f32);
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
