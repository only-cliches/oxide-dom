#[cfg(feature = "capture")]
pub mod capture;
mod events;
mod focus;
mod fonts;
#[cfg(feature = "gpu")]
pub mod gpu;
mod img;
mod input;
mod instance;
mod js;
mod net;
mod renderer;
mod scene;
mod scrollbar;
mod select;
mod spinner;
mod state;
#[cfg(feature = "winit")]
mod winit_integration;

// The JSX/TS compiler and AOT bundler live in the lightweight `solite-build`
// crate (so it can be a build-dependency without pulling in the renderer). Their
// public API is re-exported here, unchanged, for application code.
pub use events::{Event, KeyboardEvent, MouseButton, MouseEvent};
pub use fonts::FontFormat;
pub use instance::{
    FileWatch, Instance, InstanceConfig, RegisterFontError, RegisterImageError,
    SourceChangeSummary, StylesheetId,
};
pub use js::TickResult;
pub use js::VirtualSourceFile;
pub use scene::{Scene, SceneSurface, SurfaceId, SurfaceRect};
pub use scrollbar::ScrollbarTheme;
#[cfg(feature = "jsx-compiler")]
pub use solite_build::bundle;
#[cfg(feature = "jsx-compiler")]
pub use solite_build::{
    CompileError, compile_component_file, compile_component_source, map_module_specifiers,
};
pub use state::StateHandle;

#[cfg(feature = "winit")]
pub mod winit {
    //! winit integration. Available when the `winit` feature is enabled.
    pub use crate::winit_integration::{
        WinitBridge, WinitEventTarget, WinitForward, WinitPollScheduler, key_to_string,
    };
}

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
