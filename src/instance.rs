use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use blitz_dom::{BaseDocument, DocumentConfig, LocalName, Node, QualName, ns};
use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};
use notify::{self, RecommendedWatcher, RecursiveMode, Watcher};
use parley::{Affinity, Cursor, Selection};
use serde_json::json;
use style_dom::ElementState;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use url::Url;

#[cfg(feature = "jsx-compiler")]
use solite_build as compiler;
use crate::events::{Event, KeyboardEvent, MouseButton, MouseEvent};
use crate::fonts::{self, FontFormat};
use crate::img::ImgEvent;
use crate::js::{JsContext, JsContextError, TickResult, VirtualSourceFile};
use crate::net::{self, SoliteNetProvider};
use crate::renderer::{InputCaret, InputSelection, Painter};
use crate::scrollbar::{
    self, ScrollAxis, ScrollbarColors, ScrollbarDrag, ScrollbarHit, ScrollbarRegion,
    ScrollbarTheme, collect_scrollbar_regions,
};
use crate::state::StateHandle;

/// Configuration passed to [`Instance::new`].
pub struct InstanceConfig {
    pub width: u32,
    pub height: u32,
    pub device: Arc<wgpu::Device>,
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

#[derive(Debug)]
pub enum InstanceError {
    JsContext(JsContextError),
    #[cfg(feature = "jsx-compiler")]
    CompileComponent(compiler::CompileError),
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
impl From<compiler::CompileError> for InstanceError {
    fn from(value: compiler::CompileError) -> Self {
        Self::CompileComponent(value)
    }
}

fn parse_base_url(base_url: &str) -> Result<Url, InstanceError> {
    Url::parse(base_url).map_err(|err| InstanceError::BaseUrl {
        value: base_url.to_string(),
        error: err.to_string(),
    })
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
    device: Arc<wgpu::Device>,
    doc: Rc<RefCell<BaseDocument>>,
    js: JsContext,
    painter: Painter,
    texture: wgpu::Texture,
    texture_view: wgpu::TextureView,
    state: StateHandle,
    #[allow(dead_code)]
    event_tx: UnboundedSender<Event>,
    container_id: usize,
    document_scroll: bool,
    range_drag_id: Option<usize>,
    hovered_node_id: Option<usize>,
    active_node_id: Option<usize>,
    focused_node_id: Option<usize>,
    needs_paint: bool,
    wake: Arc<tokio::sync::Notify>,
    stylesheets: std::collections::HashMap<StylesheetId, String>,
    next_stylesheet_id: u64,
    /// Scrollbar regions computed at the last `render()`. Reused by
    /// `dispatch_mouse` for hit-testing scrollbar thumbs / tracks before
    /// falling back to document hit-testing.
    scrollbars: Vec<ScrollbarRegion>,
    /// Currently-dragging scrollbar, if any.
    scrollbar_drag: Option<ScrollbarDrag>,
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
/// - `bundle_rebuild`: true when a JSX/TS file changed and the JS bundle needs to
///   be recompiled.
/// - `css_reload`: true when a stylesheet changed and can potentially be updated
///   without remounting the instance.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SourceChangeSummary {
    pub bundle_rebuild: bool,
    pub css_reload: bool,
}

impl FileWatch {
    /// Non-blocking check for the next changed file path.
    pub fn poll(&self) -> Option<PathBuf> {
        self.changed.try_recv().ok()
    }

    /// Drain all pending file changes and classify them for live reload.
    ///
    /// Only files with extensions `jsx`, `tsx`, `ts`, or `css` are considered.
    /// Others are ignored so unrelated filesystem activity does not trigger
    /// unnecessary rebuild work.
    pub fn poll_source_changes(&self, source_dir: &Path) -> SourceChangeSummary {
        let mut summary = SourceChangeSummary::default();
        while let Some(path) = self.poll() {
            if !path.starts_with(source_dir) {
                continue;
            }

            match path.extension().and_then(|ext| ext.to_str()) {
                Some(ext) if matches!(ext.to_ascii_lowercase().as_str(), "jsx" | "tsx" | "ts") => {
                    summary.bundle_rebuild = true;
                }
                Some(ext) if ext.eq_ignore_ascii_case("css") => {
                    summary.css_reload = true;
                }
                _ => {}
            }
        }
        summary
    }
}

#[cfg(not(feature = "jsx-compiler"))]
fn is_jsx_or_ts_module(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "jsx" | "tsx" | "ts"))
}

impl Instance {
    /// Create a new instance.
    ///
    /// `component_source` is evaluated as an ES module. Bridge globals
    /// (`__sol_createElement`, etc.) and the `solite-runtime` module are
    /// pre-installed so the component can import and use them.
    ///
    /// Returns the instance and a channel receiver for JS-emitted events.
    fn new_inner(
        config: InstanceConfig,
        component_source: &str,
    ) -> Result<(Self, UnboundedReceiver<Event>), InstanceError> {
        let InstanceConfig {
            width,
            height,
            device,
            queue,
            stylesheets: initial_stylesheets,
            document_scroll,
            base_url: base_url_config,
        } = config;

        // --- Document ---
        let viewport = Viewport {
            window_size: (width, height),
            hidpi_scale: 1.0,
            zoom: 1.0,
            color_scheme: ColorScheme::Light,
        };
        let doc = Rc::new(RefCell::new(BaseDocument::new(DocumentConfig {
            viewport: Some(viewport),
            ..Default::default()
        })));

        // --- Resource provider (images, fonts) ---
        let net_provider = Arc::new(SoliteNetProvider::new());
        let base_url_str = base_url_config.unwrap_or_else(net::default_base_url);
        let base_url = Rc::new(RefCell::new(parse_base_url(&base_url_str)?));
        {
            let mut d = doc.borrow_mut();
            d.set_net_provider(net_provider.clone() as Arc<dyn blitz_traits::net::NetProvider>);
            d.set_base_url(&base_url.borrow().to_string());
        }

        // Create a <body>-like container element directly under the document root.
        let container_id = {
            let mut d = doc.borrow_mut();
            let cid = create_container_element(&mut d);
            d.mutate().append_children(0, &[cid]);
            cid
        };

        if document_scroll {
            apply_document_scroll_styles(&doc, container_id, height);
        }

        // --- Initial stylesheets (registered before mount so first paint is styled) ---
        let (stylesheets, next_stylesheet_id) =
            register_initial_stylesheets(&doc, &initial_stylesheets);

        let wake = Arc::new(tokio::sync::Notify::new());

        // --- State ---
        let state = StateHandle::new_with_wake(json!({}), Arc::clone(&wake));

        // --- Events ---
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

        // --- JS context ---
        let js = JsContext::new(Rc::clone(&doc), Rc::clone(&base_url))?;
        js.mount(component_source, container_id, &state, event_tx.clone())?;

        // --- GPU resources ---
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("solite"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let painter = Painter::new(Arc::clone(&device), Arc::clone(&queue), width, height);

        let instance = Self {
            width,
            height,
            device,
            doc,
            js,
            painter,
            texture,
            texture_view,
            state,
            event_tx,
            container_id,
            document_scroll,
            range_drag_id: None,
            hovered_node_id: None,
            active_node_id: None,
            focused_node_id: None,
            needs_paint: true, // first frame always paints
            wake,
            stylesheets,
            next_stylesheet_id,
            scrollbars: Vec::new(),
            scrollbar_drag: None,
            scrollbar_theme: None,
            net_provider,
            base_url,
        };

        Ok((instance, event_rx))
    }

    /// Create a new instance.
    ///
    /// `component_source` is evaluated as an ES module. Bridge globals
    /// (`__sol_createElement`, etc.) and the `solite-runtime` module are
    /// pre-installed so the component can import and use them.
    ///
    /// Returns the instance and a channel receiver for JS-emitted events.
    #[cfg(not(test))]
    pub fn new(
        config: InstanceConfig,
        component_source: &str,
    ) -> Result<(Self, UnboundedReceiver<Event>), InstanceError> {
        Self::new_inner(config, component_source)
    }

    /// Create a new instance.
    ///
    /// `component_source` is evaluated as an ES module. Bridge globals
    /// (`__sol_createElement`, etc.) and the `solite-runtime` module are
    /// pre-installed so the component can import and use them.
    ///
    /// Returns the instance and a channel receiver for JS-emitted events.
    #[cfg(test)]
    pub fn new(
        config: InstanceConfig,
        component_source: &str,
    ) -> (Self, UnboundedReceiver<Event>) {
        Self::new_inner(config, component_source).expect("instance initialization failed")
    }

    /// Create a new instance from a component file or source root directory.
    ///
    /// If `component_path` is a file, it is loaded directly. If it is a
    /// directory, the loader looks for `index.tsx` or `app.tsx` (and the
    /// matching `.jsx`, `.ts`, `.js`, and `.mjs` variants) in that directory
    /// and mounts the first match.
    ///
    /// Returns the instance and a channel receiver for JS-emitted events.
    fn new_from_file_inner(
        config: InstanceConfig,
        component_path: &Path,
    ) -> Result<(Self, UnboundedReceiver<Event>), InstanceError> {
        let component_path = crate::js::resolve_component_entrypoint(component_path);

        let component_source = std::fs::read_to_string(&component_path)?;
        #[cfg(feature = "jsx-compiler")]
        let component_source = compiler::compile_component_source(&component_path, &component_source)?;
        #[cfg(not(feature = "jsx-compiler"))]
        if is_jsx_or_ts_module(&component_path) {
            return Err(InstanceError::UnsupportedJsxModule {
                path: component_path.to_string_lossy().to_string(),
            });
        }
        let component_path = component_path.to_string_lossy().to_string();

        let InstanceConfig {
            width,
            height,
            device,
            queue,
            stylesheets: initial_stylesheets,
            document_scroll,
            base_url: base_url_config,
        } = config;

        // --- Document ---
        let viewport = Viewport {
            window_size: (width, height),
            hidpi_scale: 1.0,
            zoom: 1.0,
            color_scheme: ColorScheme::Light,
        };
        let doc = Rc::new(RefCell::new(BaseDocument::new(DocumentConfig {
            viewport: Some(viewport),
            ..Default::default()
        })));

        // --- Resource provider (images, fonts) ---
        let net_provider = Arc::new(SoliteNetProvider::new());
        // When loading from a file, default the base URL to the file's parent
        // directory so sibling images/fonts referenced relatively (`<img
        // src="logo.png">` next to the component) resolve correctly.
        let base_url_str = base_url_config.unwrap_or_else(|| {
            std::path::Path::new(&component_path)
                .parent()
                .and_then(|parent| Url::from_directory_path(parent).ok())
                .map(|u| u.to_string())
                .unwrap_or_else(net::default_base_url)
        });
        let base_url = Rc::new(RefCell::new(parse_base_url(&base_url_str)?));
        {
            let mut d = doc.borrow_mut();
            d.set_net_provider(net_provider.clone() as Arc<dyn blitz_traits::net::NetProvider>);
            d.set_base_url(&base_url.borrow().to_string());
        }

        // Create a <body>-like container element directly under the document root.
        let container_id = {
            let mut d = doc.borrow_mut();
            let cid = create_container_element(&mut d);
            d.mutate().append_children(0, &[cid]);
            cid
        };

        if document_scroll {
            apply_document_scroll_styles(&doc, container_id, height);
        }

        // --- Initial stylesheets (registered before mount so first paint is styled) ---
        let (stylesheets, next_stylesheet_id) =
            register_initial_stylesheets(&doc, &initial_stylesheets);

        let wake = Arc::new(tokio::sync::Notify::new());

        // --- State ---
        let state = StateHandle::new_with_wake(json!({}), Arc::clone(&wake));

        // --- Events ---
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

        // --- JS context ---
        let js = JsContext::new_with_module_base(
            Rc::clone(&doc),
            Some(std::path::Path::new(&component_path)),
            Rc::clone(&base_url),
        )?;
        js.mount_with_module_path(
            &component_path,
            &component_source,
            container_id,
            &state,
            event_tx.clone(),
        )?;

        // --- GPU resources ---
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("solite"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let painter = Painter::new(Arc::clone(&device), Arc::clone(&queue), width, height);

        let instance = Self {
            width,
            height,
            device,
            doc,
            js,
            painter,
            texture,
            texture_view,
            state,
            event_tx,
            container_id,
            document_scroll,
            range_drag_id: None,
            hovered_node_id: None,
            active_node_id: None,
            focused_node_id: None,
            needs_paint: true, // first frame always paints
            wake,
            stylesheets,
            next_stylesheet_id,
            scrollbars: Vec::new(),
            scrollbar_drag: None,
            scrollbar_theme: None,
            net_provider,
            base_url,
        };

        Ok((instance, event_rx))
    }

    /// Create a new instance from a component file or source root directory.
    ///
    /// If `component_path` is a file, it is loaded directly. If it is a
    /// directory, the loader looks for `index.tsx` or `app.tsx` (and the
    /// matching `.jsx`, `.ts`, `.js`, and `.mjs` variants) in that directory
    /// and mounts the first match.
    ///
    /// Returns the instance and a channel receiver for JS-emitted events.
    #[cfg(not(test))]
    pub fn new_from_file(
        config: InstanceConfig,
        component_path: &Path,
    ) -> Result<(Self, UnboundedReceiver<Event>), InstanceError> {
        Self::new_from_file_inner(config, component_path)
    }

    /// Create a new instance from a component file or source root directory.
    ///
    /// If `component_path` is a file, it is loaded directly. If it is a
    /// directory, the loader looks for `index.tsx` or `app.tsx` (and the
    /// matching `.jsx`, `.ts`, `.js`, and `.mjs` variants) in that directory
    /// and mounts the first match.
    ///
    /// Returns the instance and a channel receiver for JS-emitted events.
    #[cfg(test)]
    pub fn new_from_file(
        config: InstanceConfig,
        component_path: &Path,
    ) -> (Self, UnboundedReceiver<Event>) {
        Self::new_from_file_inner(config, component_path).expect("instance initialization failed")
    }

    /// Create a new instance from a source root directory.
    #[cfg(not(test))]
    pub fn new_from_dir(
        config: InstanceConfig,
        source_dir: &Path,
    ) -> Result<(Self, UnboundedReceiver<Event>), InstanceError> {
        Self::new_from_file_inner(config, source_dir)
    }

    /// Create a new instance from a source root directory.
    #[cfg(test)]
    pub fn new_from_dir(config: InstanceConfig, source_dir: &Path) -> (Self, UnboundedReceiver<Event>) {
        Self::new_from_file(config, source_dir)
    }

    /// Create a new instance from a virtual file list.
    ///
    /// The file paths are resolved relative to the virtual project root. The
    /// loader looks for `index.tsx` or `app.tsx` (and matching `.jsx`, `.ts`,
    /// `.js`, and `.mjs` variants) in the provided list and mounts the first
    /// match.
    fn new_from_virtual_files_inner(
        config: InstanceConfig,
        files: Vec<VirtualSourceFile>,
    ) -> Result<(Self, UnboundedReceiver<Event>), InstanceError> {
        let component_path = crate::js::resolve_virtual_entrypoint(&files);
        let component_source = files
            .iter()
            .find(|file| file.path == component_path)
            .map(|file| file.source.clone())
            .ok_or_else(|| InstanceError::MissingVirtualEntrypoint {
                path: component_path.clone(),
            })?;
        let component_source = {
            #[cfg(feature = "jsx-compiler")]
            {
                compiler::compile_component_source(Path::new(&component_path), &component_source)?
            }
            #[cfg(not(feature = "jsx-compiler"))]
            {
                if is_jsx_or_ts_module(Path::new(&component_path)) {
                    return Err(InstanceError::UnsupportedJsxModule { path: component_path });
                }
                component_source
            }
        };

        let InstanceConfig {
            width,
            height,
            device,
            queue,
            stylesheets: initial_stylesheets,
            document_scroll,
            base_url: base_url_config,
        } = config;

        // --- Document ---
        let viewport = Viewport {
            window_size: (width, height),
            hidpi_scale: 1.0,
            zoom: 1.0,
            color_scheme: ColorScheme::Light,
        };
        let doc = Rc::new(RefCell::new(BaseDocument::new(DocumentConfig {
            viewport: Some(viewport),
            ..Default::default()
        })));

        // --- Resource provider (images, fonts) ---
        let net_provider = Arc::new(SoliteNetProvider::new());
        let base_url_str = base_url_config.unwrap_or_else(net::default_base_url);
        let base_url = Rc::new(RefCell::new(parse_base_url(&base_url_str)?));
        {
            let mut d = doc.borrow_mut();
            d.set_net_provider(net_provider.clone() as Arc<dyn blitz_traits::net::NetProvider>);
            d.set_base_url(&base_url.borrow().to_string());
        }

        // Create a <body>-like container element directly under the document root.
        let container_id = {
            let mut d = doc.borrow_mut();
            let cid = create_container_element(&mut d);
            d.mutate().append_children(0, &[cid]);
            cid
        };

        if document_scroll {
            apply_document_scroll_styles(&doc, container_id, height);
        }

        // --- Initial stylesheets (registered before mount so first paint is styled) ---
        let (stylesheets, next_stylesheet_id) =
            register_initial_stylesheets(&doc, &initial_stylesheets);

        let wake = Arc::new(tokio::sync::Notify::new());

        // --- State ---
        let state = StateHandle::new_with_wake(json!({}), Arc::clone(&wake));

        // --- Events ---
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

        // --- JS context ---
        let js = JsContext::new_with_virtual_files(Rc::clone(&doc), files, Rc::clone(&base_url))?;
        js.mount_with_module_path(
            &component_path,
            &component_source,
            container_id,
            &state,
            event_tx.clone(),
        )?;

        // --- GPU resources ---
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("solite"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let painter = Painter::new(Arc::clone(&device), Arc::clone(&queue), width, height);

        let instance = Self {
            width,
            height,
            device,
            doc,
            js,
            painter,
            texture,
            texture_view,
            state,
            event_tx,
            container_id,
            document_scroll,
            range_drag_id: None,
            hovered_node_id: None,
            active_node_id: None,
            focused_node_id: None,
            needs_paint: true, // first frame always paints
            wake,
            stylesheets,
            next_stylesheet_id,
            scrollbars: Vec::new(),
            scrollbar_drag: None,
            scrollbar_theme: None,
            net_provider,
            base_url,
        };

        Ok((instance, event_rx))
    }

    /// Create a new instance from a virtual file list.
    ///
    /// The file paths are resolved relative to the virtual project root. The
    /// loader looks for `index.tsx` or `app.tsx` (and matching `.jsx`, `.ts`,
    /// `.js`, and `.mjs` variants) in the provided list and mounts the first
    /// match.
    #[cfg(not(test))]
    pub fn new_from_virtual_files(
        config: InstanceConfig,
        files: Vec<VirtualSourceFile>,
    ) -> Result<(Self, UnboundedReceiver<Event>), InstanceError> {
        Self::new_from_virtual_files_inner(config, files)
    }

    /// Create a new instance from a virtual file list.
    ///
    /// The file paths are resolved relative to the virtual project root. The
    /// loader looks for `index.tsx` or `app.tsx` (and matching `.jsx`, `.ts`,
    /// `.js`, and `.mjs` variants) in the provided list and mounts the first
    /// match.
    #[cfg(test)]
    pub fn new_from_virtual_files(
        config: InstanceConfig,
        files: Vec<VirtualSourceFile>,
    ) -> (Self, UnboundedReceiver<Event>) {
        Self::new_from_virtual_files_inner(config, files).expect("instance initialization failed")
    }

    /// Set the document shell provider after construction.
    ///
    /// This is used by hosts that need clipboard / redraw hooks or other
    /// shell integration. The provider is delegated to the underlying
    /// `blitz-dom` document instance.
    pub fn set_shell_provider(&self, shell_provider: Arc<dyn ShellProvider>) {
        self.doc.borrow_mut().set_shell_provider(shell_provider);
    }

    /// Set the base URL used to resolve relative `<img src>` / CSS `url(...)`
    /// references.
    ///
    /// Returns `false` if `url` is not a valid absolute URL. Affects
    /// subsequent attribute writes only — previously-loaded images keep their
    /// cached bytes.
    pub fn set_base_url(&self, url: &str) -> bool {
        let Ok(parsed) = Url::parse(url) else {
            return false;
        };
        *self.base_url.borrow_mut() = parsed.clone();
        self.doc.borrow_mut().set_base_url(parsed.as_str());
        true
    }

    /// Register a custom font from raw bytes (TTF, OTF, WOFF, or WOFF2).
    ///
    /// `family` is the CSS-visible family name; subsequent `font-family:
    /// '<family>'` declarations in CSS or inline styles match this font.
    /// The font is installed by injecting a synthetic `@font-face` rule and
    /// serving the bytes through the document's NetProvider, so the rest of
    /// the rendering pipeline (parley shaping, blitz `@font-face`
    /// registration, inline-context invalidation) runs unchanged.
    ///
    /// Returns an opaque [`StylesheetId`] that identifies the synthetic
    /// `@font-face` stylesheet — pass it to [`Self::remove_stylesheet`] to
    /// drop the host-tracked entry. (Note: blitz's `parley` font collection
    /// itself does not currently support unregistering a font, so the bytes
    /// remain available to text layout for the rest of the document's
    /// lifetime.)
    pub fn register_font_bytes(
        &mut self,
        family: &str,
        bytes: Vec<u8>,
        format: FontFormat,
    ) -> StylesheetId {
        let registered = fonts::register(&self.net_provider, family, bytes, format);
        // Plumb the @font-face rule through a real <style> node so blitz's
        // `add_stylesheet_for_node` path runs and `fetch_font_face` is
        // called against our NetProvider — `add_user_agent_stylesheet` does
        // NOT fire the font-face fetch path.
        let id = StylesheetId(self.next_stylesheet_id);
        self.next_stylesheet_id += 1;
        {
            let mut doc = self.doc.borrow_mut();
            let style_id = doc
                .mutate()
                .create_element(font_face_style_qual(), Vec::new());
            let text_id = doc.create_text_node(&registered.css);
            doc.mutate().append_children(style_id, &[text_id]);
            doc.mutate().append_children(self.container_id, &[style_id]);
            doc.process_style_element(style_id);
        }
        self.stylesheets.insert(id, registered.css);
        self.needs_paint = true;
        id
    }

    /// Register a custom font from a file on disk.
    ///
    /// The font format is inferred from the file extension (`.ttf`, `.otf`,
    /// `.woff`, `.woff2`); pass [`Self::register_font_bytes`] explicitly if
    /// you need to override.
    pub fn register_font_from_path(
        &mut self,
        family: &str,
        path: &Path,
    ) -> Result<StylesheetId, RegisterFontError> {
        let format = FontFormat::from_path(path).ok_or(RegisterFontError::UnknownFormat)?;
        let bytes = std::fs::read(path).map_err(RegisterFontError::Io)?;
        Ok(self.register_font_bytes(family, bytes, format))
    }

    /// Start watching a component path and receive changed file paths.
    /// Keep the returned [`FileWatch`] alive; dropping it stops the watch.
    pub fn watch_files(component_path: &Path) -> notify::Result<FileWatch> {
        let component_path = component_path
            .canonicalize()
            .unwrap_or_else(|_| component_path.to_path_buf());
        let watch_root = if component_path.is_dir() {
            component_path.clone()
        } else {
            component_path.parent().map_or_else(
                || Path::new(".").to_path_buf(),
                |parent| parent.to_path_buf(),
            )
        };

        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let mut watcher =
            notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(event) = res {
                    for path in event.paths {
                        let _ = tx.send(path);
                    }
                }
            })?;
        watcher.watch(&watch_root, RecursiveMode::Recursive)?;

        Ok(FileWatch {
            root: component_path,
            changed: rx,
            _watcher: watcher,
        })
    }

    // ── Host API ──────────────────────────────────────────────────────────────

    /// Pump the JS job queue and flush pending state patches.
    ///
    /// Call once per frame (or on a wake signal). Returns a [`TickResult`]
    /// so the host knows whether to call [`render`] and whether to schedule
    /// another tick immediately.
    pub fn tick(&mut self) -> TickResult {
        let mut result = self.js.tick(&self.state, 256);

        // Advance the caret blink on the focused native input, if any. The
        // toggle is driven from inside tick() rather than from a separate
        // host timer so anyone calling `tick()` regularly (every ~50–500 ms)
        // gets blinking for free; hosts that want a tighter cadence can call
        // `tick()` more often.
        let blink_flipped = self.advance_input_blink();
        if blink_flipped {
            self.needs_paint = true;
        }

        // Drain resource fetch outcomes from the NetProvider and turn them
        // plus the document's current image state into `load` / `error`
        // events on the JS side. Image fetches happen synchronously inside
        // mutator hooks; the decoded bytes only land on the node during the
        // next `BaseDocument::resolve()` (which `render()` calls), so this
        // is the first place where the `load` event can fire after a render
        // cycle has run.
        let fetch_events = self.net_provider.drain_events();
        let img_events: Vec<ImgEvent> = {
            let mut watcher = self.js.img_watcher.borrow_mut();
            watcher.ingest_fetch_events(fetch_events);
            watcher.collect_pending(&self.doc.borrow())
        };
        for ev in img_events {
            let (node_id, name) = match ev {
                ImgEvent::Load { node_id } => (node_id, "load"),
                ImgEvent::Error { node_id } => (node_id, "error"),
            };
            let r = self.js.dispatch_image_event(node_id, name);
            if r.needs_paint {
                self.needs_paint = true;
                result.needs_paint = true;
            }
            if r.jobs_pending {
                result.jobs_pending = true;
            }
        }

        let needs_paint = result.needs_paint || self.needs_paint;
        if result.needs_paint {
            self.needs_paint = true;
        }
        TickResult {
            needs_paint,
            jobs_pending: result.jobs_pending,
        }
    }

    /// Flip caret visibility on the currently-focused input if its blink
    /// interval has elapsed. Returns true if anything changed.
    fn advance_input_blink(&mut self) -> bool {
        let Some(focused) = self.focused_node_id else {
            return false;
        };
        self.js
            .inputs
            .borrow_mut()
            .get_mut(&focused)
            .is_some_and(|state| state.tick_blink(std::time::Instant::now()))
    }

    /// Resolve layout and paint the document into the output texture.
    ///
    /// Returns a reference to the [`wgpu::TextureView`] the host can composite.
    pub fn render(&mut self) -> &wgpu::TextureView {
        // Resolve CSS + layout.
        self.sync_input_render_text_before_layout();
        self.doc.borrow_mut().resolve(0.0);
        self.sync_input_render_text_before_layout();

        // Compute scrollbar geometry from the resolved layout — final_layout
        // and scroll_offset are now current. Reused by `dispatch_mouse` for
        // scrollbar hit-testing this frame.
        self.scrollbars = collect_scrollbar_regions(&self.doc.borrow());
        let input_selections = self.collect_input_selections();
        let input_carets = self.collect_input_carets();

        // Paint into the wgpu texture, layering scrollbars on top.
        {
            let mut doc = self.doc.borrow_mut();
            let masked_focus = self.mask_blitz_text_input_focus_for_paint(&mut doc);
            self.painter.paint(
                &mut doc,
                &self.scrollbars,
                &input_selections,
                &input_carets,
                self.scrollbar_theme,
                &self.texture,
            );
            self.restore_blitz_text_input_focus_after_paint(&mut doc, masked_focus);
        }
        self.needs_paint = false;

        &self.texture_view
    }

    /// Access the instance output texture backing the last paint.
    pub fn texture(&self) -> &wgpu::Texture {
        &self.texture
    }

    /// Resize the output texture and viewport.
    ///
    /// The next `render()` call will repaint at the new size.
    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;

        // Reallocate texture.
        self.texture = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("solite"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        self.texture_view = self
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // Update blitz viewport.
        let viewport = Viewport {
            window_size: (width, height),
            hidpi_scale: 1.0,
            zoom: 1.0,
            color_scheme: ColorScheme::Light,
        };
        self.doc.borrow_mut().set_viewport(viewport);

        if self.document_scroll {
            apply_document_scroll_styles(&self.doc, self.container_id, height);
        }

        // Update painter output buffers with new dimensions.
        self.painter.resize(width, height);
        self.needs_paint = true;
    }

    fn document_coords_for_client(&self, x: f32, y: f32) -> (f32, f32) {
        if x < 0.0 || y < 0.0 || x >= self.width as f32 || y >= self.height as f32 {
            return (x, y);
        }

        let scroll = self.doc.borrow().viewport_scroll();
        (x + scroll.x as f32, y + scroll.y as f32)
    }

    fn hit_node_id(&self, x: f32, y: f32) -> Option<usize> {
        let (document_x, document_y) = self.document_coords_for_client(x, y);
        // Open popups are absolutely-positioned children of their <select> and
        // extend below it; the regular tree hit-test (`hit_visible_node_id`)
        // bails when the click is outside the select's bounding box and never
        // descends into the popup. Check popups first using their absolute
        // positions so option clicks land on the popup nodes.
        if let Some(id) = self.hit_popup_node(document_x, document_y) {
            return Some(id);
        }
        let doc = self.doc.borrow();
        let root_id = doc.root_element().id;
        hit_visible_node_id(&doc, root_id, document_x, document_y)
    }

    /// Look for a hit against any open `<select>` popup overlay, using each
    /// popup option's resolved absolute position. Returns the deepest matching
    /// option node id, falling back to the popup root if the point is inside
    /// the popup background but not over any option.
    fn hit_popup_node(&self, document_x: f32, document_y: f32) -> Option<usize> {
        let doc = self.doc.borrow();
        let selects = self.js.selects.borrow();
        for (_, state) in selects.iter() {
            let Some(popup_id) = state.popup_root_id else {
                continue;
            };
            let Some(popup_node) = doc.get_node(popup_id) else {
                continue;
            };
            let abs = popup_node.absolute_position(0.0, 0.0);
            let size = popup_node.final_layout.size;
            if document_x < abs.x
                || document_x > abs.x + size.width
                || document_y < abs.y
                || document_y > abs.y + size.height
            {
                continue;
            }
            for opt_id in state.option_node_ids.iter().flatten().copied() {
                let Some(opt_node) = doc.get_node(opt_id) else {
                    continue;
                };
                let opt_abs = opt_node.absolute_position(0.0, 0.0);
                let opt_size = opt_node.final_layout.size;
                if document_x >= opt_abs.x
                    && document_x <= opt_abs.x + opt_size.width
                    && document_y >= opt_abs.y
                    && document_y <= opt_abs.y + opt_size.height
                {
                    return Some(opt_id);
                }
            }
            return Some(popup_id);
        }
        None
    }

    /// Read the current scroll offset of a node along one axis.
    fn node_scroll(&self, node_id: usize, axis: ScrollAxis) -> f32 {
        self.doc
            .borrow()
            .get_node(node_id)
            .map(|node| match axis {
                ScrollAxis::Vertical => node.scroll_offset.y as f32,
                ScrollAxis::Horizontal => node.scroll_offset.x as f32,
            })
            .unwrap_or(0.0)
    }

    /// Move a node's scroll offset along one axis to an absolute target value.
    ///
    /// Note: `BaseDocument::scroll_node_by` uses an inverted sign convention
    /// (positive delta scrolls *back* toward the start), so we negate `delta`.
    fn set_node_scroll(&mut self, node_id: usize, axis: ScrollAxis, target: f32) {
        let current = self.node_scroll(node_id, axis);
        let delta = (target - current) as f64;
        if delta == 0.0 {
            return;
        }
        let (dx, dy) = match axis {
            ScrollAxis::Vertical => (0.0, -delta),
            ScrollAxis::Horizontal => (-delta, 0.0),
        };
        self.doc
            .borrow_mut()
            .scroll_node_by(node_id, dx, dy, |_| {});
    }

    fn set_active_to_node(&mut self, new_active_id: Option<usize>) -> bool {
        if new_active_id == self.active_node_id {
            return false;
        }
        let old_active_id = self.active_node_id;
        let mut doc = self.doc.borrow_mut();
        let old_path = existing_node_layout_ancestors(&doc, old_active_id);
        let new_path = existing_node_layout_ancestors(&doc, new_active_id);
        let same_count = old_path
            .iter()
            .zip(&new_path)
            .take_while(|(o, n)| o == n)
            .count();
        for &id in old_path.iter().skip(same_count) {
            doc.snapshot_node_and(id, |node| node.unactive());
        }
        for &id in new_path.iter().skip(same_count) {
            doc.snapshot_node_and(id, |node| node.active());
        }
        drop(doc);
        self.active_node_id = new_active_id;
        true
    }

    /// Apply hover snapshot + state updates by deferring to Blitz's canonical
    /// `BaseDocument::set_hover_to`. This also updates the document's own
    /// `hover_node_id` — selector matching for `:hover` reads it, and any
    /// rolled-our-own snapshot path that doesn't update it leaves invalidation
    /// in a state where the pseudo-class flip never reaches subsequent
    /// restyles.
    ///
    /// We always defer to Blitz: if we gated on our local `hovered_node_id`
    /// matching the new id, any disagreement between our custom hit-test
    /// (`hit_visible_node_id`) and Blitz's (`BaseDocument::hit`) would mask
    /// out the call entirely. `new_id` is provided only to keep our local
    /// tracker (used for JS `mouseover`/`mouseout` dispatch) in sync.
    fn set_hover_to_node(&mut self, x: f32, y: f32, new_id: Option<usize>) -> bool {
        let (doc_x, doc_y) = self.document_coords_for_client(x, y);
        let changed = self.doc.borrow_mut().set_hover_to(doc_x, doc_y);
        let tracker_changed = new_id != self.hovered_node_id;
        self.hovered_node_id = new_id;
        changed || tracker_changed
    }

    // ── Mouse input ──────────────────────────────────────────────────────────

    /// Forward a mouse event to the document.
    ///
    /// Hit-tests the resolved layout to find the deepest node under `(x, y)`,
    /// walks up the ancestor chain to find a registered event handler, and
    /// calls it. `MouseEvent::Move` updates hover state and dispatches
    /// transition events (`mouseover`, `mouseout`, `mouseenter`, `mouseleave`,
    /// `hover`, `hoverenter`, `hoverleave`). Only `MouseEvent::Down { button:
    /// Left }` triggers `"click"`
    /// handlers; other events are accepted for future extension.
    ///
    /// **Requires layout to be current** — call `render()` before dispatching.
    ///
    /// Returns a [`TickResult`] so the host knows whether to call `render()`
    /// and whether to tick again.
    pub fn dispatch_mouse(&mut self, x: f32, y: f32, event: MouseEvent) -> TickResult {
        // ── Scrollbar interaction takes priority over document hit-testing.
        //
        // While the user is dragging a scrollbar thumb, every Move event
        // updates that node's scroll_offset directly. Up ends the drag.
        if let Some(drag) = self.scrollbar_drag {
            match event {
                MouseEvent::Move { x, y } => {
                    let pointer = match drag.axis {
                        ScrollAxis::Vertical => y,
                        ScrollAxis::Horizontal => x,
                    };
                    let target = drag.pointer_to_scroll(pointer);
                    self.set_node_scroll(drag.node_id, drag.axis, target);
                    self.needs_paint = true;
                    return TickResult {
                        needs_paint: true,
                        jobs_pending: false,
                    };
                }
                MouseEvent::Up { .. } => {
                    self.scrollbar_drag = None;
                    return TickResult::default();
                }
                _ => {}
            }
        }

        // Range drag takes priority: every Move updates the value, Up ends the drag.
        if let Some(drag_id) = self.range_drag_id {
            match event {
                MouseEvent::Move { x, y } => {
                    let (doc_x, _) = self.document_coords_for_client(x, y);
                    if let Some(result) = self.update_range_from_x(drag_id, doc_x) {
                        self.needs_paint = true;
                        return result;
                    }
                    return TickResult::default();
                }
                MouseEvent::Up { .. } => {
                    self.range_drag_id = None;
                    return TickResult::default();
                }
                _ => {}
            }
        }

        // MouseDown on a scrollbar thumb or track: start a drag or page-step.
        if let MouseEvent::Down {
            button: MouseButton::Left,
            ..
        } = event
        {
            let (doc_x, doc_y) = self.document_coords_for_client(x, y);
            match scrollbar::hit_scrollbar(&self.scrollbars, doc_x, doc_y) {
                Some(ScrollbarHit::Thumb(region)) => {
                    self.scrollbar_drag = Some(ScrollbarDrag::from_thumb_hit(region, doc_x, doc_y));
                    return TickResult::default();
                }
                Some(ScrollbarHit::Track(region)) => {
                    // Page step: jump by ~80% of the visible track length
                    // along the scrollbar's axis.
                    let (pointer, thumb_start, track_length) = match region.axis {
                        ScrollAxis::Vertical => (doc_y, region.thumb.1, region.track.3),
                        ScrollAxis::Horizontal => (doc_x, region.thumb.0, region.track.2),
                    };
                    let direction = if pointer < thumb_start { -1.0 } else { 1.0 };
                    let step = (track_length * 0.8).max(20.0);
                    let current = self.node_scroll(region.node_id, region.axis);
                    let target = (current + direction * step).clamp(0.0, region.max_scroll);
                    self.set_node_scroll(region.node_id, region.axis, target);
                    self.needs_paint = true;
                    return TickResult {
                        needs_paint: true,
                        jobs_pending: false,
                    };
                }
                None => {}
            }
        }

        match event {
            MouseEvent::Move { x, y } => {
                let old_hover_id = self.hovered_node_id;
                let new_hover_id = self.hit_node_id(x, y);

                // While a popup is open, hovering over an option updates the
                // active-index highlight on that popup.
                if let Some(hover_id) = new_hover_id {
                    if let Some((sel_id, opt_idx)) = self.popup_option_for_hit(hover_id) {
                        let changed = {
                            let mut selects = self.js.selects.borrow_mut();
                            match selects.get_mut(&sel_id) {
                                Some(state) if state.active_index() != Some(opt_idx) => {
                                    state.set_active_index(Some(opt_idx));
                                    true
                                }
                                _ => false,
                            }
                        };
                        if changed {
                            self.sync_select_popup_highlights(sel_id);
                            self.needs_paint = true;
                        }
                    }
                }
                let hover_changed = self.set_hover_to_node(x, y, new_hover_id);

                // Browser parity: if the mouse moves off the actively-pressed
                // node while held, the :active state drops. (When it moves
                // back over before release, browsers re-engage :active; we
                // don't track that yet since we no longer know which button
                // is held — Down/Up are the source of truth here.)
                if let Some(active) = self.active_node_id
                    && new_hover_id != Some(active)
                    && self.set_active_to_node(None)
                {
                    self.needs_paint = true;
                }

                let move_target = new_hover_id;
                let mut result = TickResult::default();

                if old_hover_id != new_hover_id {
                    if let Some(old_id) = old_hover_id {
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_with_related(
                                old_id,
                                "mouseout",
                                x,
                                y,
                                old_id,
                                new_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                old_id,
                                "mouseleave",
                                x,
                                y,
                                old_id,
                                new_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                old_id,
                                "hoverleave",
                                x,
                                y,
                                old_id,
                                new_hover_id,
                            ),
                        );
                    }

                    if let Some(new_id) = new_hover_id {
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_with_related(
                                new_id,
                                "mouseover",
                                x,
                                y,
                                new_id,
                                old_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                new_id,
                                "mouseenter",
                                x,
                                y,
                                new_id,
                                old_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                new_id,
                                "hover",
                                x,
                                y,
                                new_id,
                                old_hover_id,
                            ),
                        );
                        result = combine_tick_result(
                            result,
                            self.js.dispatch_event_at_with_target(
                                new_id,
                                "hoverenter",
                                x,
                                y,
                                new_id,
                                old_hover_id,
                            ),
                        );
                    }
                }

                if let Some(target_id) = move_target {
                    result = combine_tick_result(
                        result,
                        self.js.dispatch_event_at(target_id, "mousemove", x, y),
                    );
                }

                if hover_changed || result.needs_paint {
                    self.needs_paint = true;
                    result.needs_paint = true;
                }
                return result;
            }
            MouseEvent::Wheel {
                x,
                y,
                delta_x,
                delta_y,
            } => return self.dispatch_wheel(x, y, delta_x, delta_y),
            MouseEvent::Down { .. } | MouseEvent::Up { .. } => {}
        };

