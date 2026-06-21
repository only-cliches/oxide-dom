/// Button pressed during a mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// Mouse event forwarded from the host into [`crate::Instance::dispatch_mouse`].
#[derive(Debug, Clone, Copy)]
pub enum MouseEvent {
    Move {
        x: f32,
        y: f32,
    },
    Down {
        x: f32,
        y: f32,
        button: MouseButton,
    },
    Up {
        x: f32,
        y: f32,
        button: MouseButton,
    },
    Wheel {
        x: f32,
        y: f32,
        delta_x: f32,
        delta_y: f32,
    },
}

/// Lifecycle phase of a touch point, mirroring `winit::event::TouchPhase` and
/// the browser `TouchEvent` model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchPhase {
    /// Finger first contacted the surface.
    Started,
    /// Finger moved while in contact.
    Moved,
    /// Finger lifted off cleanly.
    Ended,
    /// Gesture was interrupted (e.g. palm rejection, window focus loss).
    Cancelled,
}

/// A single touch point forwarded from the host into
/// [`crate::Instance::dispatch_touch`]. solite tracks one active finger at a
/// time (the first `id` seen); additional simultaneous fingers are ignored
/// until multi-touch gestures are implemented.
#[derive(Debug, Clone, Copy)]
pub struct TouchEvent {
    /// Stable identifier for this finger across its `Started`→`Ended` life.
    pub id: u64,
    /// Lifecycle phase.
    pub phase: TouchPhase,
    /// Position in window-local (client) pixels, same space as
    /// [`MouseEvent`] coordinates.
    pub x: f32,
    pub y: f32,
}

/// Event emitted from JS via `sendEvent(name, payload)` and received on the
/// channel returned from [`crate::Instance::new`].
#[derive(Debug, Clone)]
pub struct Event {
    pub name: String,
    pub payload: serde_json::Value,
}

/// Keyboard event data forwarded from host into [`crate::Instance::dispatch_key`].
#[derive(Debug, Clone)]
pub struct KeyboardEvent {
    /// Human-readable key value (eg. `"a"`, `"Enter"`, `"Backspace"`).
    pub key: String,
    /// Physical key code (`"KeyA"`, `"Enter"`, etc.).
    pub code: String,
    /// Numeric virtual key code compatibility field.
    pub key_code: u32,
    /// Whether this key event is auto-repeated.
    pub repeat: bool,
    /// Modifier snapshot.
    pub shift_key: bool,
    pub ctrl_key: bool,
    pub alt_key: bool,
    pub meta_key: bool,
}
