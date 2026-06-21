mod constructor;
mod controls;
mod runtime;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use blitz_dom::BaseDocument;
use notify::RecommendedWatcher;
use std::collections::HashMap;
use url::Url;

use crate::events::Event;
use crate::js::{JsContext, JsContextError};
use crate::net::SoliteNetProvider;
use crate::renderer::Painter;
use crate::scrollbar::{ScrollbarColors, ScrollbarDrag, ScrollbarRegion};
use crate::state::StateHandle;

/// Configuration passed to [`Instance::new`].
pub struct InstanceConfig {
    /// Width of the rendered surface in **logical (CSS) pixels**.
    pub width: u32,
    /// Height of the rendered surface in **logical (CSS) pixels**.
    pub height: u32,
    /// Device pixel ratio (HiDPI / Retina scale). The GPU texture is created
    /// at `width * scale_factor` × `height * scale_factor` physical pixels so
    /// the blit onto the OS surface is 1:1 and crisp on HiDPI displays. Pass
    /// `window.scale_factor()` from winit. Defaults to `1.0` (non-HiDPI).
    pub scale_factor: f64,
    #[cfg(feature = "gpu")]
    pub device: Arc<wgpu::Device>,
    #[cfg(feature = "gpu")]
    pub queue: Arc<wgpu::Queue>,
    /// Stylesheets registered before the first paint. Each entry is a CSS
    /// source string. Equivalent to calling [`Instance::add_stylesheet`] after
    /// construction, but applied before the component mounts so initial layout
    /// already accounts for the rules.
    pub stylesheets: Vec<String>,
    /// When `true` the root container becomes a fixed-height scroll container
    /// (`overflow-y: auto`). Content taller than the instance height can be
    /// scrolled with the mouse wheel; the existing scrollbar painter draws and
    /// handles a scrollbar on the right edge, exactly like a browser page.
    /// Defaults to `false`.
    pub document_scroll: bool,
    /// Base URL used to resolve relative `<img src>` and CSS `url(...)`
    /// references. Defaults to the process working directory as a
    /// `file://…/` URL, which makes `<img src="logo.png">` resolve to a file
    /// next to the executable. Set explicitly when loading assets from a
    /// fixed directory regardless of cwd.
    pub base_url: Option<String>,
    /// Optional initial state injected before the first render. The value is
    /// available to `globalThis.state` during module execution so initial-state
    /// reads in component render paths are reliable for AOT bundles and
    /// non-string-rewritable mounts.
    pub initial_state: Option<serde_json::Value>,
    /// Resources registered in the net provider before the component mounts.
    /// Each entry is a `(url, bytes)` pair served from memory when an `<img
    /// src="…">` or CSS `url(…)` references that URL. Use any custom scheme
    /// (e.g. `"solite-image://my-icon"`) to avoid conflicts with `file://`,
    /// `data:`, or `http(s)://`. Must be provided here — not via
    /// [`Instance::register_image_bytes`] — because image fetches are triggered
    /// synchronously during component mount.
    pub registered_resources: Vec<(String, Vec<u8>)>,
}

/// Opaque identifier for a stylesheet registered via
/// [`Instance::add_stylesheet`]. Pass to [`Instance::replace_stylesheet`] or
/// [`Instance::remove_stylesheet`] to update or drop the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StylesheetId(u64);

/// Error returned by [`Instance::register_font_from_path`].
#[derive(Debug)]
pub enum RegisterFontError {
    /// The file extension is not one of `.ttf`, `.otf`, `.woff`, `.woff2`.
    UnknownFormat,
    /// Reading the font file failed.
    Io(std::io::Error),
}

/// Error returned by [`Instance::register_image_from_path`].
#[derive(Debug)]
pub enum RegisterImageError {
    /// Reading the image file failed.
    Io(std::io::Error),
}

#[derive(Debug)]
pub enum InstanceError {
    JsContext(JsContextError),
    #[cfg(feature = "jsx-compiler")]
    CompileComponent(solite_build::CompileError),
    Io(std::io::Error),
    BaseUrl {
        value: String,
        error: String,
    },
    UnsupportedJsxModule {
        path: String,
    },
    MissingVirtualEntrypoint {
        path: String,
    },
}

impl std::fmt::Display for InstanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JsContext(err) => write!(f, "{err}"),
            #[cfg(feature = "jsx-compiler")]
            Self::CompileComponent(err) => write!(f, "{err}"),
            Self::Io(err) => write!(f, "failed to read component source: {err}"),
            Self::BaseUrl { value, error } => {
                write!(f, "invalid base URL `{value}`: {error}")
            }
            Self::UnsupportedJsxModule { path } => {
                write!(
                    f,
                    "JSX/TSX/TS component loading requires the `jsx-compiler` feature: {path}"
                )
            }
            Self::MissingVirtualEntrypoint { path } => {
                write!(f, "missing virtual entrypoint source for `{path}`")
            }
        }
    }
}

impl std::error::Error for InstanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::JsContext(err) => Some(err),
            #[cfg(feature = "jsx-compiler")]
            Self::CompileComponent(err) => Some(err),
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<JsContextError> for InstanceError {
    fn from(value: JsContextError) -> Self {
        Self::JsContext(value)
    }
}