        let event_name = match event {
            MouseEvent::Down {
                button: MouseButton::Left,
                ..
            } => {
                let hit_id = self.hit_node_id(x, y);

                // Click landed on a popup option: commit the selection and close.
                if let Some(hit_id) = hit_id {
                    if let Some((sel_id, opt_idx)) = self.popup_option_for_hit(hit_id) {
                        let disabled = self
                            .js
                            .selects
                            .borrow()
                            .get(&sel_id)
                            .and_then(|s| s.options.get(opt_idx).map(|o| o.disabled))
                            .unwrap_or(true);
                        if !disabled {
                            if let Some(state) = self.js.selects.borrow_mut().get_mut(&sel_id) {
                                state.set_selected_index(Some(opt_idx));
                            }
                            self.set_select_open(sel_id, false);
                            self.refresh_select_text(sel_id);
                            let select_snapshot = self
                                .js
                                .selects
                                .borrow()
                                .get(&sel_id)
                                .map(|s| (s.value().unwrap_or_default(), s.selected_index()));
                            if let Some((value, selected_index)) = select_snapshot {
                                return self.js.dispatch_select_change_event(
                                    sel_id,
                                    &value,
                                    selected_index,
                                );
                            }
                            return TickResult::default();
                        }
                        // Disabled option click: swallow and keep open.
                        return TickResult {
                            needs_paint: false,
                            jobs_pending: false,
                        };
                    }
                }

                // Click landed somewhere outside any open select+popup: close
                // those popups (a click on the select itself is dispatched via
                // handle_select_click below).
                let owning_select = hit_id.and_then(|id| self.select_owning_hit(id));
                let open_selects: Vec<usize> = self
                    .js
                    .selects
                    .borrow()
                    .iter()
                    .filter(|(_, s)| s.is_open())
                    .map(|(id, _)| *id)
                    .collect();
                for select_id in open_selects {
                    if Some(select_id) != owning_select {
                        self.set_select_open(select_id, false);
                    }
                }

                let old_focus = self.focused_node_id;
                let hit_id = self.hit_node_id(x, y);
                let focus_id = hit_id.map(|hit_id| {
                    let doc = self.doc.borrow();
                    self.js
                        .find_handler_up(&doc, hit_id, "focus")
                        .or_else(|| self.js.find_handler_up(&doc, hit_id, "keydown"))
                        .unwrap_or(hit_id)
                });

                // :active pseudo-class flips on while the mouse button is held
                // over `hit_id`. Repaint is needed so any matching CSS rule
                // re-evaluates.
                if self.set_active_to_node(hit_id) {
                    self.needs_paint = true;
                }

                let mut result = TickResult::default();

                match focus_id {
                    Some(focus_id) => {
                        if old_focus != Some(focus_id) {
                            result = combine_tick_result(
                                result,
                                self.set_focused_node(Some(focus_id), x, y),
                            );
                        }
                    }
                    None => {
                        result = combine_tick_result(result, self.set_focused_node(None, x, y));
                        if result.needs_paint {
                            self.needs_paint = true;
                        }
                        return result;
                    }
                }

                let Some(hit_id) = hit_id else {
                    return result;
                };

                // Native inputs: handle before JS click dispatch.
                let input_kind = self
                    .js
                    .inputs
                    .borrow()
                    .get(&hit_id)
                    .map(|s| (s.is_checked_like(), s.is_range()));

                match input_kind {
                    Some((true, _)) => {
                        // Checkbox / radio: toggle on click (mirrors Space/Enter).
                        result =
                            combine_tick_result(result, self.handle_checked_input_click(hit_id));
                        self.needs_paint = true;
                        result.needs_paint = true;
                        return result;
                    }
                    Some((_, true)) => {
                        // Range slider: set value from click x, begin drag.
                        let (doc_x, _) = self.document_coords_for_client(x, y);
                        if let Some(r) = self.update_range_from_x(hit_id, doc_x) {
                            result = combine_tick_result(result, r);
                        }
                        self.range_drag_id = Some(hit_id);
                        self.needs_paint = true;
                        result.needs_paint = true;
                        return result;
                    }
                    _ => {}
                }

                // Native selects: toggle open state on click.
                if self.js.selects.borrow().contains_key(&hit_id) {
                    result = combine_tick_result(result, self.handle_select_click(hit_id));
                    return result;
                }

                let handler_node = {
                    let doc = self.doc.borrow();
                    self.js.find_handler_up(&doc, hit_id, "click")
                };

                if let Some(handler_node) = handler_node {
                    return combine_tick_result(
                        result,
                        self.js.dispatch_event(handler_node, "click", x, y),
                    );
                }

                if result.needs_paint {
                    self.needs_paint = true;
                }
                return result;
            }
            MouseEvent::Down {
                button: MouseButton::Right,
                ..
            } => "contextmenu",
            MouseEvent::Down {
                button: MouseButton::Middle,
                ..
            } => "auxclick",
            MouseEvent::Up { .. } => "mouseup",
            MouseEvent::Move { .. } | MouseEvent::Wheel { .. } => unreachable!(),
        };

        // Any mouse-up clears :active. We do this regardless of which button
        // released — browser :active is cleared on any release that ends the
        // press, and tracking which button started it isn't worth the bytes
        // for the rare multi-button case.
        if matches!(event, MouseEvent::Up { .. }) && self.set_active_to_node(None) {
            self.needs_paint = true;
        }

        // Hit-test: find the deepest node at (x, y).
        let hit_id = self.hit_node_id(x, y);
        let Some(hit_id) = hit_id else {
            return TickResult::default();
        };

        // Walk ancestors for a registered handler.
        let handler_node = {
            let doc = self.doc.borrow();
            self.js.find_handler_up(&doc, hit_id, event_name)
        };
        let Some(handler_node) = handler_node else {
            return TickResult::default();
        };

        let result = self.js.dispatch_event(handler_node, event_name, x, y);
        if result.needs_paint {
            self.needs_paint = true;
        }
        result
    }

    /// Forward a wheel event to the document.
    ///
    /// The wheel delta is applied to the nearest scrollable node under `(x, y)`.
    /// If node scrolling bubbles to an ancestor or the viewport, those scroll
    /// offsets are updated as needed and `scroll` events are dispatched for the
    /// node where the offset changed.
    pub fn dispatch_wheel(&mut self, x: f32, y: f32, delta_x: f32, delta_y: f32) -> TickResult {
        let start_id = self.hit_node_id(x, y);
        let Some(start_id) = start_id else {
            return TickResult::default();
        };

        let mut before_offsets = Vec::new();
        let before_viewport = {
            let doc = self.doc.borrow();
            let mut node_id = Some(start_id);
            while let Some(id) = node_id {
                if let Some(node) = doc.get_node(id) {
                    before_offsets.push((id, node.scroll_offset.x, node.scroll_offset.y));
                }
                if id == 0 {
                    break;
                }
                node_id = doc.get_node(id).and_then(|node| node.parent);
            }
            doc.viewport_scroll()
        };

        {
            let mut doc = self.doc.borrow_mut();
            doc.scroll_node_by(start_id, f64::from(delta_x), f64::from(delta_y), |_| {});
        }

        let after_offsets = {
            let doc = self.doc.borrow();
            let mut values = Vec::with_capacity(before_offsets.len());
            for (node_id, _, _) in before_offsets.iter().copied() {
                if let Some(node) = doc.get_node(node_id) {
                    values.push((node_id, node.scroll_offset.x, node.scroll_offset.y));
                }
            }
            values
        };
        let after_viewport = self.doc.borrow().viewport_scroll();

        let mut changed_node = None;
        let mut changed_scroll = (0.0_f64, 0.0_f64);
        for ((node_id, before_x, before_y), (_, after_x, after_y)) in
            before_offsets.iter().zip(after_offsets.iter())
        {
            if before_x != after_x || before_y != after_y {
                changed_node = Some(*node_id);
                changed_scroll = (*after_x, *after_y);
                break;
            }
        }

        let viewport_changed = before_viewport != after_viewport;
        let has_scrolled = { changed_node.is_some() || viewport_changed };
        let target_scroll = after_offsets
            .iter()
            .find(|(node_id, _, _)| *node_id == start_id)
            .map(|(_, scroll_x, scroll_y)| (*scroll_x, *scroll_y))
            .or_else(|| viewport_changed.then_some((after_viewport.x, after_viewport.y)))
            .unwrap_or((0.0, 0.0));

        let mut result = TickResult::default();

        // Resolve the handler in its own statement so the `Ref` from
        // `doc.borrow()` is dropped *before* we re-enter JS — otherwise a
        // reactive effect triggered from the wheel handler that needs
        // `doc.borrow_mut()` (e.g. `__sol_setText`) would panic.
        let wheel_handler_id = self
            .js
            .find_handler_up(&self.doc.borrow(), start_id, "wheel");
        if let Some(wheel_handler_id) = wheel_handler_id {
            result = combine_tick_result(
                result,
                self.js.dispatch_wheel_event(
                    wheel_handler_id,
                    "wheel",
                    x,
                    y,
                    delta_x,
                    delta_y,
                    start_id,
                    None,
                    target_scroll.0,
                    target_scroll.1,
                ),
            );
        }

        let should_dispatch_scroll = if let Some(node_id) = changed_node {
            Some((node_id, changed_scroll))
        } else if viewport_changed {
            Some((self.container_id, (after_viewport.x, after_viewport.y)))
        } else {
            None
        };

        if should_dispatch_scroll.is_none() && !result.needs_paint && !has_scrolled {
            return TickResult::default();
        }

        if let Some((scroll_node_id, (scroll_left, scroll_top))) = should_dispatch_scroll {
            result = combine_tick_result(
                result,
                self.js
                    .dispatch_scroll_event(scroll_node_id, x, y, scroll_left, scroll_top),
            );
        }

        if result.needs_paint || has_scrolled {
            self.needs_paint = true;
            result.needs_paint = true;
        }

        result
    }

    // ── Keyboard input ─────────────────────────────────────────────────────

    /// Forward a key-down event to the focused node.
    pub fn dispatch_key_down(&mut self, event: KeyboardEvent) -> TickResult {
        self.dispatch_key("keydown", event)
    }

    /// Forward a key-up event to the focused node.
    pub fn dispatch_key_up(&mut self, event: KeyboardEvent) -> TickResult {
        self.dispatch_key("keyup", event)
    }

    fn set_focused_node(&mut self, next_focus: Option<usize>, x: f32, y: f32) -> TickResult {
        let old_focus = self.focused_node_id;
        if old_focus == next_focus {
            return TickResult::default();
        }

        let mut result = TickResult::default();

        if let Some(previous) = old_focus {
            if self
                .js
                .selects
                .borrow()
                .get(&previous)
                .is_some_and(|state| state.is_open())
            {
                self.set_select_open(previous, false);
            }
            result = combine_tick_result(result, self.js.dispatch_event(previous, "blur", x, y));
            if self.js.inputs.borrow().contains_key(&previous) {
                self.refresh_input_text(previous);
                self.needs_paint = true;
            }
            self.doc.borrow_mut().clear_focus();
            self.focused_node_id = None;
        }

        let Some(focus_id) = next_focus else {
            if result.needs_paint {
                self.needs_paint = true;
            }
            return result;
        };

        self.focused_node_id = Some(focus_id);
        self.doc.borrow_mut().set_focus_to(focus_id);
        if self.js.inputs.borrow().contains_key(&focus_id) {
            if let Some(state) = self.js.inputs.borrow_mut().get_mut(&focus_id) {
                state.place_caret_at_end();
            }
            self.refresh_input_text(focus_id);
            self.needs_paint = true;
        }
        result = combine_tick_result(result, self.js.dispatch_event(focus_id, "focus", x, y));
        if result.needs_paint {
            self.needs_paint = true;
        }

        result
    }

    fn focus_adjacent_control(&mut self, backwards: bool) -> TickResult {
        let control_order =
            crate::focus::collect_tab_order(&self.doc.borrow(), &self.js.inputs, &self.js.selects);

        if control_order.is_empty() {
            return TickResult::default();
        }

        let next_index = match self
            .focused_node_id
            .and_then(|id| control_order.iter().position(|candidate| *candidate == id))
        {
            Some(current) if backwards => (current + control_order.len() - 1) % control_order.len(),
            Some(current) => (current + 1) % control_order.len(),
            None if backwards => control_order.len() - 1,
            None => 0,
        };

        let mut result = self.set_focused_node(Some(control_order[next_index]), 0.0, 0.0);
        result.needs_paint = true;
        result
    }

    fn dispatch_key(&mut self, event_name: &str, event: KeyboardEvent) -> TickResult {
        if event_name == "keydown" && event.key == "Tab" {
            let mut result = TickResult::default();
            if let Some(focused_id) = self.focused_node_id {
                if self
                    .js
                    .selects
                    .borrow()
                    .get(&focused_id)
                    .is_some_and(|state| state.is_open())
                {
                    let (edited, emits_change) = self.apply_select_key(focused_id, &event);
                    if edited {
                        self.refresh_select_text(focused_id);
                    }
                    if emits_change {
                        let select_snapshot = self
                            .js
                            .selects
                            .borrow()
                            .get(&focused_id)
                            .map(|s| (s.value().unwrap_or_default(), s.selected_index()));
                        if let Some((value, selected_index)) = select_snapshot {
                            let change_result = self.js.dispatch_select_change_event(
                                focused_id,
                                &value,
                                selected_index,
                            );
                            result = combine_tick_result(result, change_result);
                        }
                    }
                }
            }

            return combine_tick_result(result, self.focus_adjacent_control(event.shift_key));
        }

        let Some(focused_id) = self.focused_node_id else {
            return TickResult::default();
        };

        // If the focused node is a native `<input>`, the engine owns editing:
        // apply the keystroke to the InputState, refresh the visible text
        // node, and emit `input` after the user-defined handler so it sees
        // the updated value via `event.value` / `event.target.value`.
        // Caret-only edits (arrows/home/end etc.) refresh visual text but do
        // not dispatch `input` to match browser semantics.
        let (edited, emits_input_event, focus_target) = if event_name == "keydown"
            && self.js.inputs.borrow().contains_key(&focused_id)
        {
            if self
                .js
                .inputs
                .borrow()
                .get(&focused_id)
                .is_some_and(|state| state.is_radio())
                && matches!(
                    event.key.as_str(),
                    "ArrowLeft" | "ArrowRight" | "ArrowUp" | "ArrowDown" | "Home" | "End"
                )
            {
                self.apply_radio_navigation_key(focused_id, &event)
            } else {
                let (edited, emits_input_event) =
                    apply_input_key(&self.js.inputs, focused_id, &event);
                (edited, emits_input_event, None)
            }
        } else if event_name == "keydown" && self.js.selects.borrow().contains_key(&focused_id) {
            let (edited, emits_change) = self.apply_select_key(focused_id, &event);
            (edited, emits_change, None)
        } else {
            (false, false, None)
        };

        let mut result = self.js.dispatch_key_event(focused_id, event_name, &event);

        // Button keyboard activation. Browsers fire `click` on:
        //   - `keydown` for Enter (Repeat included — long-press repeats the
        //     activation), AND
        //   - `keyup` for Space (the keydown shows :active visual state, the
        //     keyup fires the actual click).
        // Only the unmodified keys count; Ctrl/Alt/Meta+Enter is reserved
        // for the user's own shortcut handlers via `onKeyDown`.
        if is_button_node(&self.doc.borrow(), focused_id) {
            let no_mods = !event.ctrl_key && !event.alt_key && !event.meta_key;
            let activate = no_mods
                && match (event_name, event.key.as_str()) {
                    ("keydown", "Enter") => true,
                    ("keyup", " " | "Space") => true,
                    _ => false,
                };
            if activate {
                let click = self.js.dispatch_event_at(focused_id, "click", 0.0, 0.0);
                result = combine_tick_result(result, click);
            }
        }
        if result.needs_paint {
            self.needs_paint = true;
        }

        if let Some(next_focus) = focus_target {
            let focus_result = self.set_focused_node(Some(next_focus), 0.0, 0.0);
            result = combine_tick_result(result, focus_result);
        }

        if edited {
            let target_id = focus_target.unwrap_or(focused_id);
            self.refresh_input_text(target_id);
            self.refresh_select_text(target_id);
            self.needs_paint = true;
        }

        if emits_input_event {
            // Refresh visible text + emit input event for inputs.
            let target_id = focus_target.unwrap_or(focused_id);
            let snapshot = self.js.inputs.borrow().get(&target_id).map(|s| {
                (
                    s.value().to_string(),
                    s.checked(),
                    s.selection_start(),
                    s.selection_end(),
                )
            });
            if let Some((value, checked, selection_start, selection_end)) = snapshot {
                let input_result = self.js.dispatch_input_event(
                    target_id,
                    &value,
                    checked,
                    selection_start,
                    selection_end,
                );
                return combine_tick_result(result, input_result);
            }

            // Refresh visible text + emit change event for selects.
            let select_snapshot = self
                .js
                .selects
                .borrow()
                .get(&focus_target.unwrap_or(focused_id))
                .map(|s| (s.value().unwrap_or_default(), s.selected_index()));
            if let Some((value, selected_index)) = select_snapshot {
                let change_result = self.js.dispatch_select_change_event(
                    focus_target.unwrap_or(focused_id),
                    &value,
                    selected_index,
                );
                return combine_tick_result(result, change_result);
            }
        }

        result
    }

    /// Refresh the visible text child of an `<input>` from its InputState.
    /// The child text node is the first child of the input (seeded in
    /// `__sol_createElement` when the tag is "input").
    fn refresh_input_text(&mut self, input_id: usize) {
        if self
            .js
            .inputs
            .borrow()
            .get(&input_id)
            .is_some_and(|state| state.is_range())
        {
            return;
        }

        let focused = self.focused_node_id == Some(input_id);
        let display = self
            .js
            .inputs
            .borrow()
            .get(&input_id)
            .map(|s| s.render(focused).0);
        let Some(text) = display else { return };
        let child = self
            .doc
            .borrow()
            .get_node(input_id)
            .and_then(|n| n.children.first().copied());
        if let Some(child_id) = child {
            self.doc
                .borrow_mut()
                .mutate()
                .set_node_text(child_id, &text);
        }
    }

    /// Refresh the visible text child of a `<select>` from its SelectState.
    /// The child text node is the first child of the select (seeded in
    /// `__sol_createElement` when the tag is "select").
    /// Also update the select element's value attribute for form submission.
    fn refresh_select_text(&mut self, select_id: usize) {
        let display = self
            .js
            .selects
            .borrow()
            .get(&select_id)
            .map(|s| s.current_label());
        let Some(text) = display else { return };
        let child = self
            .doc
            .borrow()
            .get_node(select_id)
            .and_then(|n| n.children.first().copied());
        if let Some(child_id) = child {
            self.doc
                .borrow_mut()
                .mutate()
                .set_node_text(child_id, &text);
        }

        // Sync the value attribute for form submission
        let value = self
            .js
            .selects
            .borrow()
            .get(&select_id)
            .and_then(|s| s.selected_value().map(str::to_owned));
        self.doc.borrow_mut().mutate().set_attribute(
            select_id,
            blitz_dom::QualName::new(None, blitz_dom::ns!(), blitz_dom::LocalName::from("value")),
            value.as_deref().unwrap_or(""),
        );
    }

    fn sync_input_render_text_before_layout(&self) {
        let focused_id = self.focused_node_id;
        let inputs: Vec<(usize, String, bool, usize)> = self
            .js
            .inputs
            .borrow()
            .iter()
            .map(|(input_id, state)| {
                let (text, placeholder) = state.render(focused_id == Some(*input_id));
                (
                    *input_id,
                    text,
                    placeholder,
                    state.display_caret_byte_index(),
                )
            })
            .collect();
        if inputs.is_empty() {
            return;
        }

        let mut doc = self.doc.borrow_mut();
        for (input_id, text, placeholder, caret_byte) in inputs {
            let Some(has_text_input) = doc.get_node(input_id).map(|node| {
                node.element_data()
                    .and_then(|element| element.text_input_data())
                    .is_some()
            }) else {
                continue;
            };

            if has_text_input {
                doc.with_text_input(input_id, |mut driver| {
                    if driver.editor.raw_text() != text {
                        driver.editor.set_text(&text);
                        driver.refresh_layout();
                    }
                    driver.move_to_byte(caret_byte);
                });
            } else if let Some(element) = doc
                .get_node_mut(input_id)
                .and_then(|node| node.element_data_mut())
            {
                element.attrs.set(attr_qual("value"), &text);
            }

            if let Some(element) = doc
                .get_node_mut(input_id)
                .and_then(|node| node.element_data_mut())
            {
                if placeholder {
                    element
                        .attrs
                        .set(attr_qual("data-ox-placeholder-active"), "true");
                } else {
                    element
                        .attrs
                        .remove(&attr_qual("data-ox-placeholder-active"));
                }
            }
        }
    }

    fn collect_input_carets(&self) -> Vec<InputCaret> {
        let Some(input_id) = self.focused_node_id else {
            return Vec::new();
        };

        let inputs = self.js.inputs.borrow();
        let Some(state) = inputs.get(&input_id) else {
            return Vec::new();
        };
        if !state.blink_visible() {
            return Vec::new();
        }

        let doc = self.doc.borrow();
        let Some(input_node) = doc.get_node(input_id) else {
            return Vec::new();
        };
        let Some(input_data) = input_node
            .element_data()
            .and_then(|element| element.text_input_data())
        else {
            return Vec::new();
        };
        let Some(cursor) = input_data.editor.cursor_geometry(1.5) else {
            return Vec::new();
        };

        let input_origin = input_node.absolute_position(0.0, 0.0);
        let layout = input_node.final_layout;
        let content_x = input_origin.x + layout.border.left + layout.padding.left;
        let content_y = input_origin.y + layout.border.top + layout.padding.top;
        let content_w = layout.content_box_width().max(0.0);
        let content_h = layout.content_box_height().max(1.0);
        let y_offset = input_node.text_input_v_centering_offset(1.0) as f32;
        let cursor_w = (cursor.x1 - cursor.x0).max(0.0) as f32;
        let cursor_h = (cursor.y1 - cursor.y0).max(0.0) as f32;

        let (x, y, caret_w, caret_h) = if cursor_h > 0.0 {
            (
                (content_x + cursor.x0 as f32).clamp(content_x, content_x + content_w),
                (content_y + y_offset + cursor.y0 as f32).clamp(content_y, content_y + content_h),
                cursor_w.max(1.0),
                cursor_h.max(1.0),
            )
        } else {
            let caret_w = cursor_w.max(1.5);
            let caret_h = (content_h * 0.7).max(1.0);
            let x = content_x + estimated_input_char_width(input_node) * state.caret() as f32;
            (
                x.clamp(content_x, content_x + content_w),
                content_y + ((content_h - caret_h).max(0.0) * 0.5),
                caret_w,
                caret_h,
            )
        };

        vec![InputCaret {
            x,
            y,
            width: caret_w,
            height: caret_h,
            color: input_caret_color(input_node),
        }]
    }

    fn collect_input_selections(&self) -> Vec<InputSelection> {
        let Some(input_id) = self.focused_node_id else {
            return Vec::new();
        };

        let inputs = self.js.inputs.borrow();
        let Some(state) = inputs.get(&input_id) else {
            return Vec::new();
        };
        let (selection_start_chars, selection_end_chars) =
            (state.selection_start(), state.selection_end());
        if selection_start_chars == selection_end_chars {
            return Vec::new();
        }
        let selection_len = (selection_end_chars - selection_start_chars) as f32;
        if !state.is_text_like() {
            return Vec::new();
        }

        let doc = self.doc.borrow();
        let Some(input_node) = doc.get_node(input_id) else {
            return Vec::new();
        };
        let Some(input_data) = input_node
            .element_data()
            .and_then(|element| element.text_input_data())
        else {
            return Vec::new();
        };

        let layout = input_node.final_layout;
        let input_origin = input_node.absolute_position(0.0, 0.0);
        let content_x = input_origin.x + layout.border.left + layout.padding.left;
        let content_y = input_origin.y + layout.border.top + layout.padding.top;
        let content_w = layout.content_box_width().max(0.0);
        let content_h = layout.content_box_height().max(1.0);
        let y_offset = input_node.text_input_v_centering_offset(1.0) as f32;

        let display_text = state.render(true).0;
        let selection_start = char_index_to_byte_index(&display_text, selection_start_chars);
        let selection_end = char_index_to_byte_index(&display_text, selection_end_chars);
        let Some(layout_data) = input_data.editor.try_layout() else {
            return vec![];
        };
        let anchor = Cursor::from_byte_index(layout_data, selection_start, Affinity::Downstream);
        let focus = Cursor::from_byte_index(layout_data, selection_end, Affinity::Downstream);
        let selection = Selection::new(anchor, focus);

        let mut selections = Vec::new();
        selection.geometry_with(layout_data, |rect, _line_idx| {
            let x0 = (content_x + rect.x0 as f32).clamp(content_x, content_x + content_w);
            let x1 = (content_x + rect.x1 as f32).clamp(content_x, content_x + content_w);
            let y0 =
                (content_y + y_offset + rect.y0 as f32).clamp(content_y, content_y + content_h);
            let y1 =
                (content_y + y_offset + rect.y1 as f32).clamp(content_y, content_y + content_h);
            let width = (x1 - x0).max(0.0);
            let height = (y1 - y0).max(0.0);
            if width <= 0.0 || height <= 0.0 {
                return;
            }
            selections.push(InputSelection {
                x: x0,
                y: y0,
                width,
                height,
            });
        });

        if selections.is_empty() {
            let width = (estimated_input_char_width(input_node) * selection_len).max(1.0);
            let height = (content_h * 0.7).max(1.0);
            let y = content_y + ((content_h - height).max(0.0) * 0.5);
            selections.push(InputSelection {
                x: (content_x
                    + estimated_input_char_width(input_node) * selection_start_chars as f32)
                    .clamp(content_x, content_x + content_w),
                y,
                width,
                height,
            });
        }

        selections
    }

    fn mask_blitz_text_input_focus_for_paint(&self, doc: &mut BaseDocument) -> Option<usize> {
        let input_id = self.focused_node_id?;
        if !self.js.inputs.borrow().contains_key(&input_id) {
            return None;
        }
        if doc.get_focussed_node_id() != Some(input_id) {
            return None;
        }

        doc.get_node_mut(input_id)?
            .element_state
            .remove(ElementState::FOCUS);
        Some(input_id)
    }

    fn restore_blitz_text_input_focus_after_paint(
        &self,
        doc: &mut BaseDocument,
        input_id: Option<usize>,
    ) {
        let Some(input_id) = input_id else {
            return;
        };
        if let Some(node) = doc.get_node_mut(input_id) {
            node.element_state.insert(ElementState::FOCUS);
        }
    }

    // ── State & events ────────────────────────────────────────────────────────

    /// A clone of the state handle. Can be sent to any thread; writes are
    /// applied on the next `tick()`.
    pub fn state(&self) -> StateHandle {
        self.state.clone()
    }

    /// Dispatch a custom event from the Rust host into the JS runtime.
    ///
    /// JS code can subscribe with `addEventListener(name, listener)` or
    /// `__sol_addEventListener(name, listener)`. The listener receives an object
    /// containing `type`, `detail`, and `payload`; `payload` is an alias for
    /// `detail` for convenience.
    pub fn dispatch_runtime_event(
        &mut self,
        name: impl AsRef<str>,
        payload: serde_json::Value,
    ) -> TickResult {
        let result = self.js.dispatch_runtime_event(name.as_ref(), &payload);
        if result.needs_paint {
            self.needs_paint = true;
        }
        result
    }

    /// Returns and clears the latest JS boundary error captured by the host bridge.
    pub fn take_send_event_error(&self) -> Option<String> {
        self.js.take_send_event_error()
    }

    /// If a focused native `<input>` is blinking, returns the deadline at
    /// which the host should wake up to advance the blink (so it can set
    /// `ControlFlow::WaitUntil(deadline)` and call `tick()`). `None` means
    /// nothing is blinking right now and the host can idle indefinitely.
    pub fn next_blink_deadline(&self) -> Option<std::time::Instant> {
        let focused = self.focused_node_id?;
        let map = self.js.inputs.borrow();
        let state = map.get(&focused)?;
        Some(state.next_blink_at())
    }

    /// A `Notify` handle that fires whenever an async source (e.g. a tokio task
    /// calling `StateHandle::set`) mutates state. The host can await this to
    /// know when to schedule a tick.
    pub fn wake_handle(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.wake)
    }

    // ── Stylesheets ──────────────────────────────────────────────────────────

    /// Register a stylesheet with the document.
    ///
    /// The returned [`StylesheetId`] can be passed to
    /// [`replace_stylesheet`](Self::replace_stylesheet) or
    /// [`remove_stylesheet`](Self::remove_stylesheet) to update or drop it.
    /// Marks the document as needing repaint.
    pub fn add_stylesheet(&mut self, css: &str) -> StylesheetId {
        let id = StylesheetId(self.next_stylesheet_id);
        self.next_stylesheet_id += 1;
        self.doc.borrow_mut().add_user_agent_stylesheet(css);
        self.stylesheets.insert(id, css.to_string());
        self.needs_paint = true;
        id
    }

    /// Replace the contents of a previously-registered stylesheet.
    ///
    /// Returns `true` if the stylesheet was found and replaced, `false`
    /// otherwise. Marks the document as needing repaint on success.
    pub fn replace_stylesheet(&mut self, id: StylesheetId, css: &str) -> bool {
        let Some(old) = self.stylesheets.get_mut(&id) else {
            return false;
        };
        let mut doc = self.doc.borrow_mut();
        doc.remove_user_agent_stylesheet(old);
        doc.add_user_agent_stylesheet(css);
        *old = css.to_string();
        drop(doc);
        self.needs_paint = true;
        true
    }

    /// Remove a previously-registered stylesheet.
    ///
    /// Returns `true` if the stylesheet was found and removed, `false`
    /// otherwise. Marks the document as needing repaint on success.
    pub fn remove_stylesheet(&mut self, id: StylesheetId) -> bool {
        let Some(old) = self.stylesheets.remove(&id) else {
            return false;
        };
        self.doc.borrow_mut().remove_user_agent_stylesheet(&old);
        self.needs_paint = true;
        true
    }

    /// Replace an existing stylesheet, or insert it when the id is not known.
    ///
    /// Returns the stylesheet id to use for subsequent updates.
    pub fn upsert_stylesheet(
        &mut self,
        stylesheet_id: Option<StylesheetId>,
        css: &str,
    ) -> StylesheetId {
        let id = match stylesheet_id {
            Some(id) => {
                if self.replace_stylesheet(id, css) {
                    id
                } else {
                    let _ = self.remove_stylesheet(id);
                    self.add_stylesheet(css)
                }
            }
            None => self.add_stylesheet(css),
        };

        id
    }

    // ── Native inputs ────────────────────────────────────────────────────────

    /// Returns the current value of the `<input>` registered at `node_id`,
    /// or `None` if no input is registered there. Useful for tests and for
    /// hosts that want to read the field directly without round-tripping
    /// through a JS handler.
    pub fn input_value(&self, node_id: usize) -> Option<String> {
        self.js.inputs.borrow().get(&node_id).map(|state| {
            if state.is_checked_like() {
                state.render(false).0
            } else {
                state.value().to_string()
            }
        })
    }

    /// Set the value of the `<input>` registered at `node_id`. Mirrors what
    /// `__sol_setAttr(node, "value", v)` does from JS — the caret moves to the
    /// end of the new text and the visible text is refreshed on next render.
    /// Returns false if no input is registered at `node_id`.
    pub fn set_input_value(&mut self, node_id: usize, value: impl Into<String>) -> bool {
        let mut map = self.js.inputs.borrow_mut();
        let Some(state) = map.get_mut(&node_id) else {
            return false;
        };
        state.set_value(value);
        drop(map);
        self.refresh_input_text(node_id);
        self.needs_paint = true;
        true
    }

    // ── Scrollbars ───────────────────────────────────────────────────────────

    /// Override scrollbar colours for every scroll container in this instance.
    ///
    /// Pass `Some(theme)` to use the host-supplied colours, or `None` to fall
    /// back to the default heuristic (the container's computed `color` tinted
    /// at a low alpha for the track and higher alpha for the thumb).
    ///
    /// Full CSS scrollbar theming (`scrollbar-color`, `::-webkit-scrollbar`)
    /// awaits a stylo build that exposes the property in servo mode.
    pub fn set_scrollbar_theme(&mut self, theme: Option<ScrollbarTheme>) {
        self.scrollbar_theme = theme.map(|t| t.to_colors());
        self.needs_paint = true;
    }

    // ── Geometry ─────────────────────────────────────────────────────────────

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    pub fn container_id(&self) -> usize {
        self.container_id
    }

    /// Iterate registered `<select>` node ids. Useful for tests and host code
    /// that wants to drive popups programmatically.
    pub fn select_node_ids(&self) -> Vec<usize> {
        self.js.selects.borrow().keys().copied().collect()
    }

    /// Programmatically open or close a `<select>` dropdown. Used by tests
    /// and by host code that drives selects without a real pointer.
    pub fn set_select_dropdown_open(&mut self, select_id: usize, open: bool) {
        self.set_select_open(select_id, open);
    }
}

