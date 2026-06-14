#[cfg(feature = "jsx-compiler")]
mod compiler;
mod events;
mod input;
mod instance;
mod js;
mod renderer;
mod scene;
mod scrollbar;
mod state;

#[cfg(feature = "jsx-compiler")]
pub use compiler::{CompileError, compile_component_file, compile_component_source};
pub use events::{Event, KeyboardEvent, MouseButton, MouseEvent};
pub use instance::{FileWatch, Instance, InstanceConfig, StylesheetId};
pub use js::TickResult;
pub use scene::{Scene, SceneSurface, SurfaceId, SurfaceRect};
pub use scrollbar::ScrollbarTheme;
pub use state::StateHandle;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Verifies that the state handle works independently of a full Instance.
    #[test]
    fn state_handle_standalone() {
        let h = StateHandle::new(json!({"counter": 0}));
        h.set("counter", json!(7));
        assert_eq!(h.get("counter"), Some(json!(7)));

        let h2 = h.clone();
        h2.set("counter", json!(42));
        assert_eq!(h.get("counter"), Some(json!(42)));
    }

    /// Verifies that drain_patches is idempotent.
    #[test]
    fn state_patches_drained_once() {
        let h = StateHandle::new(json!({}));
        h.set("x", json!(1));
        let p = h.drain_patches();
        assert_eq!(p.len(), 1);
        assert!(h.drain_patches().is_empty());
    }
}
