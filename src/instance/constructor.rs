use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use blitz_dom::{BaseDocument, DocumentConfig, LocalName, QualName, ns};
use blitz_traits::shell::{ColorScheme, ShellProvider, Viewport};
use notify::{self, RecursiveMode, Watcher};
use serde_json::json;
use tokio::sync::mpsc;
use url::Url;

use crate::fonts::{self, FontFormat};
use crate::js::{JsContext, VirtualSourceFile};
use crate::net::{self, SoliteNetProvider};
use crate::renderer::Painter;

use super::{
    Event, FileWatch, Instance, InstanceConfig, InstanceError, RegisterFontError,
    RegisterImageError, StylesheetId,
};
#[cfg(feature = "jsx-compiler")]
use solite_build as compiler;

#[cfg(not(feature = "jsx-compiler"))]
fn is_jsx_or_ts_module(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "jsx" | "tsx" | "ts"))
}

fn parse_base_url(base_url: &str) -> Result<Url, InstanceError> {
    Url::parse(base_url).map_err(|err| InstanceError::BaseUrl {
        value: base_url.to_string(),
        error: err.to_string(),
    })
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
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), InstanceError> {
        let InstanceConfig {
            width,
            height,
            device,
            queue,
            stylesheets: initial_stylesheets,
            document_scroll,
            base_url: base_url_config,
            initial_state,
            registered_resources,
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
        for (url, bytes) in registered_resources {
            net_provider.register(url, bytes);
        }
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
        let initial_state = initial_state.unwrap_or_else(|| json!({}));
        let state = super::StateHandle::new_with_wake(initial_state.clone(), Arc::clone(&wake));

        // --- Events ---
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

        // --- JS context ---
        let js = JsContext::new(Rc::clone(&doc), Rc::clone(&base_url))?;
        js.mount(
            component_source,
            container_id,
            &state,
            Some(initial_state),
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
            spinners: Vec::new(),
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
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), InstanceError> {
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
    ) -> (Self, mpsc::UnboundedReceiver<Event>) {
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
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), InstanceError> {
        let component_path = crate::js::resolve_component_entrypoint(component_path);

        let component_source = std::fs::read_to_string(&component_path)?;
        #[cfg(feature = "jsx-compiler")]
        let component_source =
            compiler::compile_component_source(&component_path, &component_source)?;
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
            initial_state,
            registered_resources,
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
        for (url, bytes) in registered_resources {
            net_provider.register(url, bytes);
        }
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
        let initial_state = initial_state.unwrap_or_else(|| json!({}));
        let state = super::StateHandle::new_with_wake(initial_state.clone(), Arc::clone(&wake));

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
            Some(initial_state),
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
            spinners: Vec::new(),
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
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), InstanceError> {
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
    ) -> (Self, mpsc::UnboundedReceiver<Event>) {
        Self::new_from_file_inner(config, component_path).expect("instance initialization failed")
    }

    /// Create a new instance from a source root directory.
    #[cfg(not(test))]
    pub fn new_from_dir(
        config: InstanceConfig,
        source_dir: &Path,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), InstanceError> {
        Self::new_from_file_inner(config, source_dir)
    }

    /// Create a new instance from a source root directory.
    #[cfg(test)]
    pub fn new_from_dir(
        config: InstanceConfig,
        source_dir: &Path,
    ) -> (Self, mpsc::UnboundedReceiver<Event>) {
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
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), InstanceError> {
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
                    return Err(InstanceError::UnsupportedJsxModule {
                        path: component_path,
                    });
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
            initial_state,
            registered_resources,
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
        for (url, bytes) in registered_resources {
            net_provider.register(url, bytes);
        }
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
        let initial_state = initial_state.unwrap_or_else(|| json!({}));
        let state = super::StateHandle::new_with_wake(initial_state.clone(), Arc::clone(&wake));

        // --- Events ---
        let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();

        // --- JS context ---
        let js = JsContext::new_with_virtual_files(Rc::clone(&doc), files, Rc::clone(&base_url))?;
        js.mount_with_module_path(
            &component_path,
            &component_source,
            container_id,
            &state,
            Some(initial_state),
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
            spinners: Vec::new(),
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
    ) -> Result<(Self, mpsc::UnboundedReceiver<Event>), InstanceError> {
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
    ) -> (Self, mpsc::UnboundedReceiver<Event>) {
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

    /// Register raw image bytes under a URL the document's net provider will
    /// serve synchronously.
    ///
    /// After calling this, any `<img src="…">` (or CSS `url(…)`) that
    /// references `url` will be served from memory rather than hitting the
    /// filesystem or network. Use any URL scheme that does not conflict with
    /// `file://`, `data:`, `http://`, or `https://` — e.g.
    /// `"solite-image://my-icon"`.
    pub fn register_image_bytes(&self, url: impl Into<String>, bytes: Vec<u8>) {
        self.net_provider.register(url, bytes);
    }

    /// Register an image from a file on disk under `url`.
    ///
    /// Reads the file once and hands the bytes to [`Self::register_image_bytes`].
    /// After this call succeeds the file is no longer required at runtime.
    pub fn register_image_from_path(
        &self,
        url: impl Into<String>,
        path: &Path,
    ) -> Result<(), RegisterImageError> {
        let bytes = std::fs::read(path).map_err(RegisterImageError::Io)?;
        self.register_image_bytes(url, bytes);
        Ok(())
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

fn create_container_element(doc: &mut BaseDocument) -> usize {
    doc.mutate().create_element(
        QualName::new(None, ns!(html), LocalName::from("div")),
        vec![],
    )
}

fn font_face_style_qual() -> QualName {
    QualName::new(None, ns!(html), LocalName::from("style"))
}