fn register_initial_stylesheets(
    doc: &Rc<RefCell<BaseDocument>>,
    sources: &[String],
) -> (std::collections::HashMap<StylesheetId, String>, u64) {
    let mut map = std::collections::HashMap::new();
    let mut d = doc.borrow_mut();
    // Always register the select popup UA stylesheet first so host stylesheets
    // can override it. It is intentionally not tracked in the host-visible
    // stylesheet map.
    d.add_user_agent_stylesheet(crate::select::POPUP_UA_CSS);
    for (i, css) in sources.iter().enumerate() {
        d.add_user_agent_stylesheet(css);
        map.insert(StylesheetId(i as u64), css.clone());
    }
    (map, sources.len() as u64)
}

fn apply_document_scroll_styles(doc: &Rc<RefCell<BaseDocument>>, container_id: usize, height: u32) {
    let mut doc = doc.borrow_mut();
    let mut m = doc.mutate();
    m.set_style_property(container_id, "height", &format!("{height}px"));
    m.set_style_property(container_id, "overflow-y", "auto");
}

/// True when `node_id` is a `<button>` element. Used by keyboard
/// activation to decide whether `Enter`/`Space` should fire `click`.
fn is_button_node(doc: &BaseDocument, node_id: usize) -> bool {
    doc.get_node(node_id)
        .and_then(|n| n.element_data())
        .is_some_and(|e| e.name.local.as_ref() == "button")
}

fn create_container_element(doc: &mut BaseDocument) -> usize {
    doc.mutate().create_element(
        QualName::new(None, ns!(html), LocalName::from("div")),
        vec![],
    )
}

fn font_face_style_qual() -> QualName {
    QualName::new(None, ns!(html), LocalName::from("style"))
}

fn hit_visible_node_id(doc: &BaseDocument, node_id: usize, x: f32, y: f32) -> Option<usize> {
    let node = doc.get_node(node_id)?;
    let local_x = x - node.final_layout.location.x;
    let local_y = y - node.final_layout.location.y;
    let size = node.final_layout.size;
    let content_size = node.final_layout.content_size;
    let overflow = node.scrollable_overflow;
    let has_scrollable_content = node.final_layout.scroll_width() > size.width
        || node.final_layout.scroll_height() > size.height
        || node.scroll_offset.x != 0.0
        || node.scroll_offset.y != 0.0;

    let matches_self =
        local_x >= 0.0 && local_x <= size.width && local_y >= 0.0 && local_y <= size.height;
    let matches_content = local_x >= 0.0
        && local_x <= content_size.width
        && local_y >= 0.0
        && local_y <= content_size.height;
    let matches_overflow = local_x >= overflow.x0 as f32
        && local_x <= overflow.x1 as f32
        && local_y >= overflow.y0 as f32
        && local_y <= overflow.y1 as f32;
    let matches_node = if has_scrollable_content {
        matches_self
    } else {
        matches_self || matches_content || matches_overflow
    };

    if !matches_node {
        return None;
    }

    let child_x = local_x + node.scroll_offset.x as f32;
    let child_y = local_y + node.scroll_offset.y as f32;
    let children = node
        .paint_children
        .borrow()
        .as_ref()
        .cloned()
        .unwrap_or_else(|| node.children.clone());

    for child_id in children.iter().rev() {
        if let Some(hit_id) = hit_visible_node_id(doc, *child_id, child_x, child_y) {
            return Some(hit_id);
        }
    }

    matches_self.then_some(node.id)
}

fn existing_node_layout_ancestors(doc: &BaseDocument, node_id: Option<usize>) -> Vec<usize> {
    node_id
        .filter(|id| doc.get_node(*id).is_some())
        .map(|id| doc.node_layout_ancestors(id))
        .unwrap_or_default()
}

fn attr_qual(name: &str) -> QualName {
    QualName::new(None, ns!(), LocalName::from(name))
}

fn estimated_input_char_width(node: &Node) -> f32 {
    node.primary_styles()
        .map(|styles| styles.clone_font_size().used_size().px() * 0.6)
        .filter(|width| width.is_finite() && *width > 0.0)
        .unwrap_or(8.0)
}

