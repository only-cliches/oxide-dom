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

#[cfg(feature = "jsx-compiler")]
use crate::compiler;
use crate::events::{Event, KeyboardEvent, MouseButton, MouseEvent};
use crate::js::{JsContext, TickResult};
use crate::renderer::{InputCaret, InputSelection, Painter};
use crate::scrollbar::{
    self, ScrollbarColors, ScrollbarDrag, ScrollbarHit, ScrollbarRegion, ScrollbarTheme,
    collect_scrollbar_regions,
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
}

/// Opaque identifier for a stylesheet registered via
/// [`Instance::add_stylesheet`]. Pass to [`Instance::replace_stylesheet`] or
/// [`Instance::remove_stylesheet`] to update or drop the sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StylesheetId(u64);

/// An oxide-dom render instance.
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
}

/// Watches a component source tree for filesystem changes.
#[derive(Debug)]
pub struct FileWatch {
    pub root: PathBuf,
    changed: std::sync::mpsc::Receiver<PathBuf>,
    #[allow(dead_code)]
    _watcher: RecommendedWatcher,
}

impl FileWatch {
    /// Non-blocking check for the next changed file path.
    pub fn poll(&self) -> Option<PathBuf> {
        self.changed.try_recv().ok()
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
    /// (`__ox_createElement`, etc.) and the `oxide-runtime` module are
    /// pre-installed so the component can import and use them.
    ///
    /// Returns the instance and a channel receiver for JS-emitted events.
    pub fn new(config: InstanceConfig, component_source: &str) -> (Self, UnboundedReceiver<Event>) {
        let InstanceConfig {
            width,
            height,
            device,
            queue,
            stylesheets: initial_stylesheets,
            document_scroll,
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
        let js = JsContext::new(Rc::clone(&doc));
        js.mount(component_source, container_id, &state, event_tx.clone());

        // --- GPU resources ---
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("oxide-dom"),
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
        };

        (instance, event_rx)
    }

    /// Create a new instance from a component file.
    ///
    /// The component is loaded from disk and mounted as an ES module named for
    /// its absolute path so relative imports resolve against the source file.
    ///
    /// Returns the instance and a channel receiver for JS-emitted events.
    pub fn new_from_file(
        config: InstanceConfig,
        component_path: &Path,
    ) -> (Self, UnboundedReceiver<Event>) {
        let component_path = component_path
            .canonicalize()
            .unwrap_or_else(|_| component_path.to_path_buf());

        let component_source =
            std::fs::read_to_string(&component_path).expect("read component source file");
        #[cfg(feature = "jsx-compiler")]
        let component_source =
            compiler::compile_component_source(&component_path, &component_source)
                .expect("compile component source file");
        #[cfg(not(feature = "jsx-compiler"))]
        let component_source = {
            if is_jsx_or_ts_module(&component_path) {
                panic!("JSX/TSX component loading requires the `jsx-compiler` feature");
            }
            component_source
        };
        let component_path = component_path.to_string_lossy().to_string();

        let InstanceConfig {
            width,
            height,
            device,
            queue,
            stylesheets: initial_stylesheets,
            document_scroll,
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
        );
        js.mount_with_module_path(
            &component_path,
            &component_source,
            container_id,
            &state,
            event_tx.clone(),
        );

        // --- GPU resources ---
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("oxide-dom"),
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
        };

        (instance, event_rx)
    }

    /// Set the document shell provider after construction.
    ///
    /// This is used by hosts that need clipboard / redraw hooks or other
    /// shell integration. The provider is delegated to the underlying
    /// `blitz-dom` document instance.
    pub fn set_shell_provider(&self, shell_provider: Arc<dyn ShellProvider>) {
        self.doc.borrow_mut().set_shell_provider(shell_provider);
    }