impl From<std::io::Error> for InstanceError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[cfg(feature = "jsx-compiler")]
impl From<solite_build::CompileError> for InstanceError {
    fn from(value: solite_build::CompileError) -> Self {
        Self::CompileComponent(value)
    }
}

impl std::fmt::Display for RegisterFontError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegisterFontError::UnknownFormat => f.write_str(
                "unknown font format: expected file extension `.ttf`, `.otf`, `.woff`, or `.woff2`",
            ),
            RegisterFontError::Io(err) => write!(f, "failed to read font file: {err}"),
        }
    }
}

impl std::error::Error for RegisterFontError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            RegisterFontError::UnknownFormat => None,
            RegisterFontError::Io(err) => Some(err),
        }
    }
}

/// An solite render instance.
///
/// Owns a blitz-dom document, a QuickJS/Solid runtime, and a Vello/wgpu
/// renderer. The host drives it by calling [`tick`] and [`render`].
pub struct Instance {
    width: u32,
    height: u32,
    scale_factor: f64,
    #[cfg(feature = "gpu")]
    device: Arc<wgpu::Device>,
    doc: Rc<RefCell<BaseDocument>>,
    js: JsContext,
    painter: Painter,
    #[cfg(feature = "gpu")]
    texture: wgpu::Texture,
    #[cfg(feature = "gpu")]
    texture_view: wgpu::TextureView,
    state: StateHandle,
    #[allow(dead_code)]
    event_tx: tokio::sync::mpsc::UnboundedSender<Event>,
    container_id: usize,
    document_scroll: bool,
    range_drag_id: Option<usize>,
    hovered_node_id: Option<usize>,
    active_node_id: Option<usize>,
    focused_node_id: Option<usize>,
    needs_paint: bool,
    wake: Arc<tokio::sync::Notify>,
    stylesheets: HashMap<StylesheetId, String>,
    next_stylesheet_id: u64,
    /// Scrollbar regions computed at the last `render()`. Reused by
    /// `dispatch_mouse` for hit-testing scrollbar thumbs / tracks before
    /// falling back to document hit-testing.
    scrollbars: Vec<ScrollbarRegion>,
    /// Spinner geometries for `<input type="number">` elements, computed at
    /// the last `render()`. Reused by `dispatch_mouse`.
    spinners: Vec<crate::spinner::NumberSpinner>,
    /// Currently-dragging scrollbar, if any.
    scrollbar_drag: Option<ScrollbarDrag>,
    /// Touch gesture state: the active finger and any coasting fling. Drives
    /// `dispatch_touch` and the momentum step in `tick`.
    touch: crate::touch::TouchState,
    /// Host-supplied scrollbar theme override. When unset, scrollbar colours
    /// are derived per node from the container's computed `color` property.
    scrollbar_theme: Option<ScrollbarColors>,
    /// NetProvider installed on the document. Held here so
    /// [`Instance::register_font_bytes`] can register synthetic
    /// `solite-font://` URLs against it.
    net_provider: Arc<SoliteNetProvider>,
    /// Base URL used to resolve relative `<img src>` / CSS `url(...)`
    /// paths. Mutated by [`Instance::set_base_url`]. Shared with the JS
    /// bridge.
    base_url: Rc<RefCell<Url>>,
}

/// Watches a component source tree for filesystem changes.
#[derive(Debug)]
pub struct FileWatch {
    pub root: PathBuf,
    changed: std::sync::mpsc::Receiver<PathBuf>,
    #[allow(dead_code)]
    _watcher: RecommendedWatcher,
}

/// Change summary while polling a file watch stream.
///
/// - `bundle_rebuild`: true when a JSX/TS file changed and the JS bundle needs
///   to be recompiled.
/// - `css_reload`: true when a stylesheet changed and can potentially be updated
///   without remounting the instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceChangeSummary {
    pub bundle_rebuild: bool,
    pub css_reload: bool,
    pub css_paths: Vec<PathBuf>,
}

impl FileWatch {
    /// Non-blocking check for the next changed file path.
    pub fn poll(&self) -> Option<PathBuf> {
        self.changed.try_recv().ok()
    }

    /// Drain all pending file changes and classify them for live reload.
    ///
    /// Only files with extensions `js`, `mjs`, `jsx`, `tsx`, `ts`, or `css`
    /// are considered. Others are ignored so unrelated filesystem activity
    /// does not trigger unnecessary rebuild work.
    pub fn poll_source_changes(&self, source_dir: &Path) -> SourceChangeSummary {
        let mut summary = SourceChangeSummary::default();
        while let Some(path) = self.poll() {
            if !path.starts_with(source_dir) {
                continue;
            }

            match path.extension().and_then(|ext| ext.to_str()) {
                Some(ext)
                    if matches!(
                        ext.to_ascii_lowercase().as_str(),
                        "js" | "mjs" | "jsx" | "tsx" | "ts"
                    ) =>
                {
                    summary.bundle_rebuild = true;
                }
                Some(ext) if ext.eq_ignore_ascii_case("css") => {
                    summary.css_reload = true;
                    summary.css_paths.push(path);
                }
                _ => {}
            }
        }
        summary
    }
}

#[cfg(test)]
#[cfg(all(test, feature = "gpu"))]
mod tests;