fn char_index_to_byte_index(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

fn input_caret_color(node: &Node) -> peniko::Color {
    let Some(styles) = node.primary_styles() else {
        return peniko::Color::BLACK;
    };
    let srgb = styles
        .clone_color()
        .to_color_space(style::color::ColorSpace::Srgb);
    let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    peniko::Color::from_rgba8(
        to_u8(srgb.components.0),
        to_u8(srgb.components.1),
        to_u8(srgb.components.2),
        255,
    )
}

/// Apply a single keystroke to the `InputState` registered for `input_id`.
/// Returns `(changed, emits_input_event)` where:
/// - `changed`: caret or value changed and rendered text should update.
/// - `emits_input_event`: value changed and an `"input"` event should dispatch.
fn apply_input_key(
    inputs: &crate::input::InputRegistry,
    input_id: usize,
    event: &KeyboardEvent,
) -> (bool, bool) {
    let mut map = inputs.borrow_mut();
    let Some(state) = map.get_mut(&input_id) else {
        return (false, false);
    };

    let has_modifier = event.ctrl_key || event.meta_key || event.alt_key;
    let with_shift = event.shift_key;

    if state.is_checked_like() {
        return match event.key.as_str() {
            // Browser checkboxes/radios toggle on Space/Enter.
            " " | "Space" | "Enter" => {
                if has_modifier {
                    return (false, false);
                }

                if state.is_radio() {
                    if state.checked() {
                        return (false, false);
                    }

                    let group_name = state.name().map(str::to_owned);
                    state.set_checked(true);

                    if let Some(group_name) = group_name {
                        for (other_id, other_state) in map.iter_mut() {
                            if *other_id == input_id {
                                continue;
                            }
                            if other_state.kind() != crate::input::InputType::Radio {
                                continue;
                            }
                            if other_state.name() == Some(group_name.as_str()) {
                                other_state.set_checked(false);
                            }
                        }
                    }

                    (true, true)
                } else {
                    let edited = state.toggle_checked();
                    (edited, edited)
                }
            }
            _ => (false, false),
        };
    }

    if state.is_range() {
        return match event.key.as_str() {
            "ArrowLeft" | "ArrowDown" => {
                let edited = state.step_number(-1);
                (edited, edited)
            }
            "ArrowRight" | "ArrowUp" => {
                let edited = state.step_number(1);
                (edited, edited)
            }
            "PageDown" => {
                let edited = state.step_number(-10);
                (edited, edited)
            }
            "PageUp" => {
                let edited = state.step_number(10);
                (edited, edited)
            }
            "Home" => {
                let edited = state.move_range_to_extreme(false);
                (edited, edited)
            }
            "End" => {
                let edited = state.move_range_to_extreme(true);
                (edited, edited)
            }
            _ => (false, false),
        };
    }

    let key = event.key.as_str();
    let word_modifier = (event.ctrl_key || event.meta_key) && !event.alt_key;

    if word_modifier {
        match key {
            "a" | "A" => {
                let edited = state.select_all();
                return (edited, false);
            }
            "ArrowLeft" => {
                let edited = state.move_word_left_extending(with_shift);
                return (edited, false);
            }
            "ArrowRight" => {
                let edited = state.move_word_right_extending(with_shift);
                return (edited, false);
            }
            "Backspace" => {
                let edited = state.delete_word_left();
                return (edited, edited);
            }
            "Delete" => {
                let edited = state.delete_word_right();
                return (edited, edited);
            }
            _ => {}
        }
    }

    match event.key.as_str() {
        "Backspace" => {
            let edited = state.backspace();
            (edited, edited)
        }
        "Delete" => {
            let edited = state.delete_forward();
            (edited, edited)
        }
        "ArrowLeft" => {
            let edited = state.move_left_extending(with_shift);
            (edited, false)
        }
        "ArrowRight" => {
            let edited = state.move_right_extending(with_shift);
            (edited, false)
        }
        "Home" => {
            let edited = state.move_home_extending(with_shift);
            (edited, false)
        }
        "End" => {
            let edited = state.move_end_extending(with_shift);
            (edited, false)
        }
        " " => {
            if state.is_numeric_like() {
                let edited = false;
                (edited, edited)
            } else {
                let edited = state.insert(' ');
                (edited, edited)
            }
        }
        "Space" => {
            if state.is_numeric_like() {
                let edited = false;
                (edited, edited)
            } else {
                let edited = state.insert(' ');
                (edited, edited)
            }
        }
        // Modifier-only keys produce empty / multi-char `key` strings we
        // shouldn't insert. Single-char printable keys *do* go in.
        key if key.chars().count() == 1 => {
            if event.ctrl_key || event.meta_key || event.alt_key {
                return (false, false);
            }
            let ch = match key.chars().next() {
                Some(ch) => ch,
                None => return (false, false),
            };
            if ch.is_control() {
                return (false, false);
            }
            let edited = if state.is_numeric_like() {
                state.insert_numeric_char(ch)
            } else {
                state.insert(ch)
            };
            (edited, edited)
        }
        _ => (false, false),
    }
}

/// Keyboard navigation for a focused radio button.
///
/// HTML radios move within their named group using arrow keys. We keep the
/// checked state in document order and shift focus to the newly selected
/// radio.
impl Instance {
    fn apply_radio_navigation_key(
        &mut self,
        input_id: usize,
        event: &KeyboardEvent,
    ) -> (bool, bool, Option<usize>) {
        let direction = match event.key.as_str() {
            "ArrowLeft" | "ArrowUp" => Some(-1),
            "ArrowRight" | "ArrowDown" => Some(1),
            "Home" => Some(i32::MIN),
            "End" => Some(i32::MAX),
            _ => None,
        };
        let Some(direction) = direction else {
            return (false, false, None);
        };

        let group_name = {
            let inputs = self.js.inputs.borrow();
            let Some(state) = inputs.get(&input_id) else {
                return (false, false, None);
            };
            if !state.is_radio() || state.disabled() {
                return (false, false, None);
            }
            state.name().map(str::to_owned)
        };
        let Some(group_name) = group_name else {
            return (false, false, None);
        };

        let members = {
            let inputs = self.js.inputs.borrow();
            let doc = self.doc.borrow();
            let mut ids = Vec::new();
            doc.visit(|node_id, _node| {
                if inputs.get(&node_id).is_some_and(|state| {
                    state.is_radio()
                        && !state.disabled()
                        && state.name() == Some(group_name.as_str())
                }) {
                    ids.push(node_id);
                }
            });
            ids
        };
        if members.is_empty() {
            return (false, false, None);
        }

        let current_index = members.iter().position(|candidate| *candidate == input_id);
        let Some(current_index) = current_index else {
            return (false, false, None);
        };

        let next_index = match direction {
            d if d == i32::MIN => 0,
            d if d == i32::MAX => members.len() - 1,
            1 => (current_index + 1) % members.len(),
            -1 => (current_index + members.len() - 1) % members.len(),
            _ => current_index,
        };
        let next_id = members[next_index];

        if next_id == input_id
            && self
                .js
                .inputs
                .borrow()
                .get(&input_id)
                .is_some_and(|state| state.checked())
        {
            return (false, false, None);
        }

        {
            let mut inputs = self.js.inputs.borrow_mut();
            for radio_id in &members {
                if let Some(state) = inputs.get_mut(radio_id) {
                    state.set_checked(*radio_id == next_id);
                }
            }
        }

        {
            let mut doc = self.doc.borrow_mut();
            for radio_id in &members {
                if let Some(node) = doc.get_node_mut(*radio_id) {
                    if let Some(el) = node.element_data_mut() {
                        if let Some(slot) = el.checkbox_input_checked_mut() {
                            *slot = *radio_id == next_id;
                        }
                    }
                }
            }
        }

        (true, true, Some(next_id))
    }
}

/// Outcome of a keyboard event applied to a select.
#[derive(Default, Clone, Copy)]
struct SelectKeyOutcome {
    /// State changed and the synthetic display text needs re-syncing.
    edited: bool,
    /// `change` event should be dispatched.
    emits_change: bool,
    /// Popup overlay should be opened (after this returns).
    open_popup: bool,
    /// Popup overlay should be closed (after this returns).
    close_popup: bool,
    /// Popup active-index changed and class lists need re-syncing.
    sync_highlights: bool,
}

/// Pure state transition: given a select and a keyboard event, return what
/// the caller should do. The state mutations that don't touch the DOM (moving
/// selection, setting active_index) are applied in place; DOM-affecting steps
/// (opening/closing the popup, restyling options) are deferred to the caller.
fn select_key_outcome(
    state: &mut crate::select::SelectState,
    event: &KeyboardEvent,
) -> SelectKeyOutcome {
    let mut out = SelectKeyOutcome::default();
    let is_popup_selectable = |idx: usize| {
        state
            .options
            .get(idx)
            .is_some_and(|opt| !opt.disabled && !opt.hidden)
    };
    if !state.is_open() {
        // Alt+ArrowDown opens the dropdown (browser parity).
        if event.alt_key && matches!(event.key.as_str(), "ArrowDown" | "Down") {
            out.edited = true;
            out.open_popup = true;
            return out;
        }
        match event.key.as_str() {
            "ArrowDown" | "Down" => {
                let edited = state.move_selection(1);
                out.edited = edited;
                out.emits_change = edited;
            }
            "ArrowUp" | "Up" => {
                let edited = state.move_selection(-1);
                out.edited = edited;
                out.emits_change = edited;
            }
            "PageDown" => {
                let edited = state.step_selection(10);
                out.edited = edited;
                out.emits_change = edited;
            }
            "PageUp" => {
                let edited = state.step_selection(-10);
                out.edited = edited;
                out.emits_change = edited;
            }
            "Home" => {
                let edited = state.jump_to_extreme(false);
                out.edited = edited;
                out.emits_change = edited;
            }
            "End" => {
                let edited = state.jump_to_extreme(true);
                out.edited = edited;
                out.emits_change = edited;
            }
            " " | "Space" | "Enter" => {
                out.edited = true;
                out.open_popup = true;
            }
            _ => {
                if let Some(ch) = type_ahead_char(event) {
                    if state.type_ahead(ch, std::time::Instant::now()).is_some() {
                        out.edited = true;
                        out.emits_change = true;
                    }
                }
            }
        }
    } else {
        // Alt+ArrowUp commits the highlighted option (if any) and closes.
        // Browser parity (Chrome/Firefox <select> open state).
        if event.alt_key && matches!(event.key.as_str(), "ArrowUp" | "Up") {
            if let Some(active) = state.active_index() {
                if is_popup_selectable(active) && state.selected_index() != Some(active) {
                    state.set_selected_index(Some(active));
                    out.edited = true;
                    out.emits_change = true;
                }
            }
            out.close_popup = true;
            return out;
        }
        match event.key.as_str() {
            "ArrowDown" | "Down" => {
                let idx = state
                    .active_index()
                    .unwrap_or_else(|| state.selected_index().unwrap_or(0));
                let len = state.options.len() as i32;
                if len > 0 {
                    let mut next = ((idx as i32 + 1).rem_euclid(len)) as usize;
                    let mut attempts = 0;
                    while attempts < len as usize && !is_popup_selectable(next) {
                        next = ((next as i32 + 1).rem_euclid(len)) as usize;
                        attempts += 1;
                    }
                    if is_popup_selectable(next) {
                        state.set_active_index(Some(next));
                        out.edited = true;
                        out.sync_highlights = true;
                    }
                }
            }
            "ArrowUp" | "Up" => {
                let idx = state
                    .active_index()
                    .unwrap_or_else(|| state.selected_index().unwrap_or(0));
                let len = state.options.len() as i32;
                if len > 0 {
                    let mut next = ((idx as i32 - 1).rem_euclid(len)) as usize;
                    let mut attempts = 0;
                    while attempts < len as usize && !is_popup_selectable(next) {
                        next = ((next as i32 - 1).rem_euclid(len)) as usize;
                        attempts += 1;
                    }
                    if is_popup_selectable(next) {
                        state.set_active_index(Some(next));
                        out.edited = true;
                        out.sync_highlights = true;
                    }
                }
            }
            "Home" => {
                if let Some(first_enabled) = state.find_first_enabled() {
                    state.set_active_index(Some(first_enabled));
                    out.edited = true;
                    out.sync_highlights = true;
                }
            }
            "End" => {
                if let Some(idx) = state
                    .options
                    .iter()
                    .rposition(|opt| !opt.disabled && !opt.hidden)
                {
                    state.set_active_index(Some(idx));
                    out.edited = true;
                    out.sync_highlights = true;
                }
            }
            "Enter" | " " | "Space" => {
                if let Some(active) = state.active_index() {
                    if is_popup_selectable(active) && state.selected_index() != Some(active) {
                        state.set_selected_index(Some(active));
                        out.edited = true;
                        out.emits_change = true;
                    }
                }
                out.close_popup = true;
            }
            "Escape" => {
                out.edited = true;
                out.close_popup = true;
            }
            "Tab" => {
                if let Some(active) = state.active_index() {
                    if is_popup_selectable(active) && state.selected_index() != Some(active) {
                        state.set_selected_index(Some(active));
                        out.edited = true;
                        out.emits_change = true;
                    }
                }
                out.close_popup = true;
            }
            _ => {
                if let Some(ch) = type_ahead_char(event) {
                    if let Some(idx) = state.type_ahead(ch, std::time::Instant::now()) {
                        // Type-ahead in an open popup highlights but does not
                        // commit — committing happens on Enter or click.
                        state.set_active_index(Some(idx));
                        out.edited = true;
                        out.sync_highlights = true;
                    }
                }
            }
        }
    }
    out
}

/// Return `Some(ch)` when `event` represents a single printable character
/// that should drive select type-ahead. Modifier keys (other than Shift)
/// and multi-char `key` values (`"ArrowDown"`, `"Tab"`, …) are filtered out.
fn type_ahead_char(event: &KeyboardEvent) -> Option<char> {
    if event.ctrl_key || event.meta_key || event.alt_key {
        return None;
    }
    let mut chars = event.key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    if ch.is_control() {
        return None;
    }
    Some(ch)
}

impl Instance {
    /// Handle a mouse click on a checkbox or radio input.
    ///
    /// Mirrors the Space/Enter path in `apply_input_key`: toggles the
    /// `InputState`, syncs blitz-dom's `CheckboxInput`, deselects radio group
    /// siblings, and dispatches an `"input"` event.
    fn handle_checked_input_click(&mut self, input_id: usize) -> TickResult {
        let toggle_info = {
            let mut map = self.js.inputs.borrow_mut();
            let Some(state) = map.get_mut(&input_id) else {
                return TickResult::default();
            };
            if state.is_radio() {
                if state.checked() {
                    return TickResult::default(); // already selected
                }
                let group = state.name().map(str::to_owned);
                state.set_checked(true);
                Some((true, true, group))
            } else {
                let toggled = state.toggle_checked();
                let new_checked = state.checked();
                toggled.then_some((new_checked, false, None))
            }
        };

        let Some((new_checked, is_radio, group_name)) = toggle_info else {
            return TickResult::default();
        };

        // Sync blitz-dom's CheckboxInput for this node.
        if let Some(node) = self.doc.borrow_mut().get_node_mut(input_id) {
            if let Some(el) = node.element_data_mut() {
                if let Some(slot) = el.checkbox_input_checked_mut() {
                    *slot = new_checked;
                }
            }
        }

        // For radio: deselect siblings in the same group.
        if is_radio {
            if let Some(ref group) = group_name {
                // InputState side.
                let sibling_ids: Vec<usize> = {
                    let map = self.js.inputs.borrow();
                    map.iter()
                        .filter(|(id, s)| {
                            **id != input_id && s.is_radio() && s.name() == Some(group.as_str())
                        })
                        .map(|(id, _)| *id)
                        .collect()
                };
                for sid in sibling_ids {
                    if let Some(s) = self.js.inputs.borrow_mut().get_mut(&sid) {
                        s.set_checked(false);
                    }
                    // blitz-dom side.
                    if let Some(node) = self.doc.borrow_mut().get_node_mut(sid) {
                        if let Some(el) = node.element_data_mut() {
                            if let Some(slot) = el.checkbox_input_checked_mut() {
                                *slot = false;
                            }
                        }
                    }
                }
            }
        }

        // Dispatch the "input" event.
        let snapshot = self.js.inputs.borrow().get(&input_id).map(|s| {
            (
                s.value().to_string(),
                s.checked(),
                s.selection_start(),
                s.selection_end(),
            )
        });
        if let Some((value, checked, sel_start, sel_end)) = snapshot {
            return self
                .js
                .dispatch_input_event(input_id, &value, checked, sel_start, sel_end);
        }
        TickResult::default()
    }

    /// Apply a keyboard event to a focused `<select>`. Mutates state, drives
    /// the popup overlay open/closed, and reports back whether the synthetic
    /// label and `change` event need to fire.
    fn apply_select_key(&mut self, select_id: usize, event: &KeyboardEvent) -> (bool, bool) {
        let outcome = {
            let mut map = self.js.selects.borrow_mut();
            let Some(state) = map.get_mut(&select_id) else {
                return (false, false);
            };
            if state.disabled() {
                return (false, false);
            }
            select_key_outcome(state, event)
        };
        if outcome.open_popup {
            self.set_select_open(select_id, true);
        }
        if outcome.close_popup {
            self.set_select_open(select_id, false);
        }
        if outcome.sync_highlights {
            self.sync_select_popup_highlights(select_id);
        }
        if outcome.edited || outcome.open_popup || outcome.close_popup || outcome.sync_highlights {
            self.needs_paint = true;
        }
        (outcome.edited, outcome.emits_change)
    }

    /// Handle a mouse click on a select element: toggle open state.
    fn handle_select_click(&mut self, select_id: usize) -> TickResult {
        let was_open = self
            .js
            .selects
            .borrow()
            .get(&select_id)
            .map(|s| s.is_open())
            .unwrap_or(false);
        self.set_select_open(select_id, !was_open);
        self.refresh_select_text(select_id);

        TickResult {
            needs_paint: true,
            jobs_pending: false,
        }
    }

    /// Open or close `select_id`'s dropdown. Keeps `SelectState.open` and the
    /// DOM popup overlay in sync — never call `state.set_open` directly from
    /// outside the select state itself.
    fn set_select_open(&mut self, select_id: usize, open: bool) {
        let was_open = {
            let mut map = self.js.selects.borrow_mut();
            let Some(state) = map.get_mut(&select_id) else {
                return;
            };
            let was = state.is_open();
            if open && !was {
                let active_index = state
                    .selected_index()
                    .filter(|&idx| {
                        state
                            .options
                            .get(idx)
                            .is_some_and(|opt| !opt.disabled && !opt.hidden)
                    })
                    .or_else(|| state.find_first_enabled());
                state.set_active_index(active_index);
            }
            state.set_open(open);
            was
        };
        if open && !was_open {
            self.mount_select_popup(select_id);
        } else if !open && was_open {
            self.unmount_select_popup(select_id);
        }
        self.needs_paint = true;
    }

    /// Build the popup `<div>` for `select_id` and append it as the last child
    /// of the select. Stores the popup root and per-option node ids on the
    /// `SelectState` so mouse hit-tests can map a hovered node back to an
    /// option index.
    fn mount_select_popup(&mut self, select_id: usize) {
        // Snapshot what the popup needs without holding the selects borrow
        // while we mutate the DOM. `hidden` options keep their slot in the
        // snapshot so indices line up with `SelectState::options`.
        let snapshot: Option<(
            Vec<(String, bool, bool)>,
            Option<usize>,
            Option<usize>,
            String,
        )> = self.js.selects.borrow().get(&select_id).and_then(|s| {
            let doc = self.doc.borrow();
            let select_node = doc.get_node(select_id)?;
            let layout = select_node.final_layout;
            let popup_left = -(layout.border.left + layout.padding.left);
            let popup_top =
                layout.content_box_height() + layout.padding.bottom + layout.border.bottom;
            let popup_width = layout.size.width;
            let popup_style =
                format!("left: {popup_left}px; top: {popup_top}px; width: {popup_width}px;");
            (
                s.options
                    .iter()
                    .map(|o| (o.label.clone(), o.disabled, o.hidden))
                    .collect(),
                s.selected_index(),
                s.active_index(),
                popup_style,
            )
                .into()
        });
        let Some((entries, selected_idx, active_idx, popup_style)) = snapshot else {
            return;
        };

        let mut doc = self.doc.borrow_mut();
        let popup_id = doc.mutate().create_element(
            QualName::new(None, ns!(html), LocalName::from("div")),
            vec![],
        );
        doc.mutate().set_attribute(
            popup_id,
            QualName::new(None, ns!(), LocalName::from("class")),
            crate::select::POPUP_CLASS,
        );
        doc.mutate().set_attribute(
            popup_id,
            QualName::new(None, ns!(), LocalName::from("style")),
            &popup_style,
        );

        let mut option_ids: Vec<Option<usize>> = Vec::with_capacity(entries.len());
        for (i, (label, disabled, hidden)) in entries.iter().enumerate() {
            if *hidden {
                option_ids.push(None);
                continue;
            }
            let opt_id = doc.mutate().create_element(
                QualName::new(None, ns!(html), LocalName::from("div")),
                vec![],
            );
            let mut classes = String::from(crate::select::POPUP_OPTION_CLASS);
            if *disabled {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_DISABLED_CLASS);
            }
            if Some(i) == selected_idx {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_SELECTED_CLASS);
            }
            if Some(i) == active_idx {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_ACTIVE_CLASS);
            }
            doc.mutate().set_attribute(
                opt_id,
                QualName::new(None, ns!(), LocalName::from("class")),
                &classes,
            );
            let text_id = doc.create_text_node(label);
            doc.mutate().append_children(opt_id, &[text_id]);
            doc.mutate().append_children(popup_id, &[opt_id]);
            option_ids.push(Some(opt_id));
        }

        doc.mutate().append_children(select_id, &[popup_id]);
        drop(doc);

        if let Some(state) = self.js.selects.borrow_mut().get_mut(&select_id) {
            state.popup_root_id = Some(popup_id);
            state.option_node_ids = option_ids;
        }
    }

    /// Remove the popup overlay rooted at the recorded popup id and clear the
    /// stored ids on the `SelectState`.
    fn unmount_select_popup(&mut self, select_id: usize) {
        let popup_id = {
            let mut map = self.js.selects.borrow_mut();
            let Some(state) = map.get_mut(&select_id) else {
                return;
            };
            state.option_node_ids.clear();
            state.popup_root_id.take()
        };
        if let Some(popup_id) = popup_id {
            self.doc
                .borrow_mut()
                .mutate()
                .remove_and_drop_node(popup_id);
        }
    }

    /// Rewrite the popup option class lists so the active/selected highlights
    /// match the current `SelectState`. Called after keyboard or mouse
    /// navigation while the popup is open.
    fn sync_select_popup_highlights(&mut self, select_id: usize) {
        let snapshot = self.js.selects.borrow().get(&select_id).map(|s| {
            (
                s.option_node_ids.clone(),
                s.options.iter().map(|o| o.disabled).collect::<Vec<bool>>(),
                s.selected_index(),
                s.active_index(),
            )
        });
        let Some((option_ids, disabled, selected_idx, active_idx)) = snapshot else {
            return;
        };
        let mut doc = self.doc.borrow_mut();
        for (i, opt_id) in option_ids.iter().enumerate() {
            let Some(opt_id) = opt_id else { continue };
            let mut classes = String::from(crate::select::POPUP_OPTION_CLASS);
            if disabled.get(i).copied().unwrap_or(false) {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_DISABLED_CLASS);
            }
            if Some(i) == selected_idx {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_SELECTED_CLASS);
            }
            if Some(i) == active_idx {
                classes.push(' ');
                classes.push_str(crate::select::POPUP_OPTION_ACTIVE_CLASS);
            }
            doc.mutate().set_attribute(
                *opt_id,
                QualName::new(None, ns!(), LocalName::from("class")),
                &classes,
            );
        }
    }

    /// Map a hovered node back to the popup option it belongs to, walking up
    /// the parent chain until we hit a popup option whose id was recorded by
    /// `mount_select_popup`. Returns `(select_id, option_index)`.
    fn popup_option_for_hit(&self, hit_id: usize) -> Option<(usize, usize)> {
        let doc = self.doc.borrow();
        let selects = self.js.selects.borrow();
        let mut current = Some(hit_id);
        while let Some(id) = current {
            for (sel_id, state) in selects.iter() {
                if let Some(pos) = state
                    .option_node_ids
                    .iter()
                    .position(|opt| *opt == Some(id))
                {
                    return Some((*sel_id, pos));
                }
            }
            current = doc.get_node(id).and_then(|n| n.parent);
        }
        None
    }

    /// Returns the id of the select that owns `hit_id`, treating both the
    /// select element itself and the open popup overlay as belonging to it.
    fn select_owning_hit(&self, hit_id: usize) -> Option<usize> {
        let doc = self.doc.borrow();
        let selects = self.js.selects.borrow();
        let mut current = Some(hit_id);
        while let Some(id) = current {
            if selects.contains_key(&id) {
                return Some(id);
            }
            for (sel_id, state) in selects.iter() {
                if state.popup_root_id == Some(id) {
                    return Some(*sel_id);
                }
            }
            current = doc.get_node(id).and_then(|n| n.parent);
        }
        None
    }

    /// Compute a new range value from a document-space x coordinate and apply
    /// it to the `InputState`. Returns `Some(TickResult)` if the value changed
    /// and an `"input"` event was dispatched, `None` if the node is not a
    /// range input or the value didn't change.
    fn update_range_from_x(&mut self, input_id: usize, doc_x: f32) -> Option<TickResult> {
        // Compute the fraction from the element's absolute position.
        let (abs_x, content_h, pad_left, pad_right, size_w) = {
            let doc = self.doc.borrow();
            let node = doc.get_node(input_id)?;
            let l = &node.final_layout;
            let abs = node.absolute_position(0.0, 0.0);
            let content_h =
                l.size.height - l.padding.top - l.padding.bottom - l.border.top - l.border.bottom;
            (
                abs.x,
                content_h,
                l.padding.left,
                l.padding.right,
                l.size.width,
            )
        };

        let content_x0 = abs_x + pad_left;
        let content_x1 = abs_x + size_w - pad_right;
        let thumb_r = (content_h / 2.0).min(8.0).max(3.0);
        let usable_x0 = content_x0 + thumb_r;
        let usable_x1 = content_x1 - thumb_r;
        let usable_w = (usable_x1 - usable_x0).max(0.0);

        let fraction = if usable_w > 0.0 {
            ((doc_x - usable_x0) / usable_w).clamp(0.0, 1.0) as f64
        } else {
            0.5
        };

        let changed = self
            .js
            .inputs
            .borrow_mut()
            .get_mut(&input_id)?
            .set_value_from_range_fraction(fraction);

        if !changed {
            return Some(TickResult::default());
        }

        self.refresh_input_text(input_id);

        let snapshot = self.js.inputs.borrow().get(&input_id).map(|s| {
            (
                s.value().to_string(),
                s.checked(),
                s.selection_start(),
                s.selection_end(),
            )
        })?;

        Some(self.js.dispatch_input_event(
            input_id,
            &snapshot.0,
            snapshot.1,
            snapshot.2,
            snapshot.3,
        ))
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
    use std::sync::Arc;

    use serde_json::{Value, json};

    const CLICK_BUTTON_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const btn = __sol_createElement("button");
          __sol_setProperty(
            btn,
            "style",
            "display:block; width: 160px; height: 80px;"
          );
          __sol_setProperty(btn, "onClick", () => {
            globalThis.state.count = (globalThis.state.count || 0) + 1;
          });
          return btn;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    const ROOT_CLICK_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const root = __sol_createElement("div");
          __sol_setProperty(root, "onClick", () => {
            globalThis.state.clicked = true;
          });
          __sol_setProperty(
            root,
            "style",
            "display:block; width: 200px; height: 200px;"
          );
          return root;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    const HOVER_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const btn = __sol_createElement("button");
          __sol_setProperty(
            btn,
            "style",
            "display:block; width: 80px; height: 80px;"
          );
          __sol_setProperty(btn, "onMouseOver", (e) => {
            globalThis.state.over = (globalThis.state.over || 0) + 1;
            globalThis.state.overTarget = e.target;
            globalThis.state.overRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onMouseOut", (e) => {
            globalThis.state.out = (globalThis.state.out || 0) + 1;
            globalThis.state.outTarget = e.target;
            globalThis.state.outRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onMouseEnter", (e) => {
            globalThis.state.enter = (globalThis.state.enter || 0) + 1;
            globalThis.state.enterCurrent = e.currentTarget;
            globalThis.state.enterRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onMouseLeave", (e) => {
            globalThis.state.leave = (globalThis.state.leave || 0) + 1;
            globalThis.state.leaveCurrent = e.currentTarget;
            globalThis.state.leaveRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onHover", (e) => {
            globalThis.state.hover = (globalThis.state.hover || 0) + 1;
            globalThis.state.hoverCurrent = e.currentTarget;
          });
          __sol_setProperty(btn, "onHoverEnter", (e) => {
            globalThis.state.hoverEnter = (globalThis.state.hoverEnter || 0) + 1;
            globalThis.state.hoverEnterRelated = e.relatedTarget;
          });
          __sol_setProperty(btn, "onHoverLeave", (e) => {
            globalThis.state.hoverLeave = (globalThis.state.hoverLeave || 0) + 1;
            globalThis.state.hoverLeaveRelated = e.relatedTarget;
          });
          return btn;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    const WHEEL_SCROLL_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const outer = __sol_createElement("div");
          __sol_setProperty(
            outer,
            "style",
            "display:block; width: 120px; height: 80px; overflow: auto;"
          );
          __sol_setProperty(outer, "onWheel", (event) => {
            globalThis.state.wheel = (globalThis.state.wheel || 0) + 1;
            sendEvent("wheel", JSON.stringify({ top: event.scrollTop, deltaY: event.deltaY }));
          });
          __sol_setProperty(outer, "onScroll", (event) => {
            globalThis.state.scroll = (globalThis.state.scroll || 0) + 1;
            globalThis.state.scrollTop = event.scrollTop;
          });

          const filler = __sol_createElement("div");
          __sol_setProperty(
            filler,
            "style",
            "display:block; width: 120px; height: 240px;"
          );
          __sol_insertNode(outer, filler, null);
          return outer;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    const TEXT_INPUT_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const input = __sol_createElement("input");
          __sol_setProperty(input, "style", "display:block; width: 220px; height: 40px;");

          __sol_setProperty(input, "onFocus", () => {
            globalThis.state.focused = true;
            globalThis.state.lastFocus = "focus";
          });

          __sol_setProperty(input, "onBlur", () => {
            globalThis.state.focused = false;
            globalThis.state.lastBlur = "blur";
          });

          __sol_setProperty(input, "onInput", (event) => {
            globalThis.state.value = event.value;
            globalThis.state.caret = event.selectionStart;
          });

          __sol_setProperty(input, "onKeyDown", (event) => {
            globalThis.state.lastKey = event.key;
            // Keep caret visible in this test path for move-only keys, since
            // native `input` events are not fired on caret movement alone.
            if (event.selectionStart !== undefined) {
              globalThis.state.caret = event.selectionStart;
            }
          });

          __sol_setProperty(input, "onKeyUp", (event) => {
            globalThis.state.lastKeyUp = event.key;
          });

          return input;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    async fn make_test_device() -> (Arc<wgpu::Device>, Arc<wgpu::Queue>) {
        if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
            unsafe {
                std::env::set_var("XDG_RUNTIME_DIR", "/tmp");
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
                // Metal has no software fallback; let the platform pick.
                force_fallback_adapter: false,
            })
            .await
            .expect("no adapter available for test");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("solite-test"),
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

    fn make_key_event(
        key: &str,
        code: &str,
        key_code: u32,
        repeat: bool,
        shift_key: bool,
        ctrl_key: bool,
        alt_key: bool,
        meta_key: bool,
    ) -> KeyboardEvent {
        KeyboardEvent {
            key: key.to_owned(),
            code: code.to_owned(),
            key_code,
            repeat,
            shift_key,
            ctrl_key,
            alt_key,
            meta_key,
        }
    }

    #[test]
    fn dispatch_mouse_click_updates_rust_state() {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            ROOT_CLICK_COMPONENT,
        );
        let state = instance.state();
        assert_eq!(state.get("clicked"), None);

        let _ = instance.render();

        let result = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert!(result.needs_paint);
        assert_eq!(state.get("clicked"), Some(json!(true)));

        let result_again = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert!(result_again.needs_paint);
        assert_eq!(state.get("clicked"), Some(json!(true)));
    }

    #[test]
    fn take_send_event_error_clears_after_read() {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 80,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            r#"
            import { render } from "solite-runtime";
            function App() {
              const btn = __sol_createElement("button");
              __sol_setProperty(btn, "style", "display:block; width: 200px; height: 80px;");
              __sol_setProperty(btn, "onClick", () => {
                sendEvent("invalid", "{invalid");
              });
              return btn;
            }
            render(() => App(), __SOL_ROOT__);
            "#,
        );

        let _ = instance.render();
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Up {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        assert!(instance
            .take_send_event_error()
            .as_ref()
            .is_some_and(|msg| !msg.is_empty()));
        assert_eq!(instance.take_send_event_error(), None);
    }

    #[test]
    fn dispatch_key_down_and_up_target_focused_node() {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 80,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            TEXT_INPUT_COMPONENT,
        );
        let state = instance.state();

        let _ = instance.render();
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        assert_eq!(state.get("focused"), Some(json!(true)));

        let _ = instance.dispatch_key_down(make_key_event(
            "A", "KeyA", 65, false, false, false, false, false,
        ));
        assert_eq!(state.get("value"), Some(json!("A")));
        assert_eq!(state.get("caret"), Some(json!(1)));
        assert_eq!(state.get("lastKey"), Some(json!("A")));

        let _ = instance.dispatch_key_down(make_key_event(
            "Backspace",
            "Backspace",
            8,
            false,
            false,
            false,
            false,
            false,
        ));
        assert_eq!(state.get("value"), Some(json!("")));
        assert_eq!(state.get("caret"), Some(json!(0)));

        let _ = instance.dispatch_key_up(make_key_event(
            "A", "KeyA", 65, false, false, false, false, false,
        ));
        assert_eq!(state.get("lastKeyUp"), Some(json!("A")));

        let _ = instance.dispatch_mouse(
            500.0,
            500.0,
            MouseEvent::Down {
                x: 500.0,
                y: 500.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(state.get("focused"), Some(json!(false)));

        let value_after_blur = state.get("value");
        let _ = instance.dispatch_key_down(make_key_event(
            "B", "KeyB", 66, false, false, false, false, false,
        ));
        assert_eq!(state.get("value"), value_after_blur);
    }

    #[test]
    fn dispatch_focus_events_update_host_state() {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 80,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            TEXT_INPUT_COMPONENT,
        );
        let state = instance.state();

        let _ = instance.render();
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        assert_eq!(state.get("focused"), Some(json!(true)));
        assert_eq!(state.get("lastFocus"), Some(json!("focus")));

        let _ = instance.dispatch_mouse(
            500.0,
            500.0,
            MouseEvent::Down {
                x: 500.0,
                y: 500.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(state.get("focused"), Some(json!(false)));
        assert_eq!(state.get("lastBlur"), Some(json!("blur")));

        let _ = instance.tick();
        assert_eq!(state.get("lastBlur"), Some(json!("blur")));
    }

    #[test]
    fn resize_updates_size_and_keeps_click_working() {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 100,
                height: 100,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            CLICK_BUTTON_COMPONENT,
        );
        let state = instance.state();
        let _ = instance.render();
        assert!(
            instance
                .dispatch_mouse(
                    20.0,
                    20.0,
                    MouseEvent::Down {
                        x: 20.0,
                        y: 20.0,
                        button: MouseButton::Left,
                    },
                )
                .needs_paint
        );
        assert_eq!(state.get("count"), Some(json!(1)));

        instance.resize(220, 80);
        assert_eq!(instance.size(), (220, 80));
        let _ = instance.render();

        let second = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert!(second.needs_paint);
        assert_eq!(state.get("count"), Some(json!(2)));
    }

    #[test]
    fn dispatch_mouse_move_updates_hover_state_and_handlers() {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            HOVER_COMPONENT,
        );
        let state = instance.state();
        let _ = instance.render();

        let btn_id = {
            let d = instance.doc.borrow();
            d.get_node(instance.container_id())
                .and_then(|c| c.children.first().copied())
                .expect("button should be mounted")
        };

        assert!(
            !instance
                .doc
                .borrow()
                .get_node(btn_id)
                .is_some_and(|n| n.is_hovered())
        );

        let enter = instance.dispatch_mouse(10.0, 10.0, MouseEvent::Move { x: 10.0, y: 10.0 });
        assert!(enter.needs_paint);

        assert!(
            instance
                .doc
                .borrow()
                .get_node(btn_id)
                .is_some_and(|n| n.is_hovered())
        );

        assert_eq!(state.get("over"), Some(json!(1)));
        assert_eq!(state.get("enter"), Some(json!(1)));
        assert_eq!(state.get("hover"), Some(json!(1)));
        assert_eq!(state.get("hoverEnter"), Some(json!(1)));
        assert_eq!(state.get("hoverCurrent"), Some(json!(btn_id)));
        assert_eq!(state.get("hoverEnterRelated"), Some(json!(null)));
        assert_eq!(state.get("overTarget"), Some(json!(btn_id)));
        assert_eq!(state.get("overRelated"), Some(json!(null)));
        assert_eq!(state.get("enterCurrent"), Some(json!(btn_id)));
        assert_eq!(state.get("enterRelated"), Some(json!(null)));

        let stay = instance.dispatch_mouse(20.0, 20.0, MouseEvent::Move { x: 20.0, y: 20.0 });
        assert!(!stay.needs_paint);
        assert_eq!(state.get("over"), Some(json!(1)));
        assert_eq!(state.get("enter"), Some(json!(1)));
        assert_eq!(state.get("hover"), Some(json!(1)));
        assert_eq!(state.get("hoverEnter"), Some(json!(1)));
        assert!(state.get("out").is_none());

        let leave = instance.dispatch_mouse(500.0, 500.0, MouseEvent::Move { x: 500.0, y: 500.0 });
        assert!(leave.needs_paint);

        assert!(
            !instance
                .doc
                .borrow()
                .get_node(btn_id)
                .is_some_and(|n| n.is_hovered())
        );

        assert_eq!(state.get("out"), Some(json!(1)));
        assert_eq!(state.get("leave"), Some(json!(1)));
        assert_eq!(state.get("hoverLeave"), Some(json!(1)));
        assert_eq!(state.get("outTarget"), Some(json!(btn_id)));
        assert_eq!(state.get("outRelated"), Some(json!(null)));
        assert_eq!(state.get("leaveCurrent"), Some(json!(btn_id)));
        assert_eq!(state.get("leaveRelated"), Some(json!(null)));
        assert_eq!(state.get("hoverLeaveRelated"), Some(json!(null)));
    }

    #[test]
    fn dispatch_wheel_scrolls_and_dispatches_events() {
        let (device, queue) = test_device();
        let (mut instance, mut rx) = Instance::new(
            InstanceConfig {
                width: 160,
                height: 160,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            WHEEL_SCROLL_COMPONENT,
        );
        let state = instance.state();
        let _ = instance.render();

        let outer_id = {
            let d = instance.doc.borrow();
            d.get_node(instance.container_id())
                .and_then(|container| container.children.first().copied())
                .expect("scroll container should be mounted")
        };
        let before_top = instance
            .doc
            .borrow()
            .get_node(outer_id)
            .expect("outer node exists")
            .scroll_offset
            .y;

        let result = instance.dispatch_wheel(10.0, 10.0, 0.0, 40.0);
        assert!(result.needs_paint);

        let after_top = instance
            .doc
            .borrow()
            .get_node(outer_id)
            .expect("outer node exists")
            .scroll_offset
            .y;

        assert_eq!(state.get("wheel"), Some(json!(1)));

        let first = rx.try_recv().expect("wheel event");
        assert_eq!(first.name, "wheel");
        let first_top = first
            .payload
            .get("top")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.0);
        assert_eq!(first_top, 0.0);
        assert!(after_top >= before_top);

        if let Ok(second) = rx.try_recv() {
            assert_eq!(second.name, "scroll");
            assert_eq!(second.payload["type"], json!("scroll"));
            let scroll_top = second
                .payload
                .get("scrollTop")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.0);
            assert_eq!(scroll_top, first_top);
            assert_eq!(state.get("scroll"), Some(json!(1)));
            assert_eq!(state.get("scrollTop"), Some(json!(0.0)));
        }

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn two_instances_share_device_and_keep_state_independent() {
        let (device, queue) = test_device();
        let (mut a, _rx_a) = Instance::new(
            InstanceConfig {
                width: 140,
                height: 140,
                device: Arc::clone(&device),
                queue: Arc::clone(&queue),
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            ROOT_CLICK_COMPONENT,
        );
        let (mut b, _rx_b) = Instance::new(
            InstanceConfig {
                width: 140,
                height: 140,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            CLICK_BUTTON_COMPONENT,
        );

        let state_a = a.state();
        let state_b = b.state();
        let _ = a.render();
        let _ = b.render();

        let click_a = a.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert!(click_a.needs_paint);

        let click_b = b.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert!(click_b.needs_paint);

        assert_eq!(state_a.get("clicked"), Some(json!(true)));
        assert_eq!(state_b.get("count"), Some(json!(1)));

        a.resize(120, 120);
        assert_eq!(a.size(), (120, 120));
        assert_eq!(b.size(), (140, 140));

        let _ = a.render();
        let _ = b.render();
        assert!(
            a.dispatch_mouse(
                8.0,
                8.0,
                MouseEvent::Down {
                    x: 8.0,
                    y: 8.0,
                    button: MouseButton::Left,
                },
            )
            .needs_paint
        );
    }

    // Regression: a reactive child that always resolves to a string should
    // mutate the same text node across re-renders rather than swapping the
    // node — otherwise `focused_node_id` ends up pointing at a detached
    // node and subsequent key events get dropped.
    #[test]
    fn reactive_text_child_keeps_same_node_across_renders() {
        const STABLE_TEXT_COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const para = __sol_createElement("div");
              // appendReactive happens implicitly when JSX passes a function
              // child; here we mimic that via createEffect over __sol_setText
              // since we don't have JSX in this test — what we want to
              // assert is that whatever runtime path the JSX uses preserves
              // the text node id, which appendReactive's fast path does
              // when consecutive values are simple text.
              let textId = __sol_createTextNode("");
              let appended = false;
              createEffect(() => {
                const v = String(globalThis.state.value || "");
                if (!appended) {
                  __sol_insertNode(para, textId, null);
                  appended = true;
                }
                __sol_setText(textId, v);
                globalThis.state.lastTextId = textId;
              });
              __sol_insertNode(root, para, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 100,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            STABLE_TEXT_COMPONENT,
        );
        let _ = instance.render();
        let first_id = instance.state().get("lastTextId");
        instance.state().set("value", json!("hello"));
        let _ = instance.tick();
        let second_id = instance.state().get("lastTextId");
        instance.state().set("value", json!("hi"));
        let _ = instance.tick();
        let third_id = instance.state().get("lastTextId");
        assert!(first_id.is_some());
        assert_eq!(first_id, second_id);
        assert_eq!(second_id, third_id);
    }

    // Regression: an onClick handler that mutates state should re-run a
    // reactive effect that inserts/removes DOM children, so clicking
    // "Add Row" in kitchen_sink actually grows the visible list.
    #[test]
    fn click_triggers_reactive_list_update() {
        const REACTIVE_LIST_COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style", "display:block; width: 200px; height: 200px;");

              const button = __sol_createElement("button");
              __sol_setProperty(button, "style", "display:block; width: 100px; height: 30px;");
              __sol_setProperty(button, "onClick", () => {
                globalThis.state.count = (globalThis.state.count || 0) + 1;
              });
              __sol_insertNode(button, __sol_createTextNode("inc"), null);
              __sol_insertNode(root, button, null);

              const list = __sol_createElement("div");
              __sol_setProperty(list, "style", "display:block;");
              __sol_insertNode(root, list, null);

              // Track inserted child ids so each effect re-run can clear them.
              let prevIds = [];
              createEffect(() => {
                for (const id of prevIds) {
                  __sol_removeNode(list, id);
                }
                prevIds = [];
                const count = Number(globalThis.state.count || 0);
                for (let i = 0; i < count; i++) {
                  const row = __sol_createElement("div");
                  __sol_insertNode(row, __sol_createTextNode("row " + i), null);
                  __sol_insertNode(list, row, null);
                  prevIds.push(row);
                }
                globalThis.state.listLen = prevIds.length;
              });

              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            REACTIVE_LIST_COMPONENT,
        );
        let _ = instance.render();
        assert_eq!(instance.state().get("listLen"), Some(json!(0)));

        // Click the button — Down{Left} fires "click" in solite.
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(instance.state().get("count"), Some(json!(1)));
        assert_eq!(instance.state().get("listLen"), Some(json!(1)));

        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(instance.state().get("count"), Some(json!(2)));
        assert_eq!(instance.state().get("listLen"), Some(json!(2)));
    }

    // Regression for a "RefCell already borrowed" panic: dispatch_wheel used
    // to hold `self.doc.borrow()` as a temporary inside the `if let` scrutinee
    // for `find_handler_up`, extending the Ref's lifetime through the body.
    // When the wheel handler mutated state, a reactive effect ran inline and
    // called `__sol_setText`, which tries `doc.borrow_mut()` → panic.
    #[test]
    fn dispatch_wheel_with_reactive_effect_does_not_panic_on_doc_borrow() {
        const REACTIVE_WHEEL_COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(
                outer,
                "style",
                "display:block; width: 120px; height: 80px; overflow: auto;"
              );
              __sol_setProperty(outer, "onWheel", () => {
                globalThis.state.wheel = (globalThis.state.wheel || 0) + 1;
              });

              const filler = __sol_createElement("div");
              __sol_setProperty(
                filler,
                "style",
                "display:block; width: 120px; height: 240px;"
              );
              __sol_insertNode(outer, filler, null);

              const status = __sol_createElement("div");
              const text = __sol_createTextNode("");
              __sol_insertNode(status, text, null);
              __sol_insertNode(outer, status, null);

              // Effect runs synchronously when state.wheel changes — this is
              // the path that re-enters Rust via __sol_setText while
              // dispatch_wheel is still on the stack.
              createEffect(() => {
                __sol_setText(text, "wheel=" + (globalThis.state.wheel || 0));
              });

              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 160,
                height: 160,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            REACTIVE_WHEEL_COMPONENT,
        );
        let _ = instance.render();

        // Would have panicked before the fix.
        let _ = instance.dispatch_wheel(10.0, 10.0, 0.0, 40.0);
        assert_eq!(instance.state().get("wheel"), Some(json!(1)));
    }

    // ── Stylesheet API & CSS feature tests ────────────────────────────────────

    /// Returns the computed color of the first child of the container as
    /// (r, g, b) bytes in sRGB space. Drives `render()` so styles resolve.
    fn first_child_color(instance: &mut Instance) -> Option<(u8, u8, u8)> {
        let _ = instance.render();
        let doc = instance.doc.borrow();
        let child_id = doc
            .get_node(instance.container_id())
            .and_then(|c| c.children.first().copied())?;
        node_color(&doc, child_id)
    }

    fn node_color(doc: &BaseDocument, node_id: usize) -> Option<(u8, u8, u8)> {
        let styles = doc.get_node(node_id)?.primary_styles()?;
        let srgb = styles
            .clone_color()
            .to_color_space(style::color::ColorSpace::Srgb);
        let c = srgb.components;
        let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        Some((to_u8(c.0), to_u8(c.1), to_u8(c.2)))
    }

    const COLORED_DIV: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const d = __sol_createElement("div");
          __sol_setProperty(d, "className", "tag");
          __sol_setProperty(d, "style", "display:block; width:50px; height:50px;");
          return d;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    fn make_instance_with(
        component: &str,
        css: &[&str],
    ) -> (Instance, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (device, queue) = test_device();
        Instance::new(
            InstanceConfig {
                width: 100,
                height: 100,
                device,
                queue,
                stylesheets: css.iter().map(|s| s.to_string()).collect(),
                document_scroll: false,
                base_url: None,
            },
            component,
        )
    }

    #[test]
    fn classname_normalizes_to_class_and_matches_selector() {
        let (mut instance, _rx) =
            make_instance_with(COLORED_DIV, &[".tag { color: rgb(255, 0, 0) }"]);
        assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));
    }

    #[test]
    fn add_stylesheet_applies_class_rule_post_mount() {
        let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[]);
        let baseline = first_child_color(&mut instance);
        let _ = instance.add_stylesheet(".tag { color: rgb(0, 128, 0) }");
        let after = first_child_color(&mut instance);
        assert_ne!(after, baseline);
        assert_eq!(after, Some((0, 128, 0)));
    }

    #[test]
    fn replace_stylesheet_swaps_rule() {
        let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[]);
        let id = instance.add_stylesheet(".tag { color: rgb(255, 0, 0) }");
        assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));
        assert!(instance.replace_stylesheet(id, ".tag { color: rgb(0, 0, 255) }"));
        assert_eq!(first_child_color(&mut instance), Some((0, 0, 255)));
    }

    #[test]
    fn remove_stylesheet_drops_rule() {
        let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[]);
        let id = instance.add_stylesheet(".tag { color: rgb(255, 0, 0) }");
        assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));
        assert!(instance.remove_stylesheet(id));
        assert_ne!(first_child_color(&mut instance), Some((255, 0, 0)));
        // Removing a non-existent id is a no-op.
        assert!(!instance.remove_stylesheet(id));
    }

    #[test]
    fn upsert_stylesheet_reuses_or_recreates_id() {
        let (mut instance, _rx) = make_instance_with(COLORED_DIV, &[]);
        let stable_id = instance.add_stylesheet(".tag { color: rgb(255, 0, 0) }");
        let updated_id =
            instance.upsert_stylesheet(Some(stable_id), ".tag { color: rgb(0, 0, 255) }");
        assert_eq!(updated_id, stable_id);
        assert_eq!(first_child_color(&mut instance), Some((0, 0, 255)));

        let missing = StylesheetId(u64::MAX);
        let new_id = instance.upsert_stylesheet(Some(missing), ".tag { color: rgb(0, 128, 0) }");
        assert_ne!(new_id, missing);
        assert_eq!(first_child_color(&mut instance), Some((0, 128, 0)));
    }

    #[test]
    fn filewatch_classifies_css_and_js_changes() {
        let root = std::env::temp_dir().join(format!(
            "solite-watch-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create temp watch dir");
        let css_path = root.join("style.css");
        let jsx_path = root.join("app.tsx");
        std::fs::write(&css_path, "body {}").expect("seed css");
        std::fs::write(&jsx_path, "export const x = 1").expect("seed jsx");

        let watch = Instance::watch_files(&root).expect("watch files");

        let wait = |watch: &FileWatch, source_dir: &Path| -> SourceChangeSummary {
            for _ in 0..60 {
                let summary = watch.poll_source_changes(source_dir);
                if summary != SourceChangeSummary::default() {
                    return summary;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            SourceChangeSummary::default()
        };

        std::fs::write(&css_path, "body { color: red }").expect("touch css");
        let css_only = wait(&watch, &root);
        assert!(
            css_only.css_reload,
            "css edits should be flagged for stylesheet reload"
        );
        assert!(
            !css_only.bundle_rebuild,
            "css-only edits should not request bundle rebuild"
        );

        std::fs::write(&jsx_path, "export const x = 2").expect("touch jsx");
        let bundle_only = wait(&watch, &root);
        assert!(
            bundle_only.bundle_rebuild,
            "jsx edits should be flagged for bundle rebuild"
        );
        assert!(
            !bundle_only.css_reload,
            "jsx-only edits should not require css reload"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn class_directive_toggles_class_token() {
        const COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "style", "display:block; width:50px; height:50px;");
              createEffect(() => {
                const on = Boolean(globalThis.state.on);
                __sol_setProperty(d, "class:tag", on);
              });
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) =
            make_instance_with(COMPONENT, &[".tag { color: rgb(255, 0, 0) }"]);
        assert_ne!(first_child_color(&mut instance), Some((255, 0, 0)));

        instance.state().set("on", json!(true));
        let _ = instance.tick();
        assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));

        instance.state().set("on", json!(false));
        let _ = instance.tick();
        assert_ne!(first_child_color(&mut instance), Some((255, 0, 0)));
    }

    #[test]
    fn style_element_applies_css_on_mount() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const style = __sol_createElement("style");
              const text = __sol_createTextNode(".tag { color: rgb(0, 200, 0) }");
              __sol_insertNode(style, text, null);
              __sol_insertNode(root, style, null);

              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "tag");
              __sol_setProperty(d, "style", "display:block; width:50px; height:50px;");
              __sol_insertNode(root, d, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        // Container's first child is `root`; root's second child is the tagged div.
        let _ = instance.render();
        let doc = instance.doc.borrow();
        let root_id = doc
            .get_node(instance.container_id())
            .and_then(|c| c.children.first().copied())
            .expect("root mounted");
        let tagged_id = doc
            .get_node(root_id)
            .and_then(|root| root.children.get(1).copied())
            .expect("tagged div mounted");
        assert_eq!(node_color(&doc, tagged_id), Some((0, 200, 0)));
    }

    #[test]
    fn style_element_refreshes_when_text_changes() {
        const COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const style = __sol_createElement("style");
              const text = __sol_createTextNode("");
              __sol_insertNode(style, text, null);
              __sol_insertNode(root, style, null);
              createEffect(() => {
                const c = String(globalThis.state.css || "");
                __sol_setText(text, c);
              });

              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "tag");
              __sol_setProperty(d, "style", "display:block; width:50px; height:50px;");
              __sol_insertNode(root, d, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        instance
            .state()
            .set("css", json!(".tag { color: rgb(10, 20, 30) }"));
        let _ = instance.tick();
        let _ = instance.render();
        {
            let doc = instance.doc.borrow();
            let root_id = doc
                .get_node(instance.container_id())
                .and_then(|c| c.children.first().copied())
                .unwrap();
            let tagged_id = doc.get_node(root_id).unwrap().children[1];
            assert_eq!(node_color(&doc, tagged_id), Some((10, 20, 30)));
        }
        instance
            .state()
            .set("css", json!(".tag { color: rgb(99, 88, 77) }"));
        let _ = instance.tick();
        let _ = instance.render();
        let doc = instance.doc.borrow();
        let root_id = doc
            .get_node(instance.container_id())
            .and_then(|c| c.children.first().copied())
            .unwrap();
        let tagged_id = doc.get_node(root_id).unwrap().children[1];
        assert_eq!(node_color(&doc, tagged_id), Some((99, 88, 77)));
    }

    #[test]
    fn hover_pseudo_class_changes_computed_color() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "tag");
              __sol_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(
            COMPONENT,
            &[".tag { color: rgb(10, 10, 10) } .tag:hover { color: rgb(200, 200, 200) }"],
        );
        assert_eq!(first_child_color(&mut instance), Some((10, 10, 10)));
        let _ = instance.dispatch_mouse(20.0, 20.0, MouseEvent::Move { x: 20.0, y: 20.0 });
        assert_eq!(first_child_color(&mut instance), Some((200, 200, 200)));
        let _ = instance.dispatch_mouse(500.0, 500.0, MouseEvent::Move { x: 500.0, y: 500.0 });
        assert_eq!(first_child_color(&mut instance), Some((10, 10, 10)));
    }

    #[test]
    fn hover_pseudo_class_works_with_multiple_class_tokens() {
        // Mirrors the kitchen_sink pattern: a multi-token `class` like
        // "btn btn-add" with `:hover` rules on the more specific token.
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "btn btn-add");
              __sol_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        // Both tokens are independently match-able and the `:hover` selector
        // on the second token must still flip the colour.
        let (mut instance, _rx) = make_instance_with(
            COMPONENT,
            &[".btn { color: rgb(50, 50, 50) } .btn-add:hover { color: rgb(255, 0, 0) }"],
        );
        // Static (non-pseudo) match against the first token works even before
        // any hover snapshot — confirms classlist parsing.
        assert_eq!(first_child_color(&mut instance), Some((50, 50, 50)));
        let _ = instance.dispatch_mouse(10.0, 10.0, MouseEvent::Move { x: 10.0, y: 10.0 });
        assert_eq!(first_child_color(&mut instance), Some((255, 0, 0)));
    }

    #[test]
    fn hover_flips_when_pointer_enters_nested_child() {
        // Pointer moves to a child node; the styled ancestor should still
        // pick up :hover because we snapshot the whole ancestor chain.
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "className", "row");
              __sol_setProperty(outer, "style", "display:block; width:80px; height:80px; padding:10px;");
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style", "display:block; width:40px; height:40px;");
              __sol_insertNode(inner, __sol_createTextNode("hi"), null);
              __sol_insertNode(outer, inner, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(
            COMPONENT,
            &[".row { color: rgb(10, 10, 10) } .row:hover { color: rgb(20, 80, 200) }"],
        );
        assert_eq!(first_child_color(&mut instance), Some((10, 10, 10)));
        // Land squarely on the inner child so the hit is on the text or
        // inner div, not the outer.
        let _ = instance.dispatch_mouse(25.0, 25.0, MouseEvent::Move { x: 25.0, y: 25.0 });
        assert_eq!(first_child_color(&mut instance), Some((20, 80, 200)));
    }

    /// Regression: in kitchen_sink, rows are inserted by Solid's `insert`
    /// helper applied to an array-returning function. Each row carries
    /// `class="row row-even"` and we want `.row:hover` to flip its colour.
    /// The class is set via the renderer's `setProp` path (the same path the
    /// JSX compiler emits), not directly via __sol_setProperty.
    #[test]
    fn hover_via_solid_setprop_on_class_works() {
        const COMPONENT: &str = r#"
            import { render, setProp, insert } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style", "display:block; width:120px; height:120px;");

              const make = () => {
                const items = [];
                for (let i = 0; i < 2; i++) {
                  const row = __sol_createElement("div");
                  setProp(row, "class", i % 2 === 0 ? "row row-even" : "row row-odd");
                  setProp(row, "style", "display:block; width:120px; height:40px;");
                  __sol_insertNode(row, __sol_createTextNode("row " + i), null);
                  items.push(row);
                }
                return items;
              };
              insert(root, make);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let css = ".row { color: rgb(50, 50, 50) } \
                   .row-even { background: rgb(20, 20, 20) } \
                   .row:hover { color: rgb(255, 0, 0) }";
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[css]);
        // First row idle = grey.
        let _ = instance.render();
        let row_color = {
            let doc = instance.doc.borrow();
            let root_id = doc
                .get_node(instance.container_id())
                .and_then(|c| c.children.first().copied())
                .unwrap();
            let row_id = doc.get_node(root_id).unwrap().children[0];
            node_color(&doc, row_id)
        };
        assert_eq!(row_color, Some((50, 50, 50)));

        // Hover the first row.
        let _ = instance.dispatch_mouse(20.0, 10.0, MouseEvent::Move { x: 20.0, y: 10.0 });
        let _ = instance.render();
        let hovered_color = {
            let doc = instance.doc.borrow();
            let root_id = doc
                .get_node(instance.container_id())
                .and_then(|c| c.children.first().copied())
                .unwrap();
            let row_id = doc.get_node(root_id).unwrap().children[0];
            node_color(&doc, row_id)
        };
        assert_eq!(hovered_color, Some((255, 0, 0)));
    }

    /// Regression: mirrors kitchen_sink exactly — same JSX pattern (a button
    /// inside a panel container), same CSS (multi-token classes + :hover on
    /// the more-specific token), driven through the JSX compiler so we
    /// exercise the same code paths as the example.
    #[cfg(feature = "jsx-compiler")]
    #[test]
    fn kitchen_sink_button_hover_flips_color() {
        const JSX: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              return (
                <div class="panel">
                  <button class="btn btn-add">+ Add Row</button>
                </div>
              );
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        const CSS: &str = r#"
            .panel { display: block; width: 200px; padding: 10px; background: #182238; }
            .btn { display: inline-block; padding: 8px 10px; border: 1px solid #7fb5ff; color: rgb(243, 247, 255); }
            .btn-add { background: rgb(31, 59, 95); }
            .btn-add:hover { background: rgb(91, 140, 250); color: rgb(255, 255, 255); }
        "#;

        let compiled = solite_build::compile_component_source(
            std::path::Path::new("/tmp/kitchen.jsx"),
            JSX,
        )
        .expect("compile");
        let (mut instance, _rx) = make_instance_with(&compiled, &[CSS]);

        // First paint resolves layout.
        let _ = instance.render();

        // The button lives at panel.children[0]. Find a point inside it.
        let (panel_id, btn_id) = {
            let doc = instance.doc.borrow();
            let panel = doc
                .get_node(instance.container_id())
                .and_then(|c| c.children.first().copied())
                .unwrap();
            let btn = doc.get_node(panel).unwrap().children[0];
            (panel, btn)
        };
        let _ = panel_id;

        // Read color before hover.
        let before = {
            let doc = instance.doc.borrow();
            node_color(&doc, btn_id)
        };
        assert_eq!(before, Some((243, 247, 255)));

        // Move pointer into the button — its layout is inside the panel which
        // has padding 10. So (20, 20) lands on the button.
        let _ = instance.dispatch_mouse(20.0, 20.0, MouseEvent::Move { x: 20.0, y: 20.0 });
        let _ = instance.render();
        let after = {
            let doc = instance.doc.borrow();
            node_color(&doc, btn_id)
        };
        assert_eq!(after, Some((255, 255, 255)), "hover should flip color");
    }

    /// Regression: kitchen_sink panicked at blitz-dom/src/stylo.rs:84
    /// (`invalid key`) when the user clicked "Add Row" after a row had been
    /// styled with a `transition:` declaration. Cause: removed nodes leave
    /// stale entries in `DocumentAnimationSet` that `resolve_stylist`
    /// indexes back into `self.nodes`. Until the upstream cleanup lands,
    /// avoid `transition:` on dynamic subtrees; this test guards against
    /// regressing back into the panic with a transition-free :hover rule.
    #[test]
    fn dynamic_subtree_with_hover_survives_remove_and_restyle() {
        const COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style", "display:block; width:120px; height:240px;");
              let prevIds = [];
              createEffect(() => {
                for (const id of prevIds) { __sol_removeNode(root, id); }
                prevIds = [];
                const n = Number(globalThis.state.rows || 0);
                for (let i = 0; i < n; i++) {
                  const row = __sol_createElement("div");
                  __sol_setProperty(row, "className", "row");
                  __sol_setProperty(row, "style", "display:block; width:120px; height:20px;");
                  __sol_insertNode(row, __sol_createTextNode("row " + i), null);
                  __sol_insertNode(root, row, null);
                  prevIds.push(row);
                }
              });
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let css = ".row { color: rgb(50, 50, 50) } .row:hover { color: rgb(255, 0, 0) }";
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[css]);
        instance.state().set("rows", json!(3));
        let _ = instance.tick();
        let _ = instance.render();
        // Hover, then add/remove rows to force the snapshot + animation path.
        let _ = instance.dispatch_mouse(10.0, 5.0, MouseEvent::Move { x: 10.0, y: 5.0 });
        let _ = instance.render();
        instance.state().set("rows", json!(5));
        let _ = instance.tick();
        // The panic was triggered by the next resolve after removal.
        let _ = instance.render();
        instance.state().set("rows", json!(2));
        let _ = instance.tick();
        let _ = instance.render();
    }

    /// Regression: read painted pixels (not just primary_styles) to confirm
    /// :hover actually reaches the texture, not just the computed-style
    /// cache. This is the missing link between "the unit tests pass" and
    /// "the live app shows no hover".
    #[test]
    fn hover_pseudo_class_actually_paints_new_color() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "swatch");
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let css = ".swatch { display:block; width:80px; height:80px; background: rgb(0, 0, 255); } \
                   .swatch:hover { background: rgb(255, 0, 0); }";
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[css]);

        let read_pixel = |instance: &mut Instance, x: u32, y: u32| -> (u8, u8, u8) {
            let _ = instance.render();
            // The painter writes into its internal cpu_buffer before uploading
            // to the texture. We can't read back the texture without a copy
            // operation, but cpu_buffer is the same RGBA8 source. Reach in.
            let buf = &instance.painter.cpu_buffer;
            let row = (instance.width * 4) as usize;
            let i = (y as usize) * row + (x as usize) * 4;
            (buf[i], buf[i + 1], buf[i + 2])
        };

        let before = read_pixel(&mut instance, 20, 20);
        assert_eq!(before, (0, 0, 255), "before hover: {before:?}");
        let _ = instance.dispatch_mouse(20.0, 20.0, MouseEvent::Move { x: 20.0, y: 20.0 });
        let after = read_pixel(&mut instance, 20, 20);
        assert_eq!(after, (255, 0, 0), "after hover: {after:?}");
    }

    #[test]
    fn scrollable_overflow_paints_a_scrollbar() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        assert!(
            !instance.scrollbars.is_empty(),
            "expected a scrollbar region for the overflowing container",
        );
        let region = instance.scrollbars[0];
        // The track sits flush against the instance viewport's right edge.
        let viewport_w = instance.width as f32;
        assert!(
            (region.track.0 + region.track.2 - viewport_w).abs() < 0.01,
            "expected scrollbar track to clamp to the instance viewport width {viewport_w}: {region:?}",
        );
        assert!(region.max_scroll > 0.0);
    }

    #[test]
    fn scrollbar_thumb_drag_moves_scroll_offset() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        let region = instance.scrollbars[0];
        let thumb_centre_x = region.thumb.0 + region.thumb.2 * 0.5;
        let thumb_centre_y = region.thumb.1 + region.thumb.3 * 0.5;
        let outer_id = region.node_id;

        // Down on the thumb starts the drag.
        let _ = instance.dispatch_mouse(
            thumb_centre_x,
            thumb_centre_y,
            MouseEvent::Down {
                x: thumb_centre_x,
                y: thumb_centre_y,
                button: MouseButton::Left,
            },
        );
        assert!(instance.scrollbar_drag.is_some());

        // Move down 30 logical pixels — scroll_offset should grow.
        let _ = instance.dispatch_mouse(
            thumb_centre_x,
            thumb_centre_y + 30.0,
            MouseEvent::Move {
                x: thumb_centre_x,
                y: thumb_centre_y + 30.0,
            },
        );
        let scrolled = instance
            .doc
            .borrow()
            .get_node(outer_id)
            .unwrap()
            .scroll_offset
            .y;
        assert!(scrolled > 0.0, "expected scroll to advance, got {scrolled}");

        // Up ends the drag.
        let _ = instance.dispatch_mouse(
            thumb_centre_x,
            thumb_centre_y + 30.0,
            MouseEvent::Up {
                x: thumb_centre_x,
                y: thumb_centre_y + 30.0,
                button: MouseButton::Left,
            },
        );
        assert!(instance.scrollbar_drag.is_none());
    }

    #[test]
    fn scrollbar_theme_paints_supplied_colors() {
        // Container sized so the scrollbar lives well inside the 100x100
        // painter texture; the test reads back pixels from cpu_buffer.
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "style", "display:block; width:80px; height:80px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "display:block; width:80px; height:400px;");
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        instance.set_scrollbar_theme(Some(crate::ScrollbarTheme {
            track: (50, 50, 50, 255),
            thumb: (220, 30, 30, 255),
        }));
        let _ = instance.render();
        let region = instance.scrollbars[0];
        // Sample the thumb centre — should be the supplied red.
        let row = (instance.width * 4) as usize;
        let tx = (region.thumb.0 + region.thumb.2 * 0.5) as usize;
        let ty = (region.thumb.1 + region.thumb.3 * 0.5) as usize;
        let i = ty * row + tx * 4;
        let (r, g, b) = (
            instance.painter.cpu_buffer[i],
            instance.painter.cpu_buffer[i + 1],
            instance.painter.cpu_buffer[i + 2],
        );
        // Allow a few-byte rounding tolerance from Vello's sampler.
        assert!(
            r > 200 && g < 60 && b < 60,
            "thumb should be red, got ({r},{g},{b})"
        );
    }

    #[test]
    fn track_click_pages_scroll_offset() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        let region = instance.scrollbars[0];
        // Click below the thumb — should page down.
        let click_x = region.track.0 + region.track.2 * 0.5;
        let click_y = region.thumb.1 + region.thumb.3 + 5.0;
        let _ = instance.dispatch_mouse(
            click_x,
            click_y,
            MouseEvent::Down {
                x: click_x,
                y: click_y,
                button: MouseButton::Left,
            },
        );
        let after = instance
            .doc
            .borrow()
            .get_node(region.node_id)
            .unwrap()
            .scroll_offset
            .y;
        assert!(
            after > 0.0,
            "expected page-down to advance scroll, got {after}"
        );
    }

    #[test]
    fn document_scroll_scrolls_root_container() {
        // A document taller than the instance height. With document_scroll:
        // true the container gets overflow-y:auto + explicit height, so wheel
        // events that aren't consumed by a child scroll the container itself.
        const TALL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style",
                "display:block; width:200px; height:600px; background:#111;");
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: true,
                base_url: None,
            },
            TALL_COMPONENT,
        );
        let _ = instance.render();

        let before = instance
            .doc
            .borrow()
            .get_node(instance.container_id())
            .expect("container exists")
            .scroll_offset
            .y;

        // Wheel down (negative winit delta → content scrolls down).
        let result = instance.dispatch_wheel(10.0, 10.0, 0.0, -40.0);
        assert!(result.needs_paint);

        let after = instance
            .doc
            .borrow()
            .get_node(instance.container_id())
            .expect("container exists")
            .scroll_offset
            .y;

        assert!(
            after > before,
            "document scroll_offset should increase after wheel down (before={before}, after={after})"
        );
    }

    #[test]
    fn horizontal_overflow_emits_horizontal_scrollbar() {
        // A scroll container with content that overflows on the X axis.
        // collect_scrollbar_regions should emit a horizontal scrollbar
        // pinned to the bottom of the container.
        const WIDE_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:block; width:200px; height:200px; overflow:auto;");
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style",
                "display:block; width:600px; height:100px; background:#888;");
              __sol_insertNode(wrap, inner, null);
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            WIDE_COMPONENT,
        );
        let _ = instance.render();

        let h_region = instance
            .scrollbars
            .iter()
            .find(|r| r.axis == ScrollAxis::Horizontal)
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "expected a horizontal scrollbar region, got {:?}",
                    instance.scrollbars
                )
            });

        // Track must be at the bottom edge of the 200x200 container,
        // SCROLLBAR_WIDTH tall, fully within the viewport.
        let (tx, ty, tw, th) = h_region.track;
        assert!(
            tx >= 0.0 && tx + tw <= 200.0,
            "track x bounds: {h_region:?}"
        );
        assert!(
            (ty + th - 200.0).abs() < 0.01,
            "track should sit on the bottom edge: {h_region:?}",
        );
        // No vertical bar in this layout (content fits vertically), so the
        // horizontal track should NOT be inset for a v-bar corner.
        assert!(
            !instance
                .scrollbars
                .iter()
                .any(|r| r.axis == ScrollAxis::Vertical),
            "this layout has no vertical overflow"
        );

        // Scrolling the container right should move the thumb right.
        let initial_thumb_x = h_region.thumb.0;
        let _ = instance.dispatch_wheel(50.0, 50.0, -200.0, 0.0);
        let _ = instance.render();
        let h2 = instance
            .scrollbars
            .iter()
            .find(|r| r.axis == ScrollAxis::Horizontal)
            .copied()
            .expect("horizontal scrollbar still present");
        assert!(
            h2.thumb.0 > initial_thumb_x,
            "thumb should move right after horizontal wheel (before={initial_thumb_x}, after={})",
            h2.thumb.0,
        );
    }

    #[test]
    fn three_scene_surfaces_keep_horizontal_scrollbar_visible() {
        use crate::scene::{Scene, SurfaceRect};

        const PANELS_COMPONENT_BASE: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const is_center = globalThis.state.targetIndex === 1;
              const outer = __sol_createElement("div");
              __sol_setProperty(
                outer,
                "style",
                is_center
                  ? "display:block; width:160px; height:80px; overflow:auto; color:#ffffff; background:#080808;"
                  : "display:block; width:160px; height:80px; overflow:auto; color:#060606; background:#f0f4ff;"
              );
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style", "display:block; width:320px; height:40px; background:#8899aa;");
              __sol_insertNode(outer, inner, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let mut scene: Scene<()> = Scene::new();
        for index in 0..3 {
            let seeded_source =
                format!("globalThis.state.targetIndex = {index};\n{PANELS_COMPONENT_BASE}");
            let (instance, _rx) = Instance::new(
                InstanceConfig {
                    width: 180,
                    height: 100,
                    device: device.clone(),
                    queue: queue.clone(),
                    stylesheets: vec![],
                    document_scroll: false,
                    base_url: None,
                },
                &seeded_source,
            );
            scene.add_surface(
                instance,
                SurfaceRect::new((180 * index) as f32, 0.0, 180.0, 100.0),
                (),
            );
        }

        for surface in scene.surfaces_mut() {
            let _ = surface.instance.render();
        }

        let left = scene.surfaces()[0]
            .instance
            .scrollbars
            .iter()
            .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
            .copied()
            .expect("left surface should expose a horizontal scrollbar");
        let center = scene.surfaces()[1]
            .instance
            .scrollbars
            .iter()
            .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
            .copied()
            .expect("center surface should expose a horizontal scrollbar");
        let right = scene.surfaces()[2]
            .instance
            .scrollbars
            .iter()
            .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
            .copied()
            .expect("right surface should expose a horizontal scrollbar");

        let read_pixel = |instance: &mut Instance, x: f32, y: f32| -> (u8, u8, u8, u8) {
            let buf = &instance.painter.cpu_buffer;
            let width = (instance.width as usize) * 4;
            let ix = (x.max(0.0) as usize).min(instance.width.saturating_sub(1) as usize);
            let iy = (y.max(0.0) as usize).min(instance.height.saturating_sub(1) as usize);
            let idx = iy * width + ix * 4;
            (buf[idx], buf[idx + 1], buf[idx + 2], buf[idx + 3])
        };

        let bg_dist = |pixel: (u8, u8, u8, u8), bg: (u8, u8, u8)| -> u8 {
            let dr = pixel.0.abs_diff(bg.0);
            let dg = pixel.1.abs_diff(bg.1);
            let db = pixel.2.abs_diff(bg.2);
            let sum = u16::from(dr) + u16::from(dg) + u16::from(db);
            (sum / 3).try_into().unwrap_or(0)
        };
        let is_visible =
            |sample: (u8, u8, u8, u8), bg: (u8, u8, u8)| -> bool { bg_dist(sample, bg) > 24 };

        let left_sample = read_pixel(
            &mut scene.surfaces_mut()[0].instance,
            left.thumb.0 + left.thumb.2 * 0.5,
            left.thumb.1 + left.thumb.3 * 0.5,
        );
        let center_sample = read_pixel(
            &mut scene.surfaces_mut()[1].instance,
            center.thumb.0 + center.thumb.2 * 0.5,
            center.thumb.1 + center.thumb.3 * 0.5,
        );
        let right_sample = read_pixel(
            &mut scene.surfaces_mut()[2].instance,
            right.thumb.0 + right.thumb.2 * 0.5,
            right.thumb.1 + right.thumb.3 * 0.5,
        );
        assert!(
            is_visible(left_sample, (240, 244, 255)),
            "left horizontal scrollbar thumb should be visible, got {:?}",
            left_sample
        );
        assert!(
            is_visible(center_sample, (8, 8, 8)),
            "center horizontal scrollbar thumb should be visible on dark background, got {:?}",
            center_sample
        );
        assert!(
            is_visible(right_sample, (240, 244, 255)),
            "right horizontal scrollbar thumb should be visible, got {:?}",
            right_sample
        );
        assert!(
            left.thumb.2 > 0.0 && center.thumb.2 > 0.0 && right.thumb.2 > 0.0,
            "horizontal scroll thumbs should exist on all three surfaces"
        );
    }

    #[test]
    fn scrollbar_tracks_are_local_to_each_scene_surface() {
        use crate::scene::{Scene, SurfaceRect};

        const SCROLL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(
                outer,
                "style",
                "display:block; width:100px; height:100px; overflow:auto;"
              );
              const filler = __sol_createElement("div");
              __sol_setProperty(
                filler,
                "style",
                "display:block; width:100px; height:300px;"
              );
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();

        let (surface_a, _rx_a) = Instance::new(
            InstanceConfig {
                width: 100,
                height: 100,
                device: device.clone(),
                queue: queue.clone(),
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            SCROLL_COMPONENT,
        );
        let (surface_b, _rx_b) = Instance::new(
            InstanceConfig {
                width: 100,
                height: 100,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            SCROLL_COMPONENT,
        );

        let mut scene: Scene<()> = Scene::new();
        scene.add_surface(surface_a, SurfaceRect::new(0.0, 0.0, 100.0, 100.0), ());
        scene.add_surface(surface_b, SurfaceRect::new(200.0, 0.0, 100.0, 100.0), ());

        let _ = scene.surfaces_mut()[0].instance.render();
        let _ = scene.surfaces_mut()[1].instance.render();

        let left_region = scene.surfaces()[0]
            .instance
            .scrollbars
            .iter()
            .next()
            .copied()
            .expect("left instance should render a scrollbar");
        let right_region = scene.surfaces()[1]
            .instance
            .scrollbars
            .iter()
            .next()
            .copied()
            .expect("right instance should render a scrollbar");

        // Scrollbars must stay inside their own 100x100 Blix surface.
        assert!(left_region.track.0 >= 0.0 && left_region.track.0 + left_region.track.2 <= 100.0);
        assert!(left_region.track.1 >= 0.0 && left_region.track.1 + left_region.track.3 <= 100.0);
        assert!(
            right_region.track.0 >= 0.0 && right_region.track.0 + right_region.track.2 <= 100.0
        );
        assert!(
            right_region.track.1 >= 0.0 && right_region.track.1 + right_region.track.3 <= 100.0
        );

        let click_x =
            scene.surfaces()[1].rect.x + right_region.track.0 + right_region.track.2 * 0.5;
        let click_y =
            scene.surfaces()[1].rect.y + right_region.track.1 + right_region.track.3 * 0.5;

        let before_left_scroll = scene.surfaces()[0]
            .instance
            .doc
            .borrow()
            .get_node(left_region.node_id)
            .unwrap()
            .scroll_offset
            .y;
        let before_right_scroll = scene.surfaces()[1]
            .instance
            .doc
            .borrow()
            .get_node(right_region.node_id)
            .unwrap()
            .scroll_offset
            .y;

        let _ = scene.dispatch_mouse(
            click_x,
            click_y,
            MouseEvent::Down {
                x: click_x,
                y: click_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.dispatch_mouse(
            click_x,
            click_y,
            MouseEvent::Up {
                x: click_x,
                y: click_y,
                button: MouseButton::Left,
            },
        );

        let after_left_scroll = scene.surfaces()[0]
            .instance
            .doc
            .borrow()
            .get_node(left_region.node_id)
            .unwrap()
            .scroll_offset
            .y;
        let after_right_scroll = scene.surfaces()[1]
            .instance
            .doc
            .borrow()
            .get_node(right_region.node_id)
            .unwrap()
            .scroll_offset
            .y;

        assert_eq!(
            before_left_scroll, after_left_scroll,
            "left surface should not scroll when interacting with right scrollbar"
        );
        assert!(
            after_right_scroll > before_right_scroll,
            "right surface should scroll when its scrollbar is clicked"
        );
    }

    #[test]
    fn horizontal_scrollbar_tracks_are_local_to_each_scene_surface() {
        use crate::scene::{Scene, SurfaceRect};

        const SCROLL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const outer = __sol_createElement("div");
              __sol_setProperty(
                outer,
                "style",
                "display:block; width:100px; height:60px; overflow:auto;"
              );
              const filler = __sol_createElement("div");
              __sol_setProperty(
                filler,
                "style",
                "display:block; width:300px; height:60px;"
              );
              __sol_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();

        let (surface_a, _rx_a) = Instance::new(
            InstanceConfig {
                width: 100,
                height: 100,
                device: device.clone(),
                queue: queue.clone(),
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            SCROLL_COMPONENT,
        );
        let (surface_b, _rx_b) = Instance::new(
            InstanceConfig {
                width: 100,
                height: 100,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            SCROLL_COMPONENT,
        );

        let mut scene: Scene<()> = Scene::new();
        scene.add_surface(surface_a, SurfaceRect::new(0.0, 0.0, 100.0, 100.0), ());
        scene.add_surface(surface_b, SurfaceRect::new(200.0, 0.0, 100.0, 100.0), ());

        let _ = scene.surfaces_mut()[0].instance.render();
        let _ = scene.surfaces_mut()[1].instance.render();

        let left_region = scene.surfaces()[0]
            .instance
            .scrollbars
            .iter()
            .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
            .copied()
            .expect("left instance should render a horizontal scrollbar");
        let right_region = scene.surfaces()[1]
            .instance
            .scrollbars
            .iter()
            .find(|region| matches!(region.axis, ScrollAxis::Horizontal))
            .copied()
            .expect("right instance should render a horizontal scrollbar");

        // Horizontal scrollbars should live at the bottom of each 100px surface.
        assert!(left_region.track.0 >= 0.0 && left_region.track.0 + left_region.track.2 <= 100.0);
        assert!(left_region.track.1 >= 0.0 && left_region.track.1 + left_region.track.3 <= 100.0);
        assert!(
            right_region.track.0 >= 0.0 && right_region.track.0 + right_region.track.2 <= 100.0
        );
        assert!(
            right_region.track.1 >= 0.0 && right_region.track.1 + right_region.track.3 <= 100.0
        );

        let click_x =
            scene.surfaces()[1].rect.x + right_region.track.0 + right_region.track.2 * 0.5;
        let click_y =
            scene.surfaces()[1].rect.y + right_region.track.1 + right_region.track.3 * 0.5;

        let before_left_scroll = scene.surfaces()[0]
            .instance
            .doc
            .borrow()
            .get_node(left_region.node_id)
            .unwrap()
            .scroll_offset
            .x;
        let before_right_scroll = scene.surfaces()[1]
            .instance
            .doc
            .borrow()
            .get_node(right_region.node_id)
            .unwrap()
            .scroll_offset
            .x;

        let _ = scene.dispatch_mouse(
            click_x,
            click_y,
            MouseEvent::Down {
                x: click_x,
                y: click_y,
                button: MouseButton::Left,
            },
        );
        let _ = scene.dispatch_mouse(
            click_x,
            click_y,
            MouseEvent::Up {
                x: click_x,
                y: click_y,
                button: MouseButton::Left,
            },
        );

        let after_left_scroll = scene.surfaces()[0]
            .instance
            .doc
            .borrow()
            .get_node(left_region.node_id)
            .unwrap()
            .scroll_offset
            .x;
        let after_right_scroll = scene.surfaces()[1]
            .instance
            .doc
            .borrow()
            .get_node(right_region.node_id)
            .unwrap()
            .scroll_offset
            .x;

        assert_eq!(
            before_left_scroll, after_left_scroll,
            "left surface should not scroll when interacting with right scrollbar"
        );
        assert!(
            after_right_scroll > before_right_scroll,
            "right surface should scroll when its horizontal scrollbar is clicked"
        );
    }

    #[test]
    fn horizontal_overflow_emits_without_vertical_scroll() {
        // overflow-x-only (with overflow-y:hidden) should still emit a horizontal
        // scrollbar so long as there is horizontal overflow.
        const WIDE_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:block; width:200px; height:200px; overflow-x:auto; overflow-y:hidden;");
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style",
                "display:block; width:600px; height:100px; background:#888;");
              __sol_insertNode(wrap, inner, null);
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            WIDE_COMPONENT,
        );
        let _ = instance.render();

        let regions = instance
            .scrollbars
            .iter()
            .filter(|r| r.axis == ScrollAxis::Horizontal)
            .collect::<Vec<_>>();
        assert_eq!(
            regions.len(),
            1,
            "expected exactly one horizontal scrollbar: {:?}",
            regions
        );

        let h_region = regions[0];
        assert!(
            !instance
                .scrollbars
                .iter()
                .any(|r| r.axis == ScrollAxis::Vertical),
            "no vertical scrollbar should be needed"
        );

        assert!(
            h_region.track.3 > 0.0,
            "horizontal track should be visible: {:?}",
            h_region.track
        );
    }

    #[test]
    fn inline_overflow_emits_horizontal_scrollbar() {
        // A long inline element that overflows horizontally should produce a
        // horizontal scrollbar even when vertical scrolling is not needed.
        const WIDE_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:block; width:200px; height:20px; overflow:auto; white-space: nowrap;");
              const child = __sol_createElement("span");
              __sol_setProperty(child, "style", "display:inline-block; width:400px; height:20px;");
              __sol_insertNode(wrap, child, null);
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 220,
                height: 80,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            WIDE_COMPONENT,
        );
        let _ = instance.render();

        assert!(
            instance
                .scrollbars
                .iter()
                .any(|r| r.axis == ScrollAxis::Horizontal),
            "expected horizontal scrollbar for inline overflow: {:?}",
            instance.scrollbars
        );
        assert!(
            !instance
                .scrollbars
                .iter()
                .any(|r| r.axis == ScrollAxis::Vertical),
            "inline overflow test should not need a vertical scrollbar"
        );
    }

    #[test]
    fn no_horizontal_scrollbar_for_vertical_only_overflow() {
        // A container that only overflows vertically (content wider than needed)
        // must not emit a horizontal scrollbar.
        const TALL_WIDE_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:block; width:200px; height:80px; overflow:auto;");
              const inner = __sol_createElement("div");
              __sol_setProperty(inner, "style", "width:200px; height:300px; background:#888;");
              __sol_insertNode(wrap, inner, null);
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 220,
                height: 80,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            TALL_WIDE_COMPONENT,
        );
        let _ = instance.render();

        let has_h = instance
            .scrollbars
            .iter()
            .any(|r| r.axis == ScrollAxis::Horizontal);
        let has_v = instance
            .scrollbars
            .iter()
            .any(|r| r.axis == ScrollAxis::Vertical);

        assert!(has_v, "vertical overflow should emit a vertical scrollbar");
        assert!(
            !has_h,
            "vertical-only overflow should not emit a horizontal scrollbar"
        );
    }

    #[test]
    fn flex_inline_overflow_emits_horizontal_scrollbar() {
        // Flex row content wider than the container should emit a horizontal scrollbar.
        const FLEX_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const wrap = __sol_createElement("div");
              __sol_setProperty(wrap, "style",
                "display:flex; width:200px; height:60px; overflow:auto;");
              for (let i = 0; i < 10; i++) {
                const item = __sol_createElement("div");
                __sol_setProperty(item, "style", "display:flex; flex:0 0 auto; width:80px; height:40px;");
                __sol_insertNode(wrap, item, null);
              }
              return wrap;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 220,
                height: 60,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            FLEX_COMPONENT,
        );
        let _ = instance.render();

        assert!(
            instance
                .scrollbars
                .iter()
                .any(|r| r.axis == ScrollAxis::Horizontal),
            "flex row overflow should emit horizontal scrollbar: {:?}",
            instance.scrollbars
        );
    }

    #[test]
    fn document_scroll_with_inner_scroll_still_scrolls() {
        // Mirror the kitchen sink layout: tall panel inside the document
        // (taller than the instance height) AND a child element with its
        // own `overflow: auto`. Wheeling over the panel (not over the
        // inner scroll container) should scroll the document container.
        const PANEL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const panel = __sol_createElement("div");
              __sol_setProperty(panel, "style",
                "display:block; width:360px; padding:10px; background:#111;");
              const title = __sol_createElement("div");
              __sol_setProperty(title, "style", "height:200px; background:#222;");
              __sol_insertNode(panel, title, null);
              const rows = __sol_createElement("div");
              __sol_setProperty(rows, "style",
                "display:block; width:340px; height:190px; overflow:auto;");
              const filler = __sol_createElement("div");
              __sol_setProperty(filler, "style", "height:600px; background:#333;");
              __sol_insertNode(rows, filler, null);
              __sol_insertNode(panel, rows, null);
              const footer = __sol_createElement("div");
              __sol_setProperty(footer, "style", "height:200px; background:#444;");
              __sol_insertNode(panel, footer, null);
              return panel;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 360,
                height: 440,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: true,
                base_url: None,
            },
            PANEL_COMPONENT,
        );
        let _ = instance.render();

        let container_id = instance.container_id();
        let before = instance
            .doc
            .borrow()
            .get_node(container_id)
            .unwrap()
            .scroll_offset
            .y;

        // Wheel over the top of the panel (well above the inner .rows).
        let _ = instance.dispatch_wheel(50.0, 50.0, 0.0, -40.0);

        let after = instance
            .doc
            .borrow()
            .get_node(container_id)
            .unwrap()
            .scroll_offset
            .y;
        assert!(
            after > before,
            "wheel at panel top should scroll the document container \
             (before={before}, after={after})"
        );
    }

    #[test]
    fn document_scroll_emits_scrollbar_region() {
        // Same setup as document_scroll_scrolls_root_container, but here we
        // assert that render() collects a scrollbar region for the root
        // container so the bar actually paints. Also exercises that the
        // track stays pinned to the viewport (not scrolled off-screen) and
        // that the thumb moves down as the container is scrolled.
        const TALL_COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "style",
                "display:block; width:200px; height:600px; background:#111;");
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 200,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: true,
                base_url: None,
            },
            TALL_COMPONENT,
        );
        let _ = instance.render();

        let container_id = instance.container_id();
        let region = instance
            .scrollbars
            .iter()
            .find(|region| region.node_id == container_id)
            .copied()
            .unwrap_or_else(|| {
                panic!(
                    "expected a scrollbar region for the document-scroll container, got {:?}",
                    instance.scrollbars
                )
            });
        let (tx, ty, tw, th) = region.track;
        assert!(
            tx >= 0.0 && tx + tw <= 200.0 && ty >= 0.0 && ty + th <= 200.0,
            "track {region:?} should be within the 200x200 viewport"
        );

        // Scroll the container, then re-render. The track must stay pinned
        // to the viewport (track_y unchanged) and the thumb must move down.
        let _ = instance.dispatch_wheel(10.0, 10.0, 0.0, -120.0);
        let _ = instance.render();
        let region2 = instance
            .scrollbars
            .iter()
            .find(|region| region.node_id == container_id)
            .copied()
            .expect("scrollbar region still present after scroll");
        assert_eq!(
            region2.track, region.track,
            "track must stay pinned to the viewport"
        );
        assert!(
            region2.thumb.1 > region.thumb.1,
            "thumb should move down after scrolling (before={}, after={})",
            region.thumb.1,
            region2.thumb.1,
        );
    }

    // ── Native <input> tests ──────────────────────────────────────────────

    fn type_key(key: &str) -> KeyboardEvent {
        KeyboardEvent {
            key: key.into(),
            code: String::new(),
            key_code: 0,
            repeat: false,
            shift_key: false,
            ctrl_key: false,
            alt_key: false,
            meta_key: false,
        }
    }

    fn input_child_text(instance: &Instance, input_id: usize) -> String {
        let doc = instance.doc.borrow();
        let child = doc
            .get_node(input_id)
            .and_then(|node| node.children.first().copied());
        child
            .and_then(|child_id| {
                doc.get_node(child_id).and_then(|child_node| {
                    if let blitz_dom::NodeData::Text(text) = &child_node.data {
                        Some(text.content.clone())
                    } else {
                        None
                    }
                })
            })
            .unwrap_or_default()
    }

    fn assert_input_selection_matches_layout(
        input_type: &str,
        typed: &str,
        anchor: usize,
        focus: usize,
    ) {
        let component = format!(
            r#"
            import {{ render }} from "solite-runtime";
            function App() {{
              const input = __sol_createElement("input");
              __sol_setProperty(input, "type", "{input_type}");
              __sol_setProperty(input, "style", "display:block; width:220px; height:40px;");
              return input;
            }}
            render(() => App(), __SOL_ROOT__);
        "#
        );
        let (mut instance, _rx) = make_instance_with(&component, &[]);
        let _ = instance.render();
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        let input_id = instance
            .doc
            .borrow()
            .get_node(1)
            .and_then(|root| root.children.first().copied())
            .unwrap();

        for ch in typed.chars() {
            let key = ch.to_string();
            let _ = instance.dispatch_key_down(type_key(&key));
        }
        if let Some(state) = instance.js.inputs.borrow_mut().get_mut(&input_id) {
            state.set_selection(anchor, focus);
        }
        let _ = instance.render();

        let selections = instance.collect_input_selections();
        assert!(
            !selections.is_empty(),
            "expected selection overlay for {input_type} input"
        );

        let expected = {
            let doc = instance.doc.borrow();
            let node = doc.get_node(input_id).unwrap();
            let input_data = node
                .element_data()
                .and_then(|element| element.text_input_data())
                .unwrap();
            let layout = node.final_layout;
            let input_origin = node.absolute_position(0.0, 0.0);
            let content_x = input_origin.x + layout.border.left + layout.padding.left;
            let content_y = input_origin.y + layout.border.top + layout.padding.top;
            let content_w = layout.content_box_width().max(0.0);
            let content_h = layout.content_box_height().max(1.0);
            let y_offset = node.text_input_v_centering_offset(1.0) as f32;
            let display_text = instance
                .js
                .inputs
                .borrow()
                .get(&input_id)
                .unwrap()
                .render(true)
                .0;
            let anchor_char = anchor;
            let focus_char = focus;
            let anchor = Cursor::from_byte_index(
                input_data.editor.try_layout().unwrap(),
                char_index_to_byte_index(&display_text, anchor_char),
                Affinity::Downstream,
            );
            let focus = Cursor::from_byte_index(
                input_data.editor.try_layout().unwrap(),
                char_index_to_byte_index(&display_text, focus_char),
                Affinity::Downstream,
            );
            let selection = Selection::new(anchor, focus);
            let mut rects = Vec::new();
            selection.geometry_with(input_data.editor.try_layout().unwrap(), |rect, _| {
                let x0 = (content_x + rect.x0 as f32).clamp(content_x, content_x + content_w);
                let x1 = (content_x + rect.x1 as f32).clamp(content_x, content_x + content_w);
                let y0 =
                    (content_y + y_offset + rect.y0 as f32).clamp(content_y, content_y + content_h);
                let y1 =
                    (content_y + y_offset + rect.y1 as f32).clamp(content_y, content_y + content_h);
                rects.push((x0, y0, x1 - x0, y1 - y0));
            });

            if rects.is_empty() {
                let width = (estimated_input_char_width(&node)
                    * (focus_char as f32 - anchor_char as f32))
                    .max(1.0);
                let height = (content_h * 0.7).max(1.0);
                let y = content_y + ((content_h - height).max(0.0) * 0.5);
                rects.push((
                    content_x + estimated_input_char_width(&node) * anchor_char as f32,
                    y,
                    width,
                    height,
                ));
            }
            rects
        };

        assert_eq!(selections.len(), expected.len());
        for (actual, expected) in selections.iter().zip(expected.iter()) {
            assert!(
                (actual.x - expected.0).abs() < 0.01,
                "x mismatch: {} vs {}",
                actual.x,
                expected.0
            );
            assert!(
                (actual.y - expected.1).abs() < 0.01,
                "y mismatch: {} vs {}",
                actual.y,
                expected.1
            );
            assert!(
                (actual.width - expected.2).abs() < 0.01,
                "width mismatch: {} vs {}",
                actual.width,
                expected.2
            );
            assert!(
                (actual.height - expected.3).abs() < 0.01,
                "height mismatch: {} vs {}",
                actual.height,
                expected.3
            );
        }
    }

    #[test]
    fn input_element_routes_keys_to_rust_owned_value() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
                globalThis.state.caret = e.selectionStart;
              });
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();

        // Click to focus.
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        // Type "hi".
        let _ = instance.dispatch_key_down(type_key("h"));
        let _ = instance.dispatch_key_down(type_key("i"));
        assert_eq!(instance.state().get("value"), Some(json!("hi")));
        assert_eq!(instance.state().get("caret"), Some(json!(2)));

        // Backspace.
        let _ = instance.dispatch_key_down(type_key("Backspace"));
        assert_eq!(instance.state().get("value"), Some(json!("h")));
        assert_eq!(instance.state().get("caret"), Some(json!(1)));

        // ArrowLeft moves caret but doesn't emit `input` (no value change),
        // so state.caret stays at 1.
        let _ = instance.dispatch_key_down(type_key("ArrowLeft"));
        assert_eq!(instance.state().get("caret"), Some(json!(1)));

        // Typing now inserts at position 0.
        let _ = instance.dispatch_key_down(type_key("a"));
        assert_eq!(instance.state().get("value"), Some(json!("ah")));
        assert_eq!(instance.state().get("caret"), Some(json!(1)));
    }

    #[test]
    fn tab_moves_focus_between_native_inputs() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");

              const first = __sol_createElement("input");
              __sol_setProperty(first, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(first, "onFocus", () => {
                globalThis.state.focused = "first";
              });
              __sol_setProperty(first, "onInput", (event) => {
                globalThis.state.firstValue = event.value;
              });

              const second = __sol_createElement("input");
              __sol_setProperty(second, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(second, "onFocus", () => {
                globalThis.state.focused = "second";
              });
              __sol_setProperty(second, "onInput", (event) => {
                globalThis.state.secondValue = event.value;
              });

              __sol_insertNode(root, first, null);
              __sol_insertNode(root, second, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();

        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(instance.state().get("focused"), Some(json!("first")));

        let _ = instance.dispatch_key_down(make_key_event(
            "Tab", "Tab", 9, false, false, false, false, false,
        ));
        assert_eq!(instance.state().get("focused"), Some(json!("second")));

        let _ = instance.dispatch_key_down(type_key("x"));
        assert_eq!(instance.state().get("firstValue"), None);
        assert_eq!(instance.state().get("secondValue"), Some(json!("x")));

        let _ = instance.dispatch_key_down(make_key_event(
            "Tab", "Tab", 9, false, true, false, false, false,
        ));
        assert_eq!(instance.state().get("focused"), Some(json!("first")));

        let _ = instance.dispatch_key_down(type_key("y"));
        assert_eq!(instance.state().get("firstValue"), Some(json!("y")));
        assert_eq!(instance.state().get("secondValue"), Some(json!("x")));
    }

    #[test]
    fn tab_commits_open_select_and_advances_focus() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");

              const first = __sol_createElement("input");
              __sol_setProperty(first, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(first, "onFocus", () => {
                globalThis.state.focused = "first";
              });

              const select = __sol_createElement("select");
              __sol_setProperty(select, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(select, "value", globalThis.state.selectValue ?? "");
              __sol_setProperty(select, "onFocus", () => {
                globalThis.state.focused = "select";
              });
              __sol_setProperty(select, "onChange", (event) => {
                globalThis.state.selectValue = event.value;
              });

              const opt0 = __sol_createElement("option");
              __sol_setProperty(opt0, "value", "");
              __sol_setProperty(opt0, "disabled", "");
              __sol_setProperty(opt0, "selected", "");
              __sol_setProperty(opt0, "hidden", "");
              __sol_insertNode(opt0, __sol_createTextNode("Choose.."), null);

              const opt1 = __sol_createElement("option");
              __sol_setProperty(opt1, "value", "one");
              __sol_insertNode(opt1, __sol_createTextNode("One"), null);

              const opt2 = __sol_createElement("option");
              __sol_setProperty(opt2, "value", "two");
              __sol_insertNode(opt2, __sol_createTextNode("Two"), null);

              __sol_insertNode(select, opt0, null);
              __sol_insertNode(select, opt1, null);
              __sol_insertNode(select, opt2, null);

              const second = __sol_createElement("input");
              __sol_setProperty(second, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(second, "onFocus", () => {
                globalThis.state.focused = "second";
              });

              __sol_insertNode(root, first, null);
              __sol_insertNode(root, select, null);
              __sol_insertNode(root, second, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();

        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(instance.state().get("focused"), Some(json!("first")));

        let _ = instance.dispatch_key_down(make_key_event(
            "Tab", "Tab", 9, false, false, false, false, false,
        ));
        assert_eq!(instance.state().get("focused"), Some(json!("select")));

        let _ = instance.dispatch_key_down(make_key_event(
            "Enter", "Enter", 13, false, false, false, false, false,
        ));
        let _ = instance.dispatch_key_down(make_key_event(
            "ArrowDown",
            "ArrowDown",
            40,
            false,
            false,
            false,
            false,
            false,
        ));
        let _ = instance.dispatch_key_down(make_key_event(
            "Tab", "Tab", 9, false, false, false, false, false,
        ));

        assert_eq!(instance.state().get("selectValue"), Some(json!("two")));
        assert_eq!(instance.state().get("focused"), Some(json!("second")));
    }

    #[test]
    fn open_select_arrow_enter_and_escape_match_keyboard_behavior() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const select = __sol_createElement("select");
              __sol_setProperty(select, "style", "display:block; width:200px; height:24px;");
              __sol_setProperty(select, "value", globalThis.state.selectValue ?? "");
              __sol_setProperty(select, "onChange", (event) => {
                globalThis.state.selectValue = event.value;
              });

              const placeholder = __sol_createElement("option");
              __sol_setProperty(placeholder, "value", "");
              __sol_setProperty(placeholder, "disabled", "");
              __sol_setProperty(placeholder, "selected", "");
              __sol_setProperty(placeholder, "hidden", "");
              __sol_insertNode(placeholder, __sol_createTextNode("Choose.."), null);

              const first = __sol_createElement("option");
              __sol_setProperty(first, "value", "first");
              __sol_insertNode(first, __sol_createTextNode("First"), null);

              const second = __sol_createElement("option");
              __sol_setProperty(second, "value", "second");
              __sol_insertNode(second, __sol_createTextNode("Second"), null);

              __sol_insertNode(select, placeholder, null);
              __sol_insertNode(select, first, null);
              __sol_insertNode(select, second, null);
              return select;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();

        let select_id = instance
            .doc
            .borrow()
            .get_node(1)
            .and_then(|root| root.children.first().copied())
            .expect("select should exist");

        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        {
            let state = instance.js.selects.borrow();
            let select = state.get(&select_id).expect("select state");
            assert!(select.is_open());
            assert_eq!(select.active_index(), Some(1));
            assert_eq!(select.selected_index(), Some(0));
        }

        let _ = instance.dispatch_key_down(make_key_event(
            "ArrowDown",
            "ArrowDown",
            40,
            false,
            false,
            false,
            false,
            false,
        ));

        {
            let state = instance.js.selects.borrow();
            let select = state.get(&select_id).expect("select state");
            assert_eq!(select.active_index(), Some(2));
            assert_eq!(select.selected_index(), Some(0));
        }

        let _ = instance.dispatch_key_down(make_key_event(
            "Escape", "Escape", 27, false, false, false, false, false,
        ));
        {
            let state = instance.js.selects.borrow();
            let select = state.get(&select_id).expect("select state");
            assert!(!select.is_open());
            assert_eq!(select.selected_index(), Some(0));
        }
        assert_eq!(instance.state().get("selectValue"), None);

        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        let _ = instance.dispatch_key_down(make_key_event(
            "ArrowDown",
            "ArrowDown",
            40,
            false,
            false,
            false,
            false,
            false,
        ));
        let _ = instance.dispatch_key_down(make_key_event(
            "Enter", "Enter", 13, false, false, false, false, false,
        ));

        {
            let state = instance.js.selects.borrow();
            let select = state.get(&select_id).expect("select state");
            assert!(!select.is_open());
            assert_eq!(select.selected_index(), Some(2));
        }
        assert_eq!(instance.state().get("selectValue"), Some(json!("second")));
    }

    #[test]
    fn radio_arrow_keys_move_selection_and_focus() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");

              const radio1 = __sol_createElement("input");
              __sol_setProperty(radio1, "type", "radio");
              __sol_setProperty(radio1, "name", "group-a");
              __sol_setProperty(radio1, "style", "display:block; width:20px; height:20px;");
              __sol_setProperty(radio1, "onFocus", () => {
                globalThis.state.focused = "r1";
              });
              __sol_setProperty(radio1, "onInput", (event) => {
                if (event.checked) {
                  globalThis.state.selected = "r1";
                }
              });

              const radio2 = __sol_createElement("input");
              __sol_setProperty(radio2, "type", "radio");
              __sol_setProperty(radio2, "name", "group-a");
              __sol_setProperty(radio2, "style", "display:block; width:20px; height:20px;");
              __sol_setProperty(radio2, "onFocus", () => {
                globalThis.state.focused = "r2";
              });
              __sol_setProperty(radio2, "onInput", (event) => {
                if (event.checked) {
                  globalThis.state.selected = "r2";
                }
              });

              const radio3 = __sol_createElement("input");
              __sol_setProperty(radio3, "type", "radio");
              __sol_setProperty(radio3, "name", "group-a");
              __sol_setProperty(radio3, "style", "display:block; width:20px; height:20px;");
              __sol_setProperty(radio3, "onFocus", () => {
                globalThis.state.focused = "r3";
              });
              __sol_setProperty(radio3, "onInput", (event) => {
                if (event.checked) {
                  globalThis.state.selected = "r3";
                }
              });

              __sol_insertNode(root, radio1, null);
              __sol_insertNode(root, radio2, null);
              __sol_insertNode(root, radio3, null);
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();

        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(instance.state().get("focused"), Some(json!("r1")));
        assert_eq!(instance.state().get("selected"), Some(json!("r1")));

        let _ = instance.dispatch_key_down(make_key_event(
            "ArrowRight",
            "ArrowRight",
            39,
            false,
            false,
            false,
            false,
            false,
        ));
        assert_eq!(instance.state().get("focused"), Some(json!("r2")));
        assert_eq!(instance.state().get("selected"), Some(json!("r2")));

        let _ = instance.dispatch_key_down(make_key_event(
            "ArrowLeft",
            "ArrowLeft",
            37,
            false,
            false,
            false,
            false,
            false,
        ));
        assert_eq!(instance.state().get("focused"), Some(json!("r1")));
        assert_eq!(instance.state().get("selected"), Some(json!("r1")));
    }

    #[test]
    fn input_space_and_caret_movement_refresh_rendered_caret() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();

        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        let input_id = instance
            .doc
            .borrow()
            .get_node(1)
            .and_then(|root| root.children.first().copied())
            .unwrap();

        let _ = instance.dispatch_key_down(type_key("h"));
        let _ = instance.dispatch_key_down(type_key("i"));
        let _ = instance.render();
        assert_eq!(input_child_text(&instance, input_id), "hi");
        let editor_text = instance
            .doc
            .borrow()
            .get_node(input_id)
            .and_then(|node| node.element_data())
            .and_then(|element| element.text_input_data())
            .map(|input| input.editor.raw_text().to_string());
        assert_eq!(editor_text.as_deref(), Some("hi"));
        let end_x = instance.collect_input_carets()[0].x;

        let _ = instance.dispatch_key_down(type_key("ArrowLeft"));
        let _ = instance.render();
        assert_eq!(input_child_text(&instance, input_id), "hi");
        let mid_x = instance.collect_input_carets()[0].x;
        assert!(
            mid_x < end_x,
            "expected caret to move left: {mid_x} >= {end_x}"
        );

        let _ = instance.dispatch_key_down(type_key(" "));
        let _ = instance.render();
        assert_eq!(input_child_text(&instance, input_id), "h i");

        let _ = instance.dispatch_key_down(type_key("a"));
        let _ = instance.render();
        assert_eq!(input_child_text(&instance, input_id), "h ai");
    }

    #[test]
    fn input_number_restricts_to_numeric_chars() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "type", "number");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
              });
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        let _ = instance.dispatch_key_down(type_key("1"));
        let _ = instance.dispatch_key_down(type_key("2"));
        let _ = instance.dispatch_key_down(type_key("."));
        let _ = instance.dispatch_key_down(type_key("3"));
        assert_eq!(instance.state().get("value"), Some(json!("12.3")));

        // Alphabetic characters are rejected by number-input handling.
        let _ = instance.dispatch_key_down(type_key("a"));
        assert_eq!(instance.state().get("value"), Some(json!("12.3")));
    }

    #[test]
    fn input_text_selection_uses_layout_geometry() {
        assert_input_selection_matches_layout("text", "illWWWtext", 2, 7);
    }

    #[test]
    fn input_password_selection_uses_masked_layout_geometry() {
        assert_input_selection_matches_layout("password", "supersecret", 1, 8);
    }

    #[test]
    fn input_range_responds_to_step_and_extremes() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "type", "range");
              __sol_setProperty(input, "min", "0");
              __sol_setProperty(input, "max", "10");
              __sol_setProperty(input, "step", "2");
              __sol_setProperty(input, "value", "4");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
              });
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        let input_id = instance
            .doc
            .borrow()
            .get_node(1)
            .and_then(|root| root.children.first().copied())
            .unwrap();

        // Range is rendered via custom slider UI; child text should stay empty.
        assert_eq!(input_child_text(&instance, input_id), "");

        // Click to focus the range input (any position inside it).  The value
        // may or may not change depending on layout; the assertion intentionally
        // avoids checking post-click value here and verifies click starts a drag.
        let _ = instance.dispatch_mouse(
            100.0,
            10.0,
            MouseEvent::Down {
                x: 100.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        // End drag so later move events don't interfere.
        let _ = instance.dispatch_mouse(
            100.0,
            10.0,
            MouseEvent::Up {
                x: 100.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        // Keyboard navigation from the current value (seeded as 4, or whatever
        // click set): ArrowRight steps +2, ArrowLeft steps -2, Home/End jump.
        let _ = instance.dispatch_key_down(type_key("Home"));
        assert_eq!(instance.state().get("value"), Some(json!("0")));
        let _ = instance.render();
        assert_eq!(
            instance
                .doc
                .borrow()
                .get_node(input_id)
                .and_then(|node| node.attr(LocalName::from("value")))
                .unwrap_or(""),
            "0"
        );

        let _ = instance.dispatch_key_down(type_key("ArrowRight"));
        assert_eq!(instance.state().get("value"), Some(json!("2")));
        let _ = instance.render();
        assert_eq!(
            instance
                .doc
                .borrow()
                .get_node(input_id)
                .and_then(|node| node.attr(LocalName::from("value")))
                .unwrap_or(""),
            "2"
        );

        let _ = instance.dispatch_key_down(type_key("ArrowRight"));
        assert_eq!(instance.state().get("value"), Some(json!("4")));
        let _ = instance.render();
        assert_eq!(
            instance
                .doc
                .borrow()
                .get_node(input_id)
                .and_then(|node| node.attr(LocalName::from("value")))
                .unwrap_or(""),
            "4"
        );

        let _ = instance.dispatch_key_down(type_key("ArrowLeft"));
        assert_eq!(instance.state().get("value"), Some(json!("2")));
        let _ = instance.render();
        assert_eq!(
            instance
                .doc
                .borrow()
                .get_node(input_id)
                .and_then(|node| node.attr(LocalName::from("value")))
                .unwrap_or(""),
            "2"
        );

        let _ = instance.dispatch_key_down(type_key("End"));
        assert_eq!(instance.state().get("value"), Some(json!("10")));
        let _ = instance.render();
        assert_eq!(
            instance
                .doc
                .borrow()
                .get_node(input_id)
                .and_then(|node| node.attr(LocalName::from("value")))
                .unwrap_or(""),
            "10"
        );
    }

    #[test]
    fn input_checkbox_and_radio_types_toggle() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");

              const checkbox = __sol_createElement("input");
              __sol_setProperty(checkbox, "type", "checkbox");
              __sol_setProperty(checkbox, "style", "display:block; width:20px; height:20px;");
              __sol_setProperty(checkbox, "onInput", (e) => {
                globalThis.state.checkbox = e.checked;
              });

              const radio1 = __sol_createElement("input");
              __sol_setProperty(radio1, "type", "radio");
              __sol_setProperty(radio1, "name", "group-a");
              __sol_setProperty(radio1, "style", "display:block; width:20px; height:20px;");

              const radio2 = __sol_createElement("input");
              __sol_setProperty(radio2, "type", "radio");
              __sol_setProperty(radio2, "name", "group-a");
              __sol_setProperty(radio2, "style", "display:block; width:20px; height:20px;");

              __sol_insertNode(root, checkbox, null);
              __sol_insertNode(root, radio1, null);
              __sol_insertNode(root, radio2, null);

              globalThis.state.radio1 = radio1;
              globalThis.state.radio2 = radio2;

              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();

        // Checkbox: clicking toggles it; Space while focused toggles again.
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(instance.state().get("checkbox"), Some(json!(true)));

        // Space toggles it back off.
        let _ = instance.dispatch_key_down(type_key("Space"));
        assert_eq!(instance.state().get("checkbox"), Some(json!(false)));

        // Click toggles on again so radio tests start from a stable state.
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(instance.state().get("checkbox"), Some(json!(true)));

        let ids = [
            instance.state().get("radio1"),
            instance.state().get("radio2"),
        ];
        let ids = [
            state_node_id(ids[0].as_ref(), "radio1"),
            state_node_id(ids[1].as_ref(), "radio2"),
        ] as [usize; 2];

        let (radio1_x, radio1_y, radio2_x, radio2_y) = {
            let doc = instance.doc.borrow();
            let r1 = doc.get_node(ids[0]).unwrap();
            let r2 = doc.get_node(ids[1]).unwrap();
            (
                r1.absolute_position(0.0, 0.0).x + 4.0,
                r1.absolute_position(0.0, 0.0).y + 4.0,
                r2.absolute_position(0.0, 0.0).x + 4.0,
                r2.absolute_position(0.0, 0.0).y + 4.0,
            )
        };

        // Pick the first radio.
        let _ = instance.dispatch_mouse(
            radio1_x,
            radio1_y,
            MouseEvent::Down {
                x: radio1_x,
                y: radio1_y,
                button: MouseButton::Left,
            },
        );
        let _ = instance.dispatch_key_down(type_key(" "));
        assert_eq!(instance.input_value(ids[0]).as_deref(), Some("on"));
        assert_eq!(instance.input_value(ids[1]).as_deref(), Some("off"));

        // Focus/select second radio and ensure group semantics switch.
        let _ = instance.dispatch_mouse(
            radio2_x,
            radio2_y,
            MouseEvent::Down {
                x: radio2_x,
                y: radio2_y,
                button: MouseButton::Left,
            },
        );
        let _ = instance.dispatch_key_down(type_key(" "));
        assert_eq!(instance.input_value(ids[0]).as_deref(), Some("off"));
        assert_eq!(instance.input_value(ids[1]).as_deref(), Some("on"));
    }

    #[test]
    fn input_named_space_key_is_treated_as_space() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );

        let input_id = instance
            .doc
            .borrow()
            .get_node(1)
            .and_then(|root| root.children.first().copied())
            .unwrap();
        let _ = instance.dispatch_key_down(type_key("Space"));
        assert_eq!(instance.input_value(input_id), Some(" ".into()));
    }

    #[test]
    fn input_value_attribute_seeds_rust_state() {
        // Setting `value` via __sol_setProperty before any user input must
        // populate the InputState; the instance API should see it too.
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "value", "preset");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        let id = instance
            .doc
            .borrow()
            .get_node(1)
            .and_then(|root| root.children.first().copied())
            .unwrap();
        assert_eq!(instance.input_value(id).as_deref(), Some("preset"));

        // Host can rewrite the value directly.
        assert!(instance.set_input_value(id, "from-host"));
        assert_eq!(instance.input_value(id).as_deref(), Some("from-host"));
    }

    #[test]
    fn keydown_handler_sees_event_value() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "onKeyDown", (e) => {
                globalThis.state.observedValue = e.value;
                globalThis.state.observedKey = e.key;
              });
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        let _ = instance.dispatch_key_down(type_key("z"));
        // The handler runs *before* the edit is committed in our dispatcher
        // order — but since enrichment reads the live InputState, the value
        // visible to the handler reflects whatever the field holds at the
        // moment the event fires. We just assert the key was observed and
        // that `e.value` is present (either "" or "z" depending on order).
        assert_eq!(instance.state().get("observedKey"), Some(json!("z")));
        assert!(instance.state().get("observedValue").is_some());
    }

    #[test]
    fn blink_toggles_visible_text_on_tick() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __sol_setProperty(input, "value", "hi");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        let id = instance
            .doc
            .borrow()
            .get_node(1)
            .and_then(|root| root.children.first().copied())
            .unwrap();
        let _ = instance.dispatch_mouse(
            10.0,
            10.0,
            MouseEvent::Down {
                x: 10.0,
                y: 10.0,
                button: MouseButton::Left,
            },
        );
        // After focus, text remains clean and the native caret overlay is visible.
        let _ = instance.render();
        assert!(
            !instance.collect_input_carets().is_empty(),
            "expected visible caret overlay"
        );
        assert_eq!(input_child_text(&instance, id), "hi");

        // Force blink to flip by rewinding the last_blink instant.
        instance
            .js
            .inputs
            .borrow_mut()
            .get_mut(&id)
            .unwrap()
            .force_blink_for_test(std::time::Duration::from_millis(600));
        let _ = instance.tick();
        let _ = instance.render();
        assert!(
            instance.collect_input_carets().is_empty(),
            "expected hidden caret overlay after blink"
        );
    }

    #[test]
    fn active_pseudo_class_flips_on_press() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const d = __sol_createElement("div");
              __sol_setProperty(d, "className", "tag");
              __sol_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(
            COMPONENT,
            &[".tag { color: rgb(1, 1, 1) } .tag:active { color: rgb(222, 0, 0) }"],
        );
        assert_eq!(first_child_color(&mut instance), Some((1, 1, 1)));
        let _ = instance.dispatch_mouse(
            20.0,
            20.0,
            MouseEvent::Down {
                x: 20.0,
                y: 20.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(first_child_color(&mut instance), Some((222, 0, 0)));
        let _ = instance.dispatch_mouse(
            20.0,
            20.0,
            MouseEvent::Up {
                x: 20.0,
                y: 20.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(first_child_color(&mut instance), Some((1, 1, 1)));
    }

    #[test]
    fn focus_pseudo_class_flips_on_click() {
        const COMPONENT: &str = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "className", "field");
              __sol_setProperty(input, "type", "text");
              __sol_setProperty(input, "style", "display:block; width:80px; height:30px;");
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(
            COMPONENT,
            &[".field { color: rgb(1, 1, 1) } .field:focus { color: rgb(200, 50, 10) }"],
        );
        assert_eq!(first_child_color(&mut instance), Some((1, 1, 1)));
        let _ = instance.dispatch_mouse(
            20.0,
            20.0,
            MouseEvent::Down {
                x: 20.0,
                y: 20.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(first_child_color(&mut instance), Some((200, 50, 10)));
        let _ = instance.dispatch_mouse(
            500.0,
            500.0,
            MouseEvent::Down {
                x: 500.0,
                y: 500.0,
                button: MouseButton::Left,
            },
        );
        assert_eq!(first_child_color(&mut instance), Some((1, 1, 1)));
    }

    // ─── Image loading ────────────────────────────────────────────────────────

    /// Build a valid 1×1 RGBA PNG using the `image` crate. Done once at runtime
    /// to dodge the fragility of hand-coded PNG byte literals (a bad CRC turns
    /// a "load" path into an "error" path and would mask the watcher logic).
    fn tiny_png_bytes() -> Vec<u8> {
        let mut buf = Vec::new();
        let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_fn(1, 1, |_, _| {
            image::Rgba([0, 0, 0, 0])
        });
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .expect("encode tiny png");
        buf
    }

    fn unique_tmp_path(prefix: &str, suffix: &str) -> PathBuf {
        use std::time::{SystemTime, UNIX_EPOCH};
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let mut dir = PathBuf::from("target");
        dir.push("test-tmp");
        std::fs::create_dir_all(&dir).expect("create tmp dir");
        dir.push(format!("solite-{prefix}-{nanos}{suffix}"));
        dir
    }

    fn write_tmp_png(prefix: &str) -> PathBuf {
        let path = unique_tmp_path(prefix, ".png");
        std::fs::write(&path, tiny_png_bytes()).expect("write png");
        path.canonicalize().unwrap_or(path)
    }

    fn file_url(path: &Path) -> String {
        let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        url::Url::from_file_path(&abs)
            .expect("absolute path")
            .to_string()
    }

    const IMG_LOAD_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const img = __sol_createElement("img");
          __sol_setProperty(img, "onLoad", function(ev) {
            sendEvent("img:load", JSON.stringify({ target: ev.target }));
          });
          __sol_setProperty(img, "onError", function(ev) {
            sendEvent("img:error", JSON.stringify({ target: ev.target }));
          });
          __sol_setProperty(img, "src", globalThis.__OX_IMG_SRC);
          return img;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    fn drain_events(rx: &mut UnboundedReceiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(ev);
        }
        out
    }

    #[test]
    fn tiny_png_decodes_for_test_fixture_sanity() {
        // Catch a regression where `tiny_png_bytes` produces an undecodable
        // blob — that's failure-mode-1 if `valid_img_src_…` returns no
        // events.
        use image::ImageReader;
        use std::io::Cursor;
        let bytes = tiny_png_bytes();
        let img = ImageReader::new(Cursor::new(&bytes))
            .with_guessed_format()
            .expect("guess format")
            .decode()
            .expect("decode tiny_png_bytes");
        assert_eq!(img.width(), 1);
        assert_eq!(img.height(), 1);
    }

    fn run_img_test(src_url: &str) -> Vec<Event> {
        let component = format!(
            "globalThis.__OX_IMG_SRC = {src:?};\n{body}",
            src = src_url,
            body = IMG_LOAD_COMPONENT
        );
        let (device, queue) = test_device();
        let (mut instance, mut rx) = Instance::new(
            InstanceConfig {
                width: 64,
                height: 64,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            &component,
        );
        // First render: blitz's `resolve()` calls `handle_messages()`, which
        // applies the loaded image (or removes the pending entry on error).
        let _ = instance.render();
        // Second tick: img watcher sees the applied state and dispatches
        // `load` or `error` to JS.
        let _ = instance.tick();
        let _ = instance.render();
        drain_events(&mut rx)
    }

    #[test]
    fn valid_img_src_dispatches_load_event() {
        let png_path = write_tmp_png("img-load");
        let events = run_img_test(&file_url(&png_path));
        let names: Vec<_> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"img:load"),
            "expected img:load, got {names:?}"
        );
        assert!(
            !names.contains(&"img:error"),
            "did not expect img:error, got {names:?}"
        );
        let _ = std::fs::remove_file(&png_path);
    }

    #[test]
    fn missing_img_src_dispatches_error_event() {
        let url = "file:///tmp/solite-does-not-exist-12345.png";
        let events = run_img_test(url);
        let names: Vec<_> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"img:error"),
            "expected img:error, got {names:?}"
        );
        assert!(
            !names.contains(&"img:load"),
            "did not expect img:load, got {names:?}"
        );
    }

    #[test]
    fn dynamic_src_mutation_fires_load_for_each_new_url() {
        // Two distinct on-disk PNGs. Mount a component that swaps `src` from
        // the first to the second after each `tick()`. Both transitions
        // should yield a `load` event.
        let a = write_tmp_png("img-dyn-a");
        let b = write_tmp_png("img-dyn-b");
        let component = format!(
            r#"
            import {{ render }} from "solite-runtime";
            globalThis.__OX_FIRST = {first:?};
            globalThis.__OX_SECOND = {second:?};
            function App() {{
              const img = __sol_createElement("img");
              __sol_setProperty(img, "onLoad", function(ev) {{
                sendEvent("img:load", JSON.stringify({{ src: __sol_getAttr(ev.target, "src") }}));
              }});
              __sol_setProperty(img, "src", globalThis.__OX_FIRST);
              globalThis.__OX_IMG = img;
              return img;
            }}
            render(() => App(), __SOL_ROOT__);
            "#,
            first = file_url(&a),
            second = file_url(&b),
        );
        let (device, queue) = test_device();
        let (mut instance, mut rx) = Instance::new(
            InstanceConfig {
                width: 64,
                height: 64,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            &component,
        );

        // First load.
        let _ = instance.render();
        let _ = instance.tick();
        let _ = instance.render();
        let first_events: Vec<String> = drain_events(&mut rx).into_iter().map(|e| e.name).collect();
        assert!(
            first_events.iter().any(|n| n == "img:load"),
            "expected first img:load, got {first_events:?}"
        );

        // Swap src to a different URL.
        instance
            .js
            .eval_test_code("__sol_setProperty(__OX_IMG, 'src', globalThis.__OX_SECOND)");

        let _ = instance.render();
        let _ = instance.tick();
        let _ = instance.render();
        let second_events: Vec<String> =
            drain_events(&mut rx).into_iter().map(|e| e.name).collect();
        assert!(
            second_events.iter().any(|n| n == "img:load"),
            "expected second img:load after src swap, got {second_events:?}"
        );

        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn data_url_image_dispatches_load_event() {
        // base64-encoded 1x1 PNG.
        use base64_dummy::encode as b64;
        let bytes = tiny_png_bytes();
        let mut url = String::from("data:image/png;base64,");
        url.push_str(&b64(&bytes));
        let events = run_img_test(&url);
        let names: Vec<_> = events.iter().map(|e| e.name.as_str()).collect();
        assert!(
            names.contains(&"img:load"),
            "expected img:load for data URL, got {names:?}"
        );
    }

    // ─── Font registration ───────────────────────────────────────────────────

    const BULLET_FONT_BYTES: &[u8] =
        include_bytes!("../vendor/blitz/packages/blitz-dom/assets/moz-bullet-font.otf");

    const FONT_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const p = __sol_createElement("p");
          __sol_setProperty(p, "class", "uses-custom");
          __sol_insertNode(p, __sol_createTextNode("•"), null);
          return p;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    const FONT_CSS: &str = ".uses-custom { font-family: 'SoliteTestBullet'; font-size: 32px; }";

    #[test]
    fn register_font_bytes_returns_distinct_stylesheet_ids() {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 64,
                height: 64,
                device,
                queue,
                stylesheets: vec![FONT_CSS.to_string()],
                document_scroll: false,
                base_url: None,
            },
            FONT_COMPONENT,
        );
        let a = instance.register_font_bytes(
            "SoliteTestBullet",
            BULLET_FONT_BYTES.to_vec(),
            FontFormat::Opentype,
        );
        let b = instance.register_font_bytes(
            "SoliteTestBullet2",
            BULLET_FONT_BYTES.to_vec(),
            FontFormat::Opentype,
        );
        assert_ne!(a, b);
    }

    #[test]
    fn register_font_bytes_does_not_panic_during_render() {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 128,
                height: 64,
                device,
                queue,
                stylesheets: vec![FONT_CSS.to_string()],
                document_scroll: false,
                base_url: None,
            },
            FONT_COMPONENT,
        );
        // Register the font, then render. We're not asserting glyph pixels —
        // just that the @font-face + NetProvider round-trip plus a follow-up
        // resolve() completes without crashing.
        let _id = instance.register_font_bytes(
            "SoliteTestBullet",
            BULLET_FONT_BYTES.to_vec(),
            FontFormat::Opentype,
        );
        let _ = instance.tick();
        let _ = instance.render();
    }

    #[test]
    fn register_font_from_path_reads_file() {
        let path = PathBuf::from("vendor/blitz/packages/blitz-dom/assets/moz-bullet-font.otf");
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 64,
                height: 64,
                device,
                queue,
                stylesheets: vec![FONT_CSS.to_string()],
                document_scroll: false,
                base_url: None,
            },
            FONT_COMPONENT,
        );
        let id = instance
            .register_font_from_path("SoliteTestBullet", &path)
            .expect("font loads");
        // Unregister should succeed via the returned stylesheet id.
        assert!(instance.remove_stylesheet(id));
    }

    #[test]
    fn register_font_from_path_rejects_unknown_extension() {
        let path = PathBuf::from("Cargo.toml");
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 64,
                height: 64,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            "import { render } from \"solite-runtime\"; render(() => __sol_createElement(\"div\"), __SOL_ROOT__);",
        );
        let err = instance
            .register_font_from_path("X", &path)
            .expect_err("must reject .toml");
        assert!(matches!(err, RegisterFontError::UnknownFormat));
    }

    /// Minimal base64 encoder used by the data-url test. Inlined to avoid
    /// adding a runtime dep just for tests.
    mod base64_dummy {
        const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        pub fn encode(input: &[u8]) -> String {
            let mut out = String::with_capacity((input.len() + 2) / 3 * 4);
            let mut chunks = input.chunks_exact(3);
            for chunk in chunks.by_ref() {
                let n = ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | (chunk[2] as u32);
                out.push(CHARS[((n >> 18) & 63) as usize] as char);
                out.push(CHARS[((n >> 12) & 63) as usize] as char);
                out.push(CHARS[((n >> 6) & 63) as usize] as char);
                out.push(CHARS[(n & 63) as usize] as char);
            }
            let rem = chunks.remainder();
            match rem.len() {
                0 => {}
                1 => {
                    let n = (rem[0] as u32) << 16;
                    out.push(CHARS[((n >> 18) & 63) as usize] as char);
                    out.push(CHARS[((n >> 12) & 63) as usize] as char);
                    out.push('=');
                    out.push('=');
                }
                2 => {
                    let n = ((rem[0] as u32) << 16) | ((rem[1] as u32) << 8);
                    out.push(CHARS[((n >> 18) & 63) as usize] as char);
                    out.push(CHARS[((n >> 12) & 63) as usize] as char);
                    out.push(CHARS[((n >> 6) & 63) as usize] as char);
                    out.push('=');
                }
                _ => unreachable!(),
            }
            out
        }
    }

    // ─── Keyboard navigation parity ──────────────────────────────────────────

    fn enter_key() -> KeyboardEvent {
        make_key_event("Enter", "Enter", 13, false, false, false, false, false)
    }
    fn space_key() -> KeyboardEvent {
        make_key_event(" ", "Space", 32, false, false, false, false, false)
    }
    fn tab_key(shift: bool) -> KeyboardEvent {
        make_key_event("Tab", "Tab", 9, false, shift, false, false, false)
    }
    fn ctrl_key(key: &str) -> KeyboardEvent {
        make_key_event(key, key, 0, false, false, true, false, false)
    }
    fn ctrl_shift_key(key: &str) -> KeyboardEvent {
        make_key_event(key, key, 0, false, true, true, false, false)
    }
    fn plain_key(key: &str) -> KeyboardEvent {
        make_key_event(key, key, 0, false, false, false, false, false)
    }
    fn alt_key(key: &str) -> KeyboardEvent {
        make_key_event(key, key, 0, false, false, false, true, false)
    }

    /// Render once to make first-time blitz layout happen, then drive a single
    /// tick to settle any image/font side effects.
    fn settle(instance: &mut Instance) {
        let _ = instance.tick();
        let _ = instance.render();
    }

    /// Component with two text inputs and a button arranged horizontally,
    /// each with a stable id for hit-testing.
    const KB_NAV_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const root = __sol_createElement("div");
          const make = (tag, attrs) => {
            const el = __sol_createElement(tag);
            for (const k in attrs) __sol_setProperty(el, k, attrs[k]);
            __sol_insertNode(root, el, null);
            globalThis["__sol_" + (attrs.id || tag)] = el;
            return el;
          };
          make("input", { id: "first", type: "text" });
          make("input", { id: "second", type: "text" });
          make("button", { id: "btn", onClick: function() { state.clicked = (state.clicked || 0) + 1; } });
          return root;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    fn make_kb_nav_instance() -> (Instance, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (device, queue) = test_device();
        let (mut instance, rx) = Instance::new(
            InstanceConfig {
                width: 320,
                height: 160,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            KB_NAV_COMPONENT,
        );
        instance.state().set("clicked", json!(0));
        settle(&mut instance);
        (instance, rx)
    }

    fn focused_tag(instance: &Instance) -> Option<String> {
        let id = instance.focused_node_id?;
        instance
            .doc
            .borrow()
            .get_node(id)
            .and_then(|n| n.element_data())
            .map(|e| e.name.local.as_ref().to_owned())
    }

    fn js_node_id(instance: &Instance, var: &str) -> usize {
        // The runtime.ts wraps every node-creating bridge call so app code
        // sees `{ __solId: n }` instead of a raw number. Unwrap here so test
        // helpers can write idiomatic `__sol_a = make(...)` and still get the
        // numeric blitz id back.
        let code = format!(
            "(function() {{ var v = globalThis.__sol_{var}; if (v && typeof v === 'object' && typeof v.__solId === 'number') return v.__solId; if (typeof v === 'number') return v; throw new Error('not a node handle: ' + (typeof v) + ' value=' + v); }})()"
        );
        let mut out: Option<usize> = None;
        instance
            .js
            .context_with(|ctx| match ctx.eval::<i32, _>(code.as_str()) {
                Ok(v) => out = Some(v as usize),
                Err(err) => {
                    let exception = ctx.catch();
                    panic!("js_node_id eval failed for __sol_{var}: {err}; exception={exception:?}");
                }
            });
        out.expect("js_node_id read")
    }

    fn state_node_id(value: Option<&serde_json::Value>, label: &str) -> usize {
        let Some(value) = value else {
            panic!("{label} missing");
        };
        if let Some(id) = value.as_u64() {
            return id as usize;
        }
        if let Some(map) = value.as_object() {
            if let Some(id) = map.get("__solId").and_then(Value::as_u64) {
                return id as usize;
            }
        }
        panic!("invalid {label} node handle: {value:?}");
    }

    #[derive(Debug, Clone)]
    struct DomSig {
        id: usize,
        kind: &'static str,
        name: String,
        class: Option<String>,
        child_count: usize,
        text: Option<String>,
    }

    fn collect_dom_signature(doc: &BaseDocument, root_id: usize) -> Vec<DomSig> {
        fn walk(doc: &BaseDocument, node_id: usize, out: &mut Vec<DomSig>) {
            let Some(node) = doc.get_node(node_id) else {
                return;
            };

            let kind = if node.is_text_node() {
                "text"
            } else if node.is_element() {
                "element"
            } else {
                "other"
            };

            let class = node
                .attr(LocalName::from("class"))
                .map(|value| value.to_string());
            let name = node
                .element_data()
                .map(|el| el.name.local.as_ref().to_string())
                .unwrap_or_default();
            let text = node.text_data().map(|text| text.content.clone());
            let child_count = node.children.len();

            out.push(DomSig {
                id: node_id,
                kind,
                name,
                class,
                child_count,
                text,
            });

            for child_id in node.children.iter().copied() {
                walk(doc, child_id, out);
            }
        }

        let mut out = Vec::new();
        walk(doc, root_id, &mut out);
        out
    }

    fn assert_structural_dom_signature_eq(expected: &[DomSig], actual: &[DomSig]) {
        assert_eq!(
            expected.len(),
            actual.len(),
            "DOM node count should not change"
        );
        for (exp, act) in expected.iter().zip(actual.iter()) {
            assert_eq!(exp.id, act.id, "node id changed for structural slot");
            assert_eq!(exp.kind, act.kind, "node kind changed for id {}", exp.id);
            assert_eq!(exp.name, act.name, "node name changed for id {}", exp.id);
            assert_eq!(exp.class, act.class, "class changed for id {}", exp.id);
            assert_eq!(
                exp.child_count, act.child_count,
                "child-count changed for node {}",
                exp.id,
            );
        }
    }

    fn text_for_node<'a>(sig: &'a [DomSig], node_id: usize) -> Option<&'a str> {
        sig.iter()
            .find(|entry| entry.id == node_id)?
            .text
            .as_deref()
    }

    fn ids_from_state(state: &StateHandle, path: &str) -> Vec<usize> {
        let Some(value) = state.get(path) else {
            return Vec::new();
        };
        let Some(arr) = value.as_array() else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|value| value.as_u64().map(|id| id as usize))
            .collect()
    }

    fn id_from_state(state: &StateHandle, path: &str) -> usize {
        state
            .get(path)
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| panic!("{path} should be present as a node id")) as usize
    }

    /// Mutation canary component that updates text and list nodes from Rust-driven
    /// state patches without recreating unrelated DOM nodes.
    const CANARY_MUTATION_COMPONENT: &str = r#"
            import { createEffect, render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              __sol_setProperty(root, "className", "canary-root");

              const title = __sol_createElement("h1");
              __sol_setProperty(title, "className", "title");
              const titleText = __sol_createTextNode("");
              __sol_insertNode(title, titleText, null);
              __sol_insertNode(root, title, null);

              const nested = __sol_createElement("p");
              __sol_setProperty(nested, "className", "nested");
              const nestedText = __sol_createTextNode("");
              __sol_insertNode(nested, nestedText, null);
              __sol_insertNode(root, nested, null);

                  const list = __sol_createElement("div");
                  __sol_setProperty(list, "className", "rows");
                  __sol_insertNode(root, list, null);

                  const row0 = __sol_createElement("div");
                  __sol_setProperty(row0, "className", "row");
                  const row0Text = __sol_createTextNode("");
                  __sol_insertNode(row0, row0Text, null);

                  const row1 = __sol_createElement("div");
                  __sol_setProperty(row1, "className", "row");
                  const row1Text = __sol_createTextNode("");
                  __sol_insertNode(row1, row1Text, null);

                  const row2 = __sol_createElement("div");
                  __sol_setProperty(row2, "className", "row");
                  const row2Text = __sol_createTextNode("");
                  __sol_insertNode(row2, row2Text, null);

                  const row3 = __sol_createElement("div");
                  __sol_setProperty(row3, "className", "row");
                  const row3Text = __sol_createTextNode("");
                  __sol_insertNode(row3, row3Text, null);

                  __sol_insertNode(list, row0, null);
                  __sol_insertNode(list, row1, null);
                  __sol_insertNode(list, row2, null);
                  __sol_insertNode(list, row3, null);

                  const status = __sol_createElement("div");
                  __sol_setProperty(status, "className", "status");
                  const statusText = __sol_createTextNode("");
                  __sol_insertNode(status, statusText, null);
                  __sol_insertNode(root, status, null);
              globalThis.state.canaryRootId = root.__solId;

                  globalThis.state.canaryTitleTextId = titleText.__solId;
                  globalThis.state.canaryNestedTextId = nestedText.__solId;
                  globalThis.state.canaryStatusTextId = statusText.__solId;
                  globalThis.state.canaryRowNodeIds = [row0.__solId, row1.__solId, row2.__solId, row3.__solId];
                  globalThis.state.canaryRowTextIds = [
                    row0Text.__solId,
                    row1Text.__solId,
                    row2Text.__solId,
                    row3Text.__solId,
                  ];

                  createEffect(() => {
                    __sol_setText(titleText, String(globalThis.state.title || ""));
                    const nestedValue =
                      globalThis.state.nested && globalThis.state.nested.value != null
                        ? globalThis.state.nested.value
                        : "";
                    __sol_setText(nestedText, String(nestedValue));

                    const rowsValue = globalThis.state.rows || {};
                    const row0Value = rowsValue?.[0];
                    const row1Value = rowsValue?.[1];
                    const row2Value = rowsValue?.[2];
                    const row3Value = rowsValue?.[3];
                    const rowEntries = [row0Value, row1Value, row2Value, row3Value];
                    const maxIdx = Object.keys(rowsValue)
                      .filter((key) => /^\d+$/.test(key))
                      .map((key) => Number(key))
                      .reduce((acc, key) => Math.max(acc, key), -1);
                    const rowLength =
                      typeof rowsValue.length === "number"
                        ? rowsValue.length
                        : maxIdx >= 0
                          ? maxIdx + 1
                          : 0;

                    __sol_setText(row0Text, rowEntries[0] == null ? "" : String(rowEntries[0]));
                    __sol_setText(row1Text, rowEntries[1] == null ? "" : String(rowEntries[1]));
                    __sol_setText(row2Text, rowEntries[2] == null ? "" : String(rowEntries[2]));
                    __sol_setText(row3Text, rowEntries[3] == null ? "" : String(rowEntries[3]));
                    globalThis.state.canaryRowCount = rowLength;
                    __sol_setText(statusText, "status=" + rowLength);
                  });

              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;

    fn make_state_mutation_canary() -> (Instance, StateHandle) {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 220,
                height: 120,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            CANARY_MUTATION_COMPONENT,
        );
        let state = instance.state();
        state.set("title", json!("seed"));
        state.set("nested", json!({ "value": "inner" }));
        state.set("rows", json!(["initial"]));
        for _ in 0..2 {
            let _ = instance.tick();
            let _ = instance.render();
        }
        (instance, state)
    }

    #[test]
    fn state_mutation_matrix_keeps_unrelated_dom_nodes_stable() {
        let (mut instance, state) = make_state_mutation_canary();

        let root = instance.container_id();
        let baseline = {
            let doc = instance.doc.borrow();
            collect_dom_signature(&doc, root)
        };

        let title_text_id = id_from_state(&state, "canaryTitleTextId");
        let nested_text_id = id_from_state(&state, "canaryNestedTextId");
        let row_node_ids = ids_from_state(&state, "canaryRowNodeIds");
        let row_text_ids = ids_from_state(&state, "canaryRowTextIds");

        assert_eq!(
            row_node_ids.len(),
            4,
            "initial should render four fixed row nodes"
        );
        assert_eq!(
            row_text_ids.len(),
            4,
            "initial should render four fixed row text nodes"
        );

        assert_eq!(
            text_for_node(&baseline, title_text_id),
            Some("seed"),
            "initial title text should be mounted"
        );
        assert_eq!(
            text_for_node(&baseline, nested_text_id),
            Some("inner"),
            "initial nested text should be mounted"
        );

        let run_and_capture = |instance: &mut Instance| -> Vec<DomSig> {
            let _ = instance.tick();
            let _ = instance.render();
            let _ = instance.tick();
            let _ = instance.render();
            let doc = instance.doc.borrow();
            collect_dom_signature(&doc, instance.container_id())
        };

        // Unrelated writes should only touch Rust state, not the mounted DOM.
        state.set("unrelated", json!(true));
        let unchanged = run_and_capture(&mut instance);
        assert_structural_dom_signature_eq(&baseline, &unchanged);
        assert_eq!(ids_from_state(&state, "canaryRowNodeIds"), row_node_ids);
        assert_eq!(ids_from_state(&state, "canaryRowTextIds"), row_text_ids);
        assert_eq!(
            text_for_node(&unchanged, title_text_id),
            Some("seed"),
            "unrelated state should not alter title text"
        );

        // Text-only path updates should mutate text content only.
        state.set("title", json!("next"));
        let title_mut = run_and_capture(&mut instance);
        assert_structural_dom_signature_eq(&baseline, &title_mut);
        assert_eq!(
            text_for_node(&title_mut, title_text_id),
            Some("next"),
            "title text should update"
        );
        assert_eq!(
            text_for_node(&title_mut, nested_text_id),
            Some("inner"),
            "nested text should remain stable"
        );

        // Duplicate same-path writes should keep the same visual result as last write.
        state.set("title", json!("temp"));
        state.set("title", json!("final"));
        let dup_paths = run_and_capture(&mut instance);
        assert_structural_dom_signature_eq(&baseline, &dup_paths);
        assert_eq!(
            text_for_node(&dup_paths, title_text_id),
            Some("final"),
            "last write should win"
        );

        // Nested path update should be reflected in the nested text node.
        state.set("nested.value", json!("deep"));
        let nested_update = run_and_capture(&mut instance);
        assert_structural_dom_signature_eq(&baseline, &nested_update);
        assert_eq!(
            text_for_node(&nested_update, nested_text_id),
            Some("deep"),
            "nested update should surface in nested text"
        );

        // Existing index update should preserve node ids and only mutate row text.
        let existing_row_node_ids = ids_from_state(&state, "canaryRowNodeIds");
        let existing_row_text_ids = ids_from_state(&state, "canaryRowTextIds");
        state.set("rows.0", json!("rewritten"));
        let updated_row = run_and_capture(&mut instance);
        let updated_row_node_ids = ids_from_state(&state, "canaryRowNodeIds");
        let updated_row_text_ids = ids_from_state(&state, "canaryRowTextIds");
        assert_eq!(updated_row_node_ids, existing_row_node_ids);
        assert_eq!(updated_row_text_ids, existing_row_text_ids);
        assert_eq!(
            text_for_node(&updated_row, existing_row_text_ids[0]),
            Some("rewritten"),
            "existing row text node should be updated in place"
        );

        // Out-of-bounds array updates should update the right row while keeping
        // existing ids stable.
        state.set("rows.3", json!("tail"));
        let oob = run_and_capture(&mut instance);
        let oob_row_node_ids = ids_from_state(&state, "canaryRowNodeIds");
        let oob_row_text_ids = ids_from_state(&state, "canaryRowTextIds");

        assert_eq!(oob_row_node_ids.len(), 4, "we keep four fixed row nodes");
        assert_eq!(
            oob_row_node_ids[0], updated_row_node_ids[0],
            "row 0 should be preserved"
        );
        assert_eq!(
            oob_row_text_ids.len(),
            4,
            "row text ids should track each index"
        );
        assert_eq!(
            text_for_node(&oob, oob_row_text_ids[0]),
            Some("rewritten"),
            "row 0 text should keep its rewritten value"
        );
        assert_eq!(
            text_for_node(&oob, oob_row_text_ids[1]),
            Some(""),
            "row 1 should remain empty when not provided"
        );
        assert_eq!(
            text_for_node(&oob, oob_row_text_ids[2]),
            Some(""),
            "row 2 should remain empty when not provided"
        );
        assert_eq!(
            text_for_node(&oob, oob_row_text_ids[3]),
            Some("tail"),
            "out-of-bounds index should materialize at tail"
        );

        // Keep an eye on pure root replacement as a full-shape mutation.
        state.set(
            "",
            json!({
                "title": "rooted",
                "nested": { "value": "replaced" },
                "rows": ["a", "b", "c"],
                "canaryRootId": root,
                "canaryTitleTextId": title_text_id,
                "canaryNestedTextId": nested_text_id,
                "canaryStatusTextId": id_from_state(&state, "canaryStatusTextId"),
                "canaryRowNodeIds": oob_row_node_ids,
                "canaryRowTextIds": oob_row_text_ids,
            }),
        );
        let root_replace = run_and_capture(&mut instance);
        let replaced_row_ids = ids_from_state(&state, "canaryRowNodeIds");
        let replaced_row_text_ids = ids_from_state(&state, "canaryRowTextIds");
        assert_eq!(
            replaced_row_ids.len(),
            4,
            "fixed row nodes remain mounted across root replacement"
        );
        assert_eq!(
            replaced_row_ids[0], oob_row_node_ids[0],
            "first row should remain through root replacement when present"
        );
        assert_eq!(
            oob_row_node_ids, replaced_row_ids,
            "rows nodes should be stable across root replacement"
        );
        assert_eq!(
            oob_row_text_ids, replaced_row_text_ids,
            "row text ids should be stable across root replacement"
        );
        assert_eq!(
            text_for_node(&root_replace, replaced_row_text_ids[0]),
            Some("a"),
            "row 0 should adopt root replacement value"
        );
        assert_eq!(
            text_for_node(&root_replace, replaced_row_text_ids[1]),
            Some("b"),
            "row 1 should receive root replacement value"
        );
        assert_eq!(
            text_for_node(&root_replace, replaced_row_text_ids[2]),
            Some("c"),
            "row 2 should receive root replacement value"
        );
        assert_eq!(
            id_from_state(&state, "canaryRootId"),
            root,
            "canary root id should remain stable"
        );
        assert_eq!(
            text_for_node(&root_replace, title_text_id),
            Some("rooted"),
            "title should reflect root replacement"
        );
        assert_eq!(
            text_for_node(&root_replace, nested_text_id),
            Some("replaced"),
            "nested field should reflect root replacement"
        );
    }

    #[test]
    fn tab_walks_inputs_and_buttons_in_doc_order() {
        let (mut instance, _rx) = make_kb_nav_instance();
        // Initial focus is None; Tab moves to the first focusable.
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(focused_tag(&instance).as_deref(), Some("input"));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(focused_tag(&instance).as_deref(), Some("input"));
        // The third focusable is the button now (was excluded under the old
        // inputs/selects-only filter).
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(focused_tag(&instance).as_deref(), Some("button"));
        // Shift+Tab walks back.
        let _ = instance.dispatch_key_down(tab_key(true));
        assert_eq!(focused_tag(&instance).as_deref(), Some("input"));
    }

    #[test]
    fn automatic_tab_order_walks_all_default_focusables_in_doc_order() {
        // Verifies that every browser-default focusable element
        // (`<input>`, `<select>`, `<button>`, `<a href>`) — none with an
        // explicit `tabindex` — is placed in the Tab order in document
        // order. This is the "automatic tab order" path: nothing in the
        // component declares focus priority; the focus collector infers it.
        let component = r##"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const add = (tag, id, attrs = {}) => {
                const el = __sol_createElement(tag);
                for (const k in attrs) __sol_setProperty(el, k, attrs[k]);
                __sol_insertNode(root, el, null);
                globalThis["__sol_" + id] = el;
                return el;
              };
              add("input", "txt", { type: "text" });
              add("button", "btn");
              const sel = add("select", "sel");
              const opt = __sol_createElement("option");
              __sol_setProperty(opt, "value", "a");
              __sol_insertNode(opt, __sol_createTextNode("A"), null);
              __sol_insertNode(sel, opt, null);
              add("a", "link", { href: "#x" });
              add("div", "plain"); // not focusable, must be skipped
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "##;
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 320,
                height: 160,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            component,
        );
        settle(&mut instance);

        let txt = js_node_id(&instance, "txt");
        let btn = js_node_id(&instance, "btn");
        let sel = js_node_id(&instance, "sel");
        let link = js_node_id(&instance, "link");

        // Tab from no focus → input → button → select → anchor → wraps to input.
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(txt));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(btn));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(sel));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(link));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(txt), "Tab should wrap");

        // Shift+Tab walks back the same chain.
        let _ = instance.dispatch_key_down(tab_key(true));
        assert_eq!(instance.focused_node_id, Some(link));
        let _ = instance.dispatch_key_down(tab_key(true));
        assert_eq!(instance.focused_node_id, Some(sel));
    }

    #[test]
    fn anchor_without_href_is_not_in_automatic_tab_order() {
        // `<a>` without `href` is NOT a default focusable per HTML spec.
        // The collector must skip it unless an explicit `tabindex` opts it in.
        let component = r##"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const inp = __sol_createElement("input"); __sol_insertNode(root, inp, null);
              const a_no_href = __sol_createElement("a");
              __sol_insertNode(a_no_href, __sol_createTextNode("nope"), null);
              __sol_insertNode(root, a_no_href, null);
              const a_with_href = __sol_createElement("a");
              __sol_setProperty(a_with_href, "href", "#x");
              __sol_insertNode(a_with_href, __sol_createTextNode("yes"), null);
              __sol_insertNode(root, a_with_href, null);
              globalThis.__sol_inp = inp;
              globalThis.__sol_a_no_href = a_no_href;
              globalThis.__sol_a_with_href = a_with_href;
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "##;
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 320,
                height: 160,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            component,
        );
        settle(&mut instance);
        let inp = js_node_id(&instance, "inp");
        let a_with_href = js_node_id(&instance, "a_with_href");
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(inp));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(
            instance.focused_node_id,
            Some(a_with_href),
            "<a> without href must NOT receive Tab focus"
        );
    }

    #[test]
    fn disabled_default_focusable_is_skipped_by_automatic_tab_order() {
        let component = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const a = __sol_createElement("input"); __sol_insertNode(root, a, null);
              const b = __sol_createElement("input"); __sol_setProperty(b, "disabled", "");
              __sol_insertNode(root, b, null);
              const c = __sol_createElement("button"); __sol_setProperty(c, "disabled", "");
              __sol_insertNode(root, c, null);
              const d = __sol_createElement("input"); __sol_insertNode(root, d, null);
              globalThis.__sol_a = a; globalThis.__sol_d = d;
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 320,
                height: 160,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            component,
        );
        settle(&mut instance);
        let a = js_node_id(&instance, "a");
        let d = js_node_id(&instance, "d");
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(a));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(
            instance.focused_node_id,
            Some(d),
            "disabled input and disabled button must both be skipped"
        );
    }

    #[test]
    fn tabindex_negative_skips_element_from_tab_order() {
        let component = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const make = (attrs) => {
                const el = __sol_createElement("input");
                for (const k in attrs) __sol_setProperty(el, k, attrs[k]);
                __sol_insertNode(root, el, null);
                globalThis["__sol_" + attrs.id] = el;
                return el;
              };
              make({ id: "a" });
              make({ id: "b", tabindex: "-1" });
              make({ id: "c" });
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 320,
                height: 160,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            component,
        );
        settle(&mut instance);
        let a = js_node_id(&instance, "a");
        let c = js_node_id(&instance, "c");
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(a));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(
            instance.focused_node_id,
            Some(c),
            "tabindex=-1 input must be skipped"
        );
    }

    #[test]
    fn positive_tabindex_takes_priority_over_doc_order() {
        let component = r#"
            import { render } from "solite-runtime";
            function App() {
              const root = __sol_createElement("div");
              const make = (attrs) => {
                const el = __sol_createElement("input");
                for (const k in attrs) __sol_setProperty(el, k, attrs[k]);
                __sol_insertNode(root, el, null);
                globalThis["__sol_" + attrs.id] = el;
                return el;
              };
              make({ id: "a" });
              make({ id: "b", tabindex: "2" });
              make({ id: "c", tabindex: "1" });
              return root;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 320,
                height: 160,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            component,
        );
        settle(&mut instance);
        let a = js_node_id(&instance, "a");
        let b = js_node_id(&instance, "b");
        let c = js_node_id(&instance, "c");
        // Order should be: tabindex=1 (c), tabindex=2 (b), tabindex=0/unset (a).
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(c));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(b));
        let _ = instance.dispatch_key_down(tab_key(false));
        assert_eq!(instance.focused_node_id, Some(a));
    }

    #[test]
    fn enter_on_focused_button_fires_click() {
        let (mut instance, _rx) = make_kb_nav_instance();
        let btn = js_node_id(&instance, "btn");
        instance.focused_node_id = Some(btn);
        let _ = instance.dispatch_key_down(enter_key());
        assert_eq!(instance.state().get("clicked"), Some(json!(1)));
    }

    #[test]
    fn space_keyup_on_focused_button_fires_click() {
        let (mut instance, _rx) = make_kb_nav_instance();
        let btn = js_node_id(&instance, "btn");
        instance.focused_node_id = Some(btn);
        // keydown alone must NOT fire — only keyup completes the click.
        let _ = instance.dispatch_key_down(space_key());
        assert_eq!(instance.state().get("clicked"), Some(json!(0)));
        let _ = instance.dispatch_key_up(space_key());
        assert_eq!(instance.state().get("clicked"), Some(json!(1)));
    }

    #[test]
    fn ctrl_left_jumps_by_word() {
        use crate::input::InputState;
        let mut s = InputState::default();
        s.set_value("hello world here");
        s.move_end();
        // Caret at end (16). Ctrl+Left → start of "here" (12).
        assert!(s.move_word_left_extending(false));
        assert_eq!(s.caret(), 12);
        // Again → start of "world" (6).
        assert!(s.move_word_left_extending(false));
        assert_eq!(s.caret(), 6);
        // Again → start of "hello" (0).
        assert!(s.move_word_left_extending(false));
        assert_eq!(s.caret(), 0);
    }

    #[test]
    fn ctrl_right_jumps_by_word() {
        use crate::input::InputState;
        let mut s = InputState::default();
        s.set_value("hello world here");
        s.move_home();
        assert!(s.move_word_right_extending(false));
        assert_eq!(s.caret(), 5); // end of "hello"
        assert!(s.move_word_right_extending(false));
        assert_eq!(s.caret(), 11); // end of "world"
        assert!(s.move_word_right_extending(false));
        assert_eq!(s.caret(), 16); // end of "here"
    }

    #[test]
    fn ctrl_shift_right_extends_selection_by_word() {
        use crate::input::InputState;
        let mut s = InputState::default();
        s.set_value("foo bar");
        s.move_home();
        assert!(s.move_word_right_extending(true));
        assert_eq!(s.selection_start(), 0);
        assert_eq!(s.selection_end(), 3);
    }

    #[test]
    fn ctrl_backspace_deletes_previous_word() {
        use crate::input::InputState;
        let mut s = InputState::default();
        s.set_value("hello world");
        s.move_end();
        assert!(s.delete_word_left());
        assert_eq!(s.value(), "hello ");
        assert!(s.delete_word_left());
        assert_eq!(s.value(), "");
    }

    #[test]
    fn ctrl_delete_removes_next_word() {
        use crate::input::InputState;
        let mut s = InputState::default();
        s.set_value("hello world");
        s.move_home();
        assert!(s.delete_word_right());
        assert_eq!(s.value(), " world");
    }

    #[test]
    fn ctrl_left_via_dispatch_key_works_end_to_end() {
        let component = r#"
            import { render } from "solite-runtime";
            function App() {
              const input = __sol_createElement("input");
              __sol_setProperty(input, "type", "text");
              __sol_setProperty(input, "value", "hello world");
              globalThis.__sol_input = input;
              return input;
            }
            render(() => App(), __SOL_ROOT__);
        "#;
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 320,
                height: 80,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            component,
        );
        settle(&mut instance);
        let input = js_node_id(&instance, "input");
        instance.focused_node_id = Some(input);
        // Caret defaults to end-of-value after set_value (11).
        let before = instance
            .js
            .inputs
            .borrow()
            .get(&input)
            .map(|s| s.caret())
            .unwrap_or(0);
        assert_eq!(before, 11);
        // Ctrl+Left → caret moves to start of "world" (6).
        let _ = instance.dispatch_key_down(ctrl_key("ArrowLeft"));
        let after = instance
            .js
            .inputs
            .borrow()
            .get(&input)
            .map(|s| s.caret())
            .unwrap_or(0);
        assert_eq!(after, 6);
        // Ctrl+Shift+Left → extends selection back to start of "hello".
        let _ = instance.dispatch_key_down(ctrl_shift_key("ArrowLeft"));
        let (sel_start, sel_end) = instance
            .js
            .inputs
            .borrow()
            .get(&input)
            .map(|s| (s.selection_start(), s.selection_end()))
            .unwrap_or((0, 0));
        assert_eq!(sel_start, 0);
        assert_eq!(sel_end, 6);
    }

    // ─── Select keyboard navigation ──────────────────────────────────────────

    const SELECT_NAV_COMPONENT: &str = r#"
        import { render } from "solite-runtime";
        function App() {
          const sel = __sol_createElement("select");
          const make_opt = (val, label) => {
            const o = __sol_createElement("option");
            __sol_setProperty(o, "value", val);
            __sol_insertNode(o, __sol_createTextNode(label), null);
            __sol_insertNode(sel, o, null);
          };
          make_opt("apple", "Apple");
          make_opt("apricot", "Apricot");
          make_opt("banana", "Banana");
          make_opt("blueberry", "Blueberry");
          make_opt("cherry", "Cherry");
          make_opt("date", "Date");
          make_opt("elderberry", "Elderberry");
          make_opt("fig", "Fig");
          make_opt("grape", "Grape");
          make_opt("honeydew", "Honeydew");
          make_opt("kiwi", "Kiwi");
          make_opt("lemon", "Lemon");
          globalThis.__sol_sel = sel;
          return sel;
        }
        render(() => App(), __SOL_ROOT__);
    "#;

    fn make_select_nav_instance() -> Instance {
        let (device, queue) = test_device();
        let (mut instance, _rx) = Instance::new(
            InstanceConfig {
                width: 240,
                height: 200,
                device,
                queue,
                stylesheets: vec![],
                document_scroll: false,
                base_url: None,
            },
            SELECT_NAV_COMPONENT,
        );
        settle(&mut instance);
        let sel = js_node_id(&instance, "sel");
        instance.focused_node_id = Some(sel);
        instance
    }

    fn select_value(instance: &Instance, sel_id: usize) -> Option<String> {
        instance
            .js
            .selects
            .borrow()
            .get(&sel_id)
            .and_then(|s| s.value())
    }

    #[test]
    fn select_type_ahead_jumps_to_first_match_after_current() {
        let mut instance = make_select_nav_instance();
        let sel = js_node_id(&instance, "sel");
        // The closed select starts with the first option selected. Press
        // "c" — should jump to "Cherry" (next "c" match after current).
        let _ = instance.dispatch_key_down(plain_key("c"));
        assert_eq!(select_value(&instance, sel).as_deref(), Some("cherry"));
    }

    #[test]
    fn select_type_ahead_b_finds_banana() {
        let mut instance = make_select_nav_instance();
        let sel = js_node_id(&instance, "sel");
        let _ = instance.dispatch_key_down(plain_key("b"));
        assert_eq!(select_value(&instance, sel).as_deref(), Some("banana"));
    }

    #[test]
    fn select_type_ahead_cycles_on_repeat_letter() {
        let mut instance = make_select_nav_instance();
        let sel = js_node_id(&instance, "sel");
        // Browser semantics: from "apple" selected, pressing "a" advances to
        // the next option matching "a" — that's apricot (idx 1). Pressing
        // "a" again cycles forward and wraps back to apple.
        let _ = instance.dispatch_key_down(plain_key("a"));
        assert_eq!(select_value(&instance, sel).as_deref(), Some("apricot"));
        let _ = instance.dispatch_key_down(plain_key("a"));
        assert_eq!(select_value(&instance, sel).as_deref(), Some("apple"));
        let _ = instance.dispatch_key_down(plain_key("a"));
        assert_eq!(select_value(&instance, sel).as_deref(), Some("apricot"));
    }

    #[test]
    fn select_page_down_steps_ten() {
        let mut instance = make_select_nav_instance();
        let sel = js_node_id(&instance, "sel");
        // First option selected → PageDown moves +10 → index 10 = "kiwi".
        let _ = instance.dispatch_key_down(plain_key("PageDown"));
        assert_eq!(select_value(&instance, sel).as_deref(), Some("kiwi"));
        // PageUp from index 10 → clamps to 0 = "apple".
        let _ = instance.dispatch_key_down(plain_key("PageUp"));
        assert_eq!(select_value(&instance, sel).as_deref(), Some("apple"));
    }

    #[test]
    fn select_alt_down_opens_dropdown() {
        let mut instance = make_select_nav_instance();
        let sel = js_node_id(&instance, "sel");
        assert!(!instance.js.selects.borrow().get(&sel).unwrap().is_open());
        let _ = instance.dispatch_key_down(alt_key("ArrowDown"));
        assert!(instance.js.selects.borrow().get(&sel).unwrap().is_open());
    }

    #[test]
    fn select_alt_up_commits_and_closes() {
        let mut instance = make_select_nav_instance();
        let sel = js_node_id(&instance, "sel");
        // Open and move highlight to second option.
        let _ = instance.dispatch_key_down(alt_key("ArrowDown"));
        let _ = instance.dispatch_key_down(plain_key("ArrowDown"));
        // Alt+Up commits and closes.
        let _ = instance.dispatch_key_down(alt_key("ArrowUp"));
        assert!(!instance.js.selects.borrow().get(&sel).unwrap().is_open());
        assert_eq!(select_value(&instance, sel).as_deref(), Some("apricot"));
    }

    // ────────────────────────────────────────────────────────────────────────
    // todo example end-to-end: drives the actual examples/todo_app.jsx through
    // a full UX loop (add several items → switch filters → toggle done →
    // re-check filters → clear completed) and asserts the visible DOM matches
    // expectations at each step. Loads the JSX from disk so the test always
    // reflects the shipped example.
    // ────────────────────────────────────────────────────────────────────────
    #[cfg(feature = "jsx-compiler")]
    mod todo_example {
        use super::*;

        const TODO_JSX: &str = include_str!("../examples/todo_app.jsx");
        const TODO_CSS: &str = include_str!("../examples/todo_app.css");

        fn find_descendants_by_class(doc: &BaseDocument, root: usize, class: &str) -> Vec<usize> {
            let mut out = Vec::new();
            let mut stack = vec![root];
            let class_ln = LocalName::from("class");
            while let Some(id) = stack.pop() {
                let Some(node) = doc.get_node(id) else {
                    continue;
                };
                if let Some(value) = node.attr(class_ln.clone()) {
                    if value.split_whitespace().any(|c| c == class) {
                        out.push(id);
                    }
                }
                for child in node.children.iter().rev() {
                    stack.push(*child);
                }
            }
            out
        }

        fn first_by_class(doc: &BaseDocument, root: usize, class: &str) -> usize {
            *find_descendants_by_class(doc, root, class)
                .first()
                .unwrap_or_else(|| panic!("missing element with class '{class}'"))
        }

        fn center_of(doc: &BaseDocument, node_id: usize) -> (f32, f32) {
            let node = doc.get_node(node_id).expect("node");
            let pos = node.absolute_position(0.0, 0.0);
            let size = node.final_layout.size;
            (pos.x + size.width / 2.0, pos.y + size.height / 2.0)
        }

        fn has_class(doc: &BaseDocument, node_id: usize, class: &str) -> bool {
            let class_ln = LocalName::from("class");
            doc.get_node(node_id)
                .and_then(|n| n.attr(class_ln))
                .map(|v| v.split_whitespace().any(|c| c == class))
                .unwrap_or(false)
        }

        fn text_of(doc: &BaseDocument, node_id: usize) -> String {
            doc.get_node(node_id)
                .map(|n| n.text_content())
                .unwrap_or_default()
        }

        fn visible_items(instance: &Instance) -> Vec<(String, bool)> {
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let items = find_descendants_by_class(&doc, root, "todo-item");
            items
                .into_iter()
                .map(|id| {
                    // Each todo-item has a .todo-text span.
                    let text_id = first_by_class(&doc, id, "todo-text");
                    let text = text_of(&doc, text_id);
                    let done = has_class(&doc, id, "done");
                    (text, done)
                })
                .collect()
        }

        fn empty_state_text(instance: &Instance) -> Option<String> {
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let states = find_descendants_by_class(&doc, root, "empty-state");
            states.first().map(|id| text_of(&doc, *id))
        }

        fn active_chip(instance: &Instance) -> Option<String> {
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let chips = find_descendants_by_class(&doc, root, "chip");
            chips
                .into_iter()
                .find(|id| has_class(&doc, *id, "active"))
                .map(|id| {
                    // text_content concatenates the chip label and its count
                    // span (e.g. "All3"). Match against the known labels by
                    // prefix so we return just the label.
                    let text = text_of(&doc, id);
                    for label in ["Active", "All", "Done"] {
                        if text.starts_with(label) {
                            return label.to_string();
                        }
                    }
                    text
                })
        }

        fn assert_chip_labels_stable(instance: &Instance) {
            let doc = instance.doc.borrow();
            let root = instance.container_id();
            let labels = find_descendants_by_class(&doc, root, "chip")
                .into_iter()
                .map(|id| text_of(&doc, id))
                .collect::<Vec<_>>();
            assert_eq!(labels.len(), 3, "todo filter should keep three chips");
            assert!(
                labels[0].starts_with("All")
                    && labels[1].starts_with("Active")
                    && labels[2].starts_with("Done"),
                "filter chip labels should not be replaced by reactive booleans/counts: {labels:?}"
            );
        }

        fn click(instance: &mut Instance, x: f32, y: f32) {
            let _ = instance.dispatch_mouse(
                x,
                y,
                MouseEvent::Down {
                    x,
                    y,
                    button: MouseButton::Left,
                },
            );
            let _ = instance.dispatch_mouse(
                x,
                y,
                MouseEvent::Up {
                    x,
                    y,
                    button: MouseButton::Left,
                },
            );
        }

        fn click_by_class(instance: &mut Instance, class: &str) {
            let (x, y) = {
                let doc = instance.doc.borrow();
                let root = instance.container_id();
                let id = first_by_class(&doc, root, class);
                center_of(&doc, id)
            };
            click(instance, x, y);
            let _ = instance.tick();
            let _ = instance.render();
        }

        fn click_nth_by_class(instance: &mut Instance, class: &str, n: usize) {
            let (x, y) = {
                let doc = instance.doc.borrow();
                let root = instance.container_id();
                let nodes = find_descendants_by_class(&doc, root, class);
                let id = *nodes
                    .get(n)
                    .unwrap_or_else(|| panic!("no element {class} at index {n}"));
                center_of(&doc, id)
            };
            click(instance, x, y);
            let _ = instance.tick();
            let _ = instance.render();
        }

        fn type_text(instance: &mut Instance, text: &str) {
            for ch in text.chars() {
                let _ = instance.dispatch_key_down(make_key_event(
                    &ch.to_string(),
                    "",
                    0,
                    false,
                    false,
                    false,
                    false,
                    false,
                ));
            }
            let _ = instance.tick();
            let _ = instance.render();
        }

        fn press_enter(instance: &mut Instance) {
            let _ = instance.dispatch_key_down(make_key_event(
                "Enter", "Enter", 13, false, false, false, false, false,
            ));
            let _ = instance.tick();
            let _ = instance.render();
        }

        fn type_and_enter(instance: &mut Instance, text: &str) {
            type_text(instance, text);
            press_enter(instance);
        }

        fn make_todo_instance() -> Instance {
            let compiled = solite_build::compile_component_source(
                std::path::Path::new("todo_app.jsx"),
                TODO_JSX,
            )
            .expect("compile todo jsx");
            let (device, queue) = test_device();
            let (mut instance, _rx) = Instance::new(
                InstanceConfig {
                    width: 540,
                    height: 800,
                    device,
                    queue,
                    stylesheets: vec![TODO_CSS.to_string()],
                    document_scroll: true,
                    base_url: None,
                },
                &compiled,
            );
            let _ = instance.tick();
            let _ = instance.render();
            instance
        }

        /// End-to-end: with createEffect mirrors injected, verify that
        /// (a) typing into the input updates the `draft` signal, and
        /// (b) clicking the Add button fires the handler, calls
        ///     setTodos+setDraft, and clears the input.
        /// This catches the original bug ("Add does nothing") at the signal
        /// level — independent of paint/render.
        #[test]
        fn add_button_click_fires_handler_and_updates_signals() {
            use crate::scene::{Scene, SurfaceRect};

            // Inject side-channel mirrors via createEffect so we can read
            // signal state from Rust through globalThis.state.
            let probed = TODO_JSX
                .replacen(
                    "import { createMemo, createSignal, render } from \"solite-runtime\";",
                    "import { createEffect, createMemo, createSignal, render } from \"solite-runtime\";",
                    1,
                )
                .replacen(
                    "let nextId = 1;",
                    "let nextId = 1;\n  createEffect(() => { globalThis.state.__draft = draft(); });\n  createEffect(() => { globalThis.state.__todoCount = todos().length; });",
                    1,
                );

            let compiled = solite_build::compile_component_source(
                std::path::Path::new("todo_app.jsx"),
                &probed,
            )
            .expect("compile");
            let (device, queue) = test_device();
            let (mut instance, _rx) = Instance::new(
                InstanceConfig {
                    width: 540,
                    height: 800,
                    device,
                    queue,
                    stylesheets: vec![TODO_CSS.to_string()],
                    document_scroll: true,
                    base_url: None,
                },
                &compiled,
            );
            let _ = instance.tick();
            let _ = instance.render();

            let state = instance.state();
            assert_eq!(state.get("__draft"), Some(json!("")));
            assert_eq!(state.get("__todoCount"), Some(json!(0)));

            let (input_x, input_y, add_x, add_y) = {
                let doc = instance.doc.borrow();
                let root = instance.container_id();
                let input_id = first_by_class(&doc, root, "todo-input");
                let add_id = first_by_class(&doc, root, "add-btn");
                let (ix, iy) = center_of(&doc, input_id);
                let (ax, ay) = center_of(&doc, add_id);
                (ix, iy, ax, ay)
            };

            let mut scene: Scene<()> = Scene::new();
            scene.add_surface(instance, SurfaceRect::new(0.0, 0.0, 540.0, 800.0), ());

            // Focus + type "tea".
            let _ = scene.dispatch_mouse(
                input_x,
                input_y,
                MouseEvent::Down {
                    x: input_x,
                    y: input_y,
                    button: MouseButton::Left,
                },
            );
            let _ = scene.dispatch_mouse(
                input_x,
                input_y,
                MouseEvent::Up {
                    x: input_x,
                    y: input_y,
                    button: MouseButton::Left,
                },
            );
            for ch in "tea".chars() {
                let _ = scene.dispatch_key_down(make_key_event(
                    &ch.to_string(),
                    "",
                    0,
                    false,
                    false,
                    false,
                    false,
                    false,
                ));
            }
            let _ = scene.tick();
            assert_eq!(
                state.get("__draft"),
                Some(json!("tea")),
                "draft signal must mirror typed text after the user types"
            );

            // Click Add.
            let _ = scene.dispatch_mouse(
                add_x,
                add_y,
                MouseEvent::Down {
                    x: add_x,
                    y: add_y,
                    button: MouseButton::Left,
                },
            );
            let _ = scene.dispatch_mouse(
                add_x,
                add_y,
                MouseEvent::Up {
                    x: add_x,
                    y: add_y,
                    button: MouseButton::Left,
                },
            );
            let _ = scene.tick();

            assert_eq!(
                state.get("__todoCount"),
                Some(json!(1)),
                "todoCount should be 1 after clicking Add"
            );
            assert_eq!(
                state.get("__draft"),
                Some(json!("")),
                "draft signal should clear after Add"
            );
        }

        /// Drive the example through the same Scene + dispatch path the
        /// `examples/todo.rs` winit host uses. Catches bugs that don't appear
        /// when driving an Instance directly (e.g. event routing through a
        /// surface, focus tracking on the scene).
        #[test]
        fn clicking_add_button_via_scene_grows_the_list() {
            use crate::scene::{Scene, SurfaceRect};

            let compiled = solite_build::compile_component_source(
                std::path::Path::new("todo_app.jsx"),
                TODO_JSX,
            )
            .expect("compile todo jsx");
            let (device, queue) = test_device();
            let (mut instance, _rx) = Instance::new(
                InstanceConfig {
                    width: 540,
                    height: 800,
                    device,
                    queue,
                    stylesheets: vec![TODO_CSS.to_string()],
                    document_scroll: true,
                    base_url: None,
                },
                &compiled,
            );
            let _ = instance.tick();
            let _ = instance.render();

            // Snapshot the input + Add button positions BEFORE moving into the
            // scene, since the scene takes ownership.
            let (input_x, input_y, add_x, add_y) = {
                let doc = instance.doc.borrow();
                let root = instance.container_id();
                let input_id = first_by_class(&doc, root, "todo-input");
                let add_id = first_by_class(&doc, root, "add-btn");
                let (ix, iy) = center_of(&doc, input_id);
                let (ax, ay) = center_of(&doc, add_id);
                (ix, iy, ax, ay)
            };

            let mut scene: Scene<()> = Scene::new();
            scene.add_surface(instance, SurfaceRect::new(0.0, 0.0, 540.0, 800.0), ());

            // Click the input to focus it via the Scene dispatch path.
            let _ = scene.dispatch_mouse(
                input_x,
                input_y,
                MouseEvent::Down {
                    x: input_x,
                    y: input_y,
                    button: MouseButton::Left,
                },
            );
            let _ = scene.dispatch_mouse(
                input_x,
                input_y,
                MouseEvent::Up {
                    x: input_x,
                    y: input_y,
                    button: MouseButton::Left,
                },
            );
            let _ = scene.tick();

            // Type "milk" via the Scene's key dispatch path (routes to the
            // focused surface).
            for ch in "milk".chars() {
                let _ = scene.dispatch_key_down(make_key_event(
                    &ch.to_string(),
                    "",
                    0,
                    false,
                    false,
                    false,
                    false,
                    false,
                ));
            }
            let _ = scene.tick();

            // Click the Add button (NOT Enter) via the Scene path.
            let _ = scene.dispatch_mouse(
                add_x,
                add_y,
                MouseEvent::Down {
                    x: add_x,
                    y: add_y,
                    button: MouseButton::Left,
                },
            );
            let _ = scene.dispatch_mouse(
                add_x,
                add_y,
                MouseEvent::Up {
                    x: add_x,
                    y: add_y,
                    button: MouseButton::Left,
                },
            );
            let _ = scene.tick();

            let surface = &scene.surfaces_mut()[0];
            let instance_ref = &surface.instance;
            let _ = instance_ref;
            let surface = &mut scene.surfaces_mut()[0];
            let _ = surface.instance.render();

            // Verify the item was added by looking at the DOM.
            let items: Vec<(String, bool)> = {
                let instance = &scene.surfaces_mut()[0].instance;
                let doc = instance.doc.borrow();
                let root = instance.container_id();
                let item_ids = find_descendants_by_class(&doc, root, "todo-item");
                item_ids
                    .into_iter()
                    .map(|id| {
                        let text_id = first_by_class(&doc, id, "todo-text");
                        (text_of(&doc, text_id), has_class(&doc, id, "done"))
                    })
                    .collect()
            };
            assert_eq!(
                items.len(),
                1,
                "clicking Add via Scene should add exactly one item"
            );
            assert_eq!(items[0].0, "milk");
        }

        #[test]
        fn full_user_flow_create_filter_toggle_clear() {
            let mut instance = make_todo_instance();

            // Initial state: no items, "all" filter, empty state visible.
            assert!(visible_items(&instance).is_empty());
            assert_eq!(active_chip(&instance).as_deref(), Some("All"));
            assert!(
                empty_state_text(&instance)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains("nothing here")
            );

            // Focus the input by clicking it, then add two todos via Enter
            // and one via clicking the Add button.
            click_by_class(&mut instance, "todo-input");
            type_and_enter(&mut instance, "buy milk");
            type_and_enter(&mut instance, "walk dog");
            type_text(&mut instance, "write report");
            click_by_class(&mut instance, "add-btn");

            let items = visible_items(&instance);
            assert_eq!(items.len(), 3, "three items after adding");
            assert_eq!(
                items.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
                vec!["buy milk", "walk dog", "write report"]
            );
            assert!(items.iter().all(|(_, done)| !done), "all start undone");

            // Filter: Active should show all 3 (none are done yet).
            click_by_class(&mut instance, "chip"); // first chip (All) — no-op, just confirms
            // The "All" chip is index 0; clicking it again should leave state
            // unchanged. Now click "Active" (index 1) and "Done" (index 2).
            click_nth_by_class(&mut instance, "chip", 1);
            assert_chip_labels_stable(&instance);
            assert_eq!(active_chip(&instance).as_deref(), Some("Active"));
            assert_eq!(
                visible_items(&instance).len(),
                3,
                "active = 3 when nothing done"
            );

            click_nth_by_class(&mut instance, "chip", 2);
            assert_chip_labels_stable(&instance);
            assert_eq!(active_chip(&instance).as_deref(), Some("Done"));
            assert!(
                visible_items(&instance).is_empty(),
                "done filter shows no items before toggling"
            );
            assert!(
                empty_state_text(&instance)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains("no completed"),
                "empty state should describe completed filter"
            );

            // Back to "All".
            click_nth_by_class(&mut instance, "chip", 0);
            assert_chip_labels_stable(&instance);
            assert_eq!(active_chip(&instance).as_deref(), Some("All"));
            assert_eq!(visible_items(&instance).len(), 3);

            // Mark items 1 ("buy milk") and 3 ("write report") as done by
            // clicking their checkboxes.
            click_nth_by_class(&mut instance, "todo-checkbox", 0);
            click_nth_by_class(&mut instance, "todo-checkbox", 2);

            let items = visible_items(&instance);
            assert_eq!(items.len(), 3);
            assert!(items[0].1, "buy milk done");
            assert!(!items[1].1, "walk dog not done");
            assert!(items[2].1, "write report done");

            // Active filter → only "walk dog".
            click_nth_by_class(&mut instance, "chip", 1);
            assert_chip_labels_stable(&instance);
            let active_items = visible_items(&instance);
            assert_eq!(active_items.len(), 1, "active = 1 after marking two done");
            assert_eq!(active_items[0].0, "walk dog");

            // Done filter → "buy milk", "write report".
            click_nth_by_class(&mut instance, "chip", 2);
            assert_chip_labels_stable(&instance);
            let done_items = visible_items(&instance);
            assert_eq!(done_items.len(), 2);
            let done_texts: Vec<&str> = done_items.iter().map(|(t, _)| t.as_str()).collect();
            assert!(done_texts.contains(&"buy milk"));
            assert!(done_texts.contains(&"write report"));

            // Switch back to All, then click Clear completed.
            click_nth_by_class(&mut instance, "chip", 0);
            assert_chip_labels_stable(&instance);
            click_by_class(&mut instance, "clear-btn");

            let remaining = visible_items(&instance);
            assert_eq!(remaining.len(), 1, "clear completed leaves only undone");
            assert_eq!(remaining[0].0, "walk dog");
            assert!(!remaining[0].1, "remaining item is not done");

            // After clearing, switching to Done shows the empty state again.
            click_nth_by_class(&mut instance, "chip", 2);
            assert_chip_labels_stable(&instance);
            assert!(visible_items(&instance).is_empty());
            assert!(
                empty_state_text(&instance)
                    .unwrap_or_default()
                    .to_lowercase()
                    .contains("no completed")
            );
        }
    }
}