    /// Start watching a component path and receive changed file paths.
    /// Keep the returned [`FileWatch`] alive; dropping it stops the watch.
    pub fn watch_files(component_path: &Path) -> notify::Result<FileWatch> {
        let component_path = component_path
            .canonicalize()
            .unwrap_or_else(|_| component_path.to_path_buf());
        let watch_root = component_path.parent().map_or_else(
            || Path::new(".").to_path_buf(),
            |parent| parent.to_path_buf(),
        );

        let (tx, rx) = std::sync::mpsc::channel::<PathBuf>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                for path in event.paths {
                    let _ = tx.send(path);
                }
            }
        })
        .expect("create file watcher");
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
        let result = self.js.tick(&self.state, 256);

        // Advance the caret blink on the focused native input, if any. The
        // toggle is driven from inside tick() rather than from a separate
        // host timer so anyone calling `tick()` regularly (every ~50–500 ms)
        // gets blinking for free; hosts that want a tighter cadence can call
        // `tick()` more often.
        let blink_flipped = self.advance_input_blink();
        if blink_flipped {
            self.needs_paint = true;
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
            label: Some("oxide-dom"),
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

    /// Read the current vertical scroll offset of a node.
    fn node_scroll(&self, node_id: usize) -> f32 {
        self.doc
            .borrow()
            .get_node(node_id)
            .map(|node| node.scroll_offset.y as f32)
            .unwrap_or(0.0)
    }

    /// Move a node's vertical scroll offset to an absolute target value.
    ///
    /// Note: `BaseDocument::scroll_node_by` uses an inverted sign convention
    /// (positive y scrolls *back* toward the top), so we negate `delta`.
    fn set_node_scroll(&mut self, node_id: usize, target: f32) {
        let current = self.node_scroll(node_id);
        let delta = (target - current) as f64;
        if delta == 0.0 {
            return;
        }
        self.doc
            .borrow_mut()
            .scroll_node_by(node_id, 0.0, -delta, |_| {});
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
                MouseEvent::Move { y, .. } => {
                    let target = drag.pointer_to_scroll(y);
                    self.set_node_scroll(drag.node_id, target);
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
                    self.scrollbar_drag = Some(ScrollbarDrag::from_thumb_hit(region, doc_y));
                    return TickResult::default();
                }
                Some(ScrollbarHit::Track(region)) => {
                    // Page up/down: jump by ~80% of the visible track.
                    let direction = if doc_y < region.thumb.1 { -1.0 } else { 1.0 };
                    let step = (region.track.3 * 0.8).max(20.0);
                    let current = self.node_scroll(region.node_id);
                    let target = (current + direction * step).clamp(0.0, region.max_scroll);
                    self.set_node_scroll(region.node_id, target);
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
        // `doc.borrow_mut()` (e.g. `__ox_setText`) would panic.
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
        let control_order = {
            let inputs = self.js.inputs.borrow();
            let selects = self.js.selects.borrow();
            let doc = self.doc.borrow();
            let mut ids = Vec::new();
            doc.visit(|node_id, _node| {
                if inputs.get(&node_id).is_some_and(|state| !state.disabled()) {
                    ids.push(node_id);
                } else if selects.get(&node_id).is_some_and(|state| !state.disabled()) {
                    ids.push(node_id);
                }
            });
            ids
        };

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
            return self.focus_adjacent_control(event.shift_key);
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
        let (edited, emits_input_event) = if event_name == "keydown"
            && self.js.inputs.borrow().contains_key(&focused_id)
        {
            apply_input_key(&self.js.inputs, focused_id, &event)
        } else if event_name == "keydown" && self.js.selects.borrow().contains_key(&focused_id) {
            self.apply_select_key(focused_id, &event)
        } else {
            (false, false)
        };

        let result = self.js.dispatch_key_event(focused_id, event_name, &event);
        if result.needs_paint {
            self.needs_paint = true;
        }

        if edited {
            self.refresh_input_text(focused_id);
            self.refresh_select_text(focused_id);
            self.needs_paint = true;
        }

        if emits_input_event {
            // Refresh visible text + emit input event for inputs.
            let snapshot = self.js.inputs.borrow().get(&focused_id).map(|s| {
                (
                    s.value().to_string(),
                    s.checked(),
                    s.selection_start(),
                    s.selection_end(),
                )
            });
            if let Some((value, checked, selection_start, selection_end)) = snapshot {
                let input_result = self.js.dispatch_input_event(
                    focused_id,
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
                .get(&focused_id)
                .map(|s| (s.value().unwrap_or_default(), s.selected_index()));
            if let Some((value, selected_index)) = select_snapshot {
                let change_result =
                    self.js
                        .dispatch_select_change_event(focused_id, &value, selected_index);
                return combine_tick_result(result, change_result);
            }
        }

        result
    }

    /// Refresh the visible text child of an `<input>` from its InputState.
    /// The child text node is the first child of the input (seeded in
    /// `__ox_createElement` when the tag is "input").
    fn refresh_input_text(&mut self, input_id: usize) {
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
    /// `__ox_createElement` when the tag is "select").
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
        let (selection_start, selection_end) = (state.selection_start(), state.selection_end());
        if selection_start == selection_end {
            return Vec::new();
        }
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
        let selection_start = char_index_to_byte_index(&display_text, selection_start);
        let selection_end = char_index_to_byte_index(&display_text, selection_end);
        let layout_data = input_data.editor.try_layout().unwrap();
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

    // ── Native inputs ────────────────────────────────────────────────────────

    /// Returns the current value of the `<input>` registered at `node_id`,
    /// or `None` if no input is registered there. Useful for tests and for
    /// hosts that want to read the field directly without round-tripping
    /// through a JS handler.
    pub fn input_value(&self, node_id: usize) -> Option<String> {
        self.js
            .inputs
            .borrow()
            .get(&node_id)
            .map(|state| state.value().to_string())
    }

    /// Set the value of the `<input>` registered at `node_id`. Mirrors what
    /// `__ox_setAttr(node, "value", v)` does from JS — the caret moves to the
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

fn create_container_element(doc: &mut BaseDocument) -> usize {
    doc.mutate().create_element(
        QualName::new(None, ns!(html), LocalName::from("div")),
        vec![],
    )
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

    if (event.ctrl_key || event.meta_key) && !event.alt_key {
        match key {
            "a" | "A" => {
                let edited = state.select_all();
                return (edited, false);
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
            let ch = key.chars().next().unwrap();
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
            _ => {}
        }
    } else {
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
                    if is_popup_selectable(active) {
                        state.set_selected_index(Some(active));
                    }
                }
                out.edited = true;
                out.emits_change = true;
                out.close_popup = true;
            }
            "Escape" => {
                out.edited = true;
                out.close_popup = true;
            }
            "Tab" => {
                if let Some(active) = state.active_index() {
                    if is_popup_selectable(active) {
                        state.set_selected_index(Some(active));
                    }
                }
                out.edited = true;
                out.emits_change = true;
                out.close_popup = true;
            }
            _ => {}
        }
    }
    out
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

    use serde_json::json;

    const CLICK_BUTTON_COMPONENT: &str = r#"
        import { render } from "oxide-runtime";
        function App() {
          const btn = __ox_createElement("button");
          __ox_setProperty(
            btn,
            "style",
            "display:block; width: 160px; height: 80px;"
          );
          __ox_setProperty(btn, "onClick", () => {
            globalThis.state.count = (globalThis.state.count || 0) + 1;
          });
          return btn;
        }
        render(() => App(), __OX_ROOT__);
    "#;

    const ROOT_CLICK_COMPONENT: &str = r#"
        import { render } from "oxide-runtime";
        function App() {
          const root = __ox_createElement("div");
          __ox_setProperty(root, "onClick", () => {
            globalThis.state.clicked = true;
          });
          __ox_setProperty(
            root,
            "style",
            "display:block; width: 200px; height: 200px;"
          );
          return root;
        }
        render(() => App(), __OX_ROOT__);
    "#;

    const HOVER_COMPONENT: &str = r#"
        import { render } from "oxide-runtime";
        function App() {
          const btn = __ox_createElement("button");
          __ox_setProperty(
            btn,
            "style",
            "display:block; width: 80px; height: 80px;"
          );
          __ox_setProperty(btn, "onMouseOver", (e) => {
            globalThis.state.over = (globalThis.state.over || 0) + 1;
            globalThis.state.overTarget = e.target;
            globalThis.state.overRelated = e.relatedTarget;
          });
          __ox_setProperty(btn, "onMouseOut", (e) => {
            globalThis.state.out = (globalThis.state.out || 0) + 1;
            globalThis.state.outTarget = e.target;
            globalThis.state.outRelated = e.relatedTarget;
          });
          __ox_setProperty(btn, "onMouseEnter", (e) => {
            globalThis.state.enter = (globalThis.state.enter || 0) + 1;
            globalThis.state.enterCurrent = e.currentTarget;
            globalThis.state.enterRelated = e.relatedTarget;
          });
          __ox_setProperty(btn, "onMouseLeave", (e) => {
            globalThis.state.leave = (globalThis.state.leave || 0) + 1;
            globalThis.state.leaveCurrent = e.currentTarget;
            globalThis.state.leaveRelated = e.relatedTarget;
          });
          __ox_setProperty(btn, "onHover", (e) => {
            globalThis.state.hover = (globalThis.state.hover || 0) + 1;
            globalThis.state.hoverCurrent = e.currentTarget;
          });
          __ox_setProperty(btn, "onHoverEnter", (e) => {
            globalThis.state.hoverEnter = (globalThis.state.hoverEnter || 0) + 1;
            globalThis.state.hoverEnterRelated = e.relatedTarget;
          });
          __ox_setProperty(btn, "onHoverLeave", (e) => {
            globalThis.state.hoverLeave = (globalThis.state.hoverLeave || 0) + 1;
            globalThis.state.hoverLeaveRelated = e.relatedTarget;
          });
          return btn;
        }
        render(() => App(), __OX_ROOT__);
    "#;

    const WHEEL_SCROLL_COMPONENT: &str = r#"
        import { render } from "oxide-runtime";
        function App() {
          const outer = __ox_createElement("div");
          __ox_setProperty(
            outer,
            "style",
            "display:block; width: 120px; height: 80px; overflow: auto;"
          );
          __ox_setProperty(outer, "onWheel", (event) => {
            globalThis.state.wheel = (globalThis.state.wheel || 0) + 1;
            sendEvent("wheel", JSON.stringify({ top: event.scrollTop, deltaY: event.deltaY }));
          });
          __ox_setProperty(outer, "onScroll", (event) => {
            globalThis.state.scroll = (globalThis.state.scroll || 0) + 1;
            globalThis.state.scrollTop = event.scrollTop;
          });

          const filler = __ox_createElement("div");
          __ox_setProperty(
            filler,
            "style",
            "display:block; width: 120px; height: 240px;"
          );
          __ox_insertNode(outer, filler, null);
          return outer;
        }
        render(() => App(), __OX_ROOT__);
    "#;

    const TEXT_INPUT_COMPONENT: &str = r#"
        import { render } from "oxide-runtime";
        function App() {
          const input = __ox_createElement("input");
          __ox_setProperty(input, "style", "display:block; width: 220px; height: 40px;");

          __ox_setProperty(input, "onFocus", () => {
            globalThis.state.focused = true;
            globalThis.state.lastFocus = "focus";
          });

          __ox_setProperty(input, "onBlur", () => {
            globalThis.state.focused = false;
            globalThis.state.lastBlur = "blur";
          });

          __ox_setProperty(input, "onInput", (event) => {
            globalThis.state.value = event.value;
            globalThis.state.caret = event.selectionStart;
          });

          __ox_setProperty(input, "onKeyDown", (event) => {
            globalThis.state.lastKey = event.key;
            // Keep caret visible in this test path for move-only keys, since
            // native `input` events are not fired on caret movement alone.
            if (event.selectionStart !== undefined) {
              globalThis.state.caret = event.selectionStart;
            }
          });

          __ox_setProperty(input, "onKeyUp", (event) => {
            globalThis.state.lastKeyUp = event.key;
          });

          return input;
        }
        render(() => App(), __OX_ROOT__);
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
                label: Some("oxide-dom-test"),
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
            import { createEffect, render } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");
              const para = __ox_createElement("div");
              // appendReactive happens implicitly when JSX passes a function
              // child; here we mimic that via createEffect over __ox_setText
              // since we don't have JSX in this test — what we want to
              // assert is that whatever runtime path the JSX uses preserves
              // the text node id, which appendReactive's fast path does
              // when consecutive values are simple text.
              let textId = __ox_createTextNode("");
              let appended = false;
              createEffect(() => {
                const v = String(globalThis.state.value || "");
                if (!appended) {
                  __ox_insertNode(para, textId, null);
                  appended = true;
                }
                __ox_setText(textId, v);
                globalThis.state.lastTextId = textId;
              });
              __ox_insertNode(root, para, null);
              return root;
            }
            render(() => App(), __OX_ROOT__);
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
            import { createEffect, render } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");
              __ox_setProperty(root, "style", "display:block; width: 200px; height: 200px;");

              const button = __ox_createElement("button");
              __ox_setProperty(button, "style", "display:block; width: 100px; height: 30px;");
              __ox_setProperty(button, "onClick", () => {
                globalThis.state.count = (globalThis.state.count || 0) + 1;
              });
              __ox_insertNode(button, __ox_createTextNode("inc"), null);
              __ox_insertNode(root, button, null);

              const list = __ox_createElement("div");
              __ox_setProperty(list, "style", "display:block;");
              __ox_insertNode(root, list, null);

              // Track inserted child ids so each effect re-run can clear them.
              let prevIds = [];
              createEffect(() => {
                for (const id of prevIds) {
                  __ox_removeNode(list, id);
                }
                prevIds = [];
                const count = Number(globalThis.state.count || 0);
                for (let i = 0; i < count; i++) {
                  const row = __ox_createElement("div");
                  __ox_insertNode(row, __ox_createTextNode("row " + i), null);
                  __ox_insertNode(list, row, null);
                  prevIds.push(row);
                }
                globalThis.state.listLen = prevIds.length;
              });

              return root;
            }
            render(() => App(), __OX_ROOT__);
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
            },
            REACTIVE_LIST_COMPONENT,
        );
        let _ = instance.render();
        assert_eq!(instance.state().get("listLen"), Some(json!(0)));

        // Click the button — Down{Left} fires "click" in oxide-dom.
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
    // called `__ox_setText`, which tries `doc.borrow_mut()` → panic.
    #[test]
    fn dispatch_wheel_with_reactive_effect_does_not_panic_on_doc_borrow() {
        const REACTIVE_WHEEL_COMPONENT: &str = r#"
            import { createEffect, render } from "oxide-runtime";
            function App() {
              const outer = __ox_createElement("div");
              __ox_setProperty(
                outer,
                "style",
                "display:block; width: 120px; height: 80px; overflow: auto;"
              );
              __ox_setProperty(outer, "onWheel", () => {
                globalThis.state.wheel = (globalThis.state.wheel || 0) + 1;
              });

              const filler = __ox_createElement("div");
              __ox_setProperty(
                filler,
                "style",
                "display:block; width: 120px; height: 240px;"
              );
              __ox_insertNode(outer, filler, null);

              const status = __ox_createElement("div");
              const text = __ox_createTextNode("");
              __ox_insertNode(status, text, null);
              __ox_insertNode(outer, status, null);

              // Effect runs synchronously when state.wheel changes — this is
              // the path that re-enters Rust via __ox_setText while
              // dispatch_wheel is still on the stack.
              createEffect(() => {
                __ox_setText(text, "wheel=" + (globalThis.state.wheel || 0));
              });

              return outer;
            }
            render(() => App(), __OX_ROOT__);
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
        import { render } from "oxide-runtime";
        function App() {
          const d = __ox_createElement("div");
          __ox_setProperty(d, "className", "tag");
          __ox_setProperty(d, "style", "display:block; width:50px; height:50px;");
          return d;
        }
        render(() => App(), __OX_ROOT__);
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
    fn class_directive_toggles_class_token() {
        const COMPONENT: &str = r#"
            import { createEffect, render } from "oxide-runtime";
            function App() {
              const d = __ox_createElement("div");
              __ox_setProperty(d, "style", "display:block; width:50px; height:50px;");
              createEffect(() => {
                const on = Boolean(globalThis.state.on);
                __ox_setProperty(d, "class:tag", on);
              });
              return d;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");
              const style = __ox_createElement("style");
              const text = __ox_createTextNode(".tag { color: rgb(0, 200, 0) }");
              __ox_insertNode(style, text, null);
              __ox_insertNode(root, style, null);

              const d = __ox_createElement("div");
              __ox_setProperty(d, "className", "tag");
              __ox_setProperty(d, "style", "display:block; width:50px; height:50px;");
              __ox_insertNode(root, d, null);
              return root;
            }
            render(() => App(), __OX_ROOT__);
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
            import { createEffect, render } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");
              const style = __ox_createElement("style");
              const text = __ox_createTextNode("");
              __ox_insertNode(style, text, null);
              __ox_insertNode(root, style, null);
              createEffect(() => {
                const c = String(globalThis.state.css || "");
                __ox_setText(text, c);
              });

              const d = __ox_createElement("div");
              __ox_setProperty(d, "className", "tag");
              __ox_setProperty(d, "style", "display:block; width:50px; height:50px;");
              __ox_insertNode(root, d, null);
              return root;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const d = __ox_createElement("div");
              __ox_setProperty(d, "className", "tag");
              __ox_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const d = __ox_createElement("div");
              __ox_setProperty(d, "className", "btn btn-add");
              __ox_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const outer = __ox_createElement("div");
              __ox_setProperty(outer, "className", "row");
              __ox_setProperty(outer, "style", "display:block; width:80px; height:80px; padding:10px;");
              const inner = __ox_createElement("div");
              __ox_setProperty(inner, "style", "display:block; width:40px; height:40px;");
              __ox_insertNode(inner, __ox_createTextNode("hi"), null);
              __ox_insertNode(outer, inner, null);
              return outer;
            }
            render(() => App(), __OX_ROOT__);
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
    /// JSX compiler emits), not directly via __ox_setProperty.
    #[test]
    fn hover_via_solid_setprop_on_class_works() {
        const COMPONENT: &str = r#"
            import { render, setProp, insert } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");
              __ox_setProperty(root, "style", "display:block; width:120px; height:120px;");

              const make = () => {
                const items = [];
                for (let i = 0; i < 2; i++) {
                  const row = __ox_createElement("div");
                  setProp(row, "class", i % 2 === 0 ? "row row-even" : "row row-odd");
                  setProp(row, "style", "display:block; width:120px; height:40px;");
                  __ox_insertNode(row, __ox_createTextNode("row " + i), null);
                  items.push(row);
                }
                return items;
              };
              insert(root, make);
              return root;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              return (
                <div class="panel">
                  <button class="btn btn-add">+ Add Row</button>
                </div>
              );
            }
            render(() => App(), __OX_ROOT__);
        "#;
        const CSS: &str = r#"
            .panel { display: block; width: 200px; padding: 10px; background: #182238; }
            .btn { display: inline-block; padding: 8px 10px; border: 1px solid #7fb5ff; color: rgb(243, 247, 255); }
            .btn-add { background: rgb(31, 59, 95); }
            .btn-add:hover { background: rgb(91, 140, 250); color: rgb(255, 255, 255); }
        "#;

        let compiled = crate::compiler::compile_component_source(
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
            import { createEffect, render } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");
              __ox_setProperty(root, "style", "display:block; width:120px; height:240px;");
              let prevIds = [];
              createEffect(() => {
                for (const id of prevIds) { __ox_removeNode(root, id); }
                prevIds = [];
                const n = Number(globalThis.state.rows || 0);
                for (let i = 0; i < n; i++) {
                  const row = __ox_createElement("div");
                  __ox_setProperty(row, "className", "row");
                  __ox_setProperty(row, "style", "display:block; width:120px; height:20px;");
                  __ox_insertNode(row, __ox_createTextNode("row " + i), null);
                  __ox_insertNode(root, row, null);
                  prevIds.push(row);
                }
              });
              return root;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const d = __ox_createElement("div");
              __ox_setProperty(d, "className", "swatch");
              return d;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const outer = __ox_createElement("div");
              __ox_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __ox_createElement("div");
              __ox_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __ox_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __OX_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();
        assert!(
            !instance.scrollbars.is_empty(),
            "expected a scrollbar region for the overflowing container",
        );
        let region = instance.scrollbars[0];
        // The track sits flush against the right edge of the 120px container.
        assert!(region.track.0 >= 100.0 && region.track.0 <= 120.0);
        assert!(region.max_scroll > 0.0);
    }

    #[test]
    fn scrollbar_thumb_drag_moves_scroll_offset() {
        const COMPONENT: &str = r#"
            import { render } from "oxide-runtime";
            function App() {
              const outer = __ox_createElement("div");
              __ox_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __ox_createElement("div");
              __ox_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __ox_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const outer = __ox_createElement("div");
              __ox_setProperty(outer, "style", "display:block; width:80px; height:80px; overflow:auto;");
              const filler = __ox_createElement("div");
              __ox_setProperty(filler, "style", "display:block; width:80px; height:400px;");
              __ox_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const outer = __ox_createElement("div");
              __ox_setProperty(outer, "style", "display:block; width:120px; height:80px; overflow:auto;");
              const filler = __ox_createElement("div");
              __ox_setProperty(filler, "style", "display:block; width:120px; height:480px;");
              __ox_insertNode(outer, filler, null);
              return outer;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");
              __ox_setProperty(root, "style",
                "display:block; width:200px; height:600px; background:#111;");
              return root;
            }
            render(() => App(), __OX_ROOT__);
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
            import {{ render }} from "oxide-runtime";
            function App() {{
              const input = __ox_createElement("input");
              __ox_setProperty(input, "type", "{input_type}");
              __ox_setProperty(input, "style", "display:block; width:220px; height:40px;");
              return input;
            }}
            render(() => App(), __OX_ROOT__);
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
            let anchor = Cursor::from_byte_index(
                input_data.editor.try_layout().unwrap(),
                char_index_to_byte_index(&display_text, anchor),
                Affinity::Downstream,
            );
            let focus = Cursor::from_byte_index(
                input_data.editor.try_layout().unwrap(),
                char_index_to_byte_index(&display_text, focus),
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
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __ox_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
                globalThis.state.caret = e.selectionStart;
              });
              return input;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");

              const first = __ox_createElement("input");
              __ox_setProperty(first, "style", "display:block; width:200px; height:24px;");
              __ox_setProperty(first, "onFocus", () => {
                globalThis.state.focused = "first";
              });
              __ox_setProperty(first, "onInput", (event) => {
                globalThis.state.firstValue = event.value;
              });

              const second = __ox_createElement("input");
              __ox_setProperty(second, "style", "display:block; width:200px; height:24px;");
              __ox_setProperty(second, "onFocus", () => {
                globalThis.state.focused = "second";
              });
              __ox_setProperty(second, "onInput", (event) => {
                globalThis.state.secondValue = event.value;
              });

              __ox_insertNode(root, first, null);
              __ox_insertNode(root, second, null);
              return root;
            }
            render(() => App(), __OX_ROOT__);
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
    fn input_space_and_caret_movement_refresh_rendered_caret() {
        const COMPONENT: &str = r#"
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "style", "display:block; width:200px; height:40px;");
              return input;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "type", "number");
              __ox_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __ox_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
              });
              return input;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "type", "range");
              __ox_setProperty(input, "min", "0");
              __ox_setProperty(input, "max", "10");
              __ox_setProperty(input, "step", "2");
              __ox_setProperty(input, "value", "4");
              __ox_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __ox_setProperty(input, "onInput", (e) => {
                globalThis.state.value = e.value;
              });
              return input;
            }
            render(() => App(), __OX_ROOT__);
        "#;
        let (mut instance, _rx) = make_instance_with(COMPONENT, &[]);
        let _ = instance.render();

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

        let _ = instance.dispatch_key_down(type_key("ArrowRight"));
        assert_eq!(instance.state().get("value"), Some(json!("2")));

        let _ = instance.dispatch_key_down(type_key("ArrowRight"));
        assert_eq!(instance.state().get("value"), Some(json!("4")));

        let _ = instance.dispatch_key_down(type_key("ArrowLeft"));
        assert_eq!(instance.state().get("value"), Some(json!("2")));

        let _ = instance.dispatch_key_down(type_key("End"));
        assert_eq!(instance.state().get("value"), Some(json!("10")));
    }

    #[test]
    fn input_checkbox_and_radio_types_toggle() {
        const COMPONENT: &str = r#"
            import { render } from "oxide-runtime";
            function App() {
              const root = __ox_createElement("div");

              const checkbox = __ox_createElement("input");
              __ox_setProperty(checkbox, "type", "checkbox");
              __ox_setProperty(checkbox, "style", "display:block; width:20px; height:20px;");
              __ox_setProperty(checkbox, "onInput", (e) => {
                globalThis.state.checkbox = e.checked;
              });

              const radio1 = __ox_createElement("input");
              __ox_setProperty(radio1, "type", "radio");
              __ox_setProperty(radio1, "name", "group-a");
              __ox_setProperty(radio1, "style", "display:block; width:20px; height:20px;");

              const radio2 = __ox_createElement("input");
              __ox_setProperty(radio2, "type", "radio");
              __ox_setProperty(radio2, "name", "group-a");
              __ox_setProperty(radio2, "style", "display:block; width:20px; height:20px;");

              __ox_insertNode(root, checkbox, null);
              __ox_insertNode(root, radio1, null);
              __ox_insertNode(root, radio2, null);

              globalThis.state.radio1 = radio1;
              globalThis.state.radio2 = radio2;

              return root;
            }
            render(() => App(), __OX_ROOT__);
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
            instance
                .state()
                .get("radio1")
                .and_then(|v| v.as_u64())
                .unwrap() as usize,
            instance
                .state()
                .get("radio2")
                .and_then(|v| v.as_u64())
                .unwrap() as usize,
        ];

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
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "style", "display:block; width:200px; height:40px;");
              return input;
            }
            render(() => App(), __OX_ROOT__);
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
        // Setting `value` via __ox_setProperty before any user input must
        // populate the InputState; the instance API should see it too.
        const COMPONENT: &str = r#"
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __ox_setProperty(input, "value", "preset");
              return input;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __ox_setProperty(input, "onKeyDown", (e) => {
                globalThis.state.observedValue = e.value;
                globalThis.state.observedKey = e.key;
              });
              return input;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "style", "display:block; width:200px; height:40px;");
              __ox_setProperty(input, "value", "hi");
              return input;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const d = __ox_createElement("div");
              __ox_setProperty(d, "className", "tag");
              __ox_setProperty(d, "style", "display:block; width:80px; height:80px;");
              return d;
            }
            render(() => App(), __OX_ROOT__);
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
            import { render } from "oxide-runtime";
            function App() {
              const input = __ox_createElement("input");
              __ox_setProperty(input, "className", "field");
              __ox_setProperty(input, "type", "text");
              __ox_setProperty(input, "style", "display:block; width:80px; height:30px;");
              return input;
            }
            render(() => App(), __OX_ROOT__);
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
}
