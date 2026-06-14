mod bridge;
mod outbox;
mod state;

pub(crate) use bridge::{DomBridge, HandlerMap};

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use blitz_dom::BaseDocument;
use rquickjs::loader::{BuiltinLoader, BuiltinResolver, FileResolver, Loader};
use rquickjs::{Context, Ctx, Error, Module, Object, Runtime, Value};
use tokio::sync::mpsc::UnboundedSender;

#[cfg(feature = "jsx-compiler")]
use crate::compiler;
use crate::events::{Event, KeyboardEvent};
use crate::state::StateHandle;

const SOLID_RUNTIME: &str = include_str!("../../js/dist/runtime.js");

const DEFAULT_JOB_BUDGET: u32 = 256;

/// Returned by [`JsContext::tick`] and [`JsContext::dispatch_event`] so the
/// host knows whether to call `render()` and whether to tick again soon.
#[derive(Debug, Clone, Copy, Default)]
pub struct TickResult {
    pub needs_paint: bool,
    pub jobs_pending: bool,
}

pub(crate) struct JsContext {
    // Field order matters for drop order (first declared ⟹ first dropped).
    // Persistent<Function> values must be freed while the Runtime is alive.
    // Clearing handlers inside context.with() ensures the GC is valid.
    runtime: Runtime,
    context: Context,
    doc: Rc<RefCell<BaseDocument>>,
    /// Persistent handler functions keyed by (node_id, event_name).
    pub(crate) handlers: Rc<RefCell<HandlerMap>>,
    /// Native-input state. Cloned into the bridge so attribute writes
    /// (`value`, `placeholder`, `type`, `readonly`) can update the input,
    /// and accessed by the [`Instance`] when dispatching key events.
    pub(crate) inputs: crate::input::InputRegistry,
}

impl Drop for JsContext {
    fn drop(&mut self) {
        // Free all Persistent<Function> values while the JS context and runtime
        // are still alive, avoiding the QuickJS "gc_obj_list not empty" abort.
        self.context.with(|_ctx| {
            self.handlers.borrow_mut().clear();
        });
    }
}

impl JsContext {
    pub fn new(doc: Rc<RefCell<BaseDocument>>) -> Self {
        Self::new_with_module_base(doc, None)
    }

    pub fn new_with_module_base(
        doc: Rc<RefCell<BaseDocument>>,
        module_base: Option<&Path>,
    ) -> Self {
        let runtime = Runtime::new().expect("rquickjs runtime");
        let mut file_resolver = FileResolver::default()
            .with_pattern("{}.js")
            .with_pattern("{}.mjs")
            .with_pattern("{}.jsx")
            .with_pattern("{}.ts")
            .with_pattern("{}.tsx")
            .with_pattern("{}.css");
        if let Some(module_base) = module_base.and_then(|path| path.parent()) {
            file_resolver = file_resolver.with_path(module_base.to_string_lossy().as_ref());
        }

        runtime.set_loader(
            (
                BuiltinResolver::default().with_module("oxide-runtime"),
                file_resolver,
            ),
            (
                BuiltinLoader::default().with_module("oxide-runtime", SOLID_RUNTIME),
                OxideModuleLoader,
                CssLoader,
            ),
        );
        let context = Context::full(&runtime).expect("rquickjs context");
        let handlers = Rc::new(RefCell::new(HandlerMap::new()));
        let inputs = crate::input::new_registry();
        Self {
            runtime,
            context,
            doc,
            handlers,
            inputs,
        }
    }

    pub fn mount(
        &self,
        component_source: &str,
        container_id: usize,
        state: &StateHandle,
        event_tx: UnboundedSender<Event>,
    ) {
        self.mount_with_module_path("app", component_source, container_id, state, event_tx)
    }

    pub(crate) fn mount_with_module_path(
        &self,
        module_path: &str,
        component_source: &str,
        container_id: usize,
        state: &StateHandle,
        event_tx: UnboundedSender<Event>,
    ) {
        let bridge = DomBridge::new(
            Rc::clone(&self.doc),
            Rc::clone(&self.handlers),
            Rc::clone(&self.inputs),
        );

        self.context.with(|ctx| {
            bridge.install(ctx.clone()).expect("bridge install");
            state::install(ctx.clone(), state).expect("state install");
            outbox::install(ctx.clone(), event_tx).expect("outbox install");
            ctx.globals()
                .set("__OX_ROOT__", container_id)
                .expect("set __OX_ROOT__");

            let bytes = component_source.as_bytes().to_vec();
            let module = Module::declare(ctx.clone(), module_path, bytes).unwrap_or_else(|err| {
                let exception = ctx.catch();
                panic!("module declare failed at {module_path}: {err}; exception = {exception:?}")
            });
            let (_m, _promise) = module.eval().unwrap_or_else(|err| {
                let exception = ctx.catch();
                panic!("module eval failed: {err}; exception = {exception:?}")
            });

            for _ in 0..DEFAULT_JOB_BUDGET {
                if !ctx.execute_pending_job() {
                    break;
                }
            }

            let init_state =
                serde_json::to_string(&state.snapshot()).unwrap_or_else(|_| "{}".into());
            let init_code = format!(
                r#"
                if (typeof __ox_state !== 'undefined' && typeof __ox_state.__init === 'function') {{
                    __ox_state.__init({init_state});
                }}
                "#,
            );
            let _: rquickjs::Value = ctx
                .eval(init_code)
                .unwrap_or(rquickjs::Value::new_null(ctx.clone()));
        });
    }

    /// Pump the JS job queue, applying pending state patches first.
    pub fn tick(&self, state: &StateHandle, budget: u32) -> TickResult {
        let patches = state.drain_patches();
        let has_patches = !patches.is_empty();
        if has_patches {
            self.context.with(|ctx| {
                for (path, value) in &patches {
                    state::apply_patch(ctx.clone(), path, value).expect("apply_patch");
                }
            });
        }

        let mut jobs_ran = 0u32;
        self.context.with(|ctx| {
            for _ in 0..budget {
                if !ctx.execute_pending_job() {
                    break;
                }
                jobs_ran += 1;
            }
        });

        TickResult {
            needs_paint: has_patches || jobs_ran > 0,
            jobs_pending: self.runtime.is_job_pending(),
        }
    }

    // ── Event dispatch ────────────────────────────────────────────────────────

    /// Walk the ancestor chain of `start_id` looking for a registered handler
    /// for `event_name`. Returns the first matching node_id, or `None`.
    pub fn find_handler_up(
        &self,
        doc: &BaseDocument,
        start_id: usize,
        event_name: &str,
    ) -> Option<usize> {
        let handlers = self.handlers.borrow();
        let mut id = start_id;
        loop {
            if handlers.contains_key(&(id, event_name.to_string())) {
                return Some(id);
            }
            id = doc.get_node(id)?.parent?;
            if id == 0 {
                break; // reached document root
            }
        }
        None
    }

    /// Call the stored handler for `(node_id, event_name)` (if any), then
    /// pump the job queue. Returns `TickResult` so the caller knows whether to
    /// repaint.
    pub fn dispatch_event(&self, node_id: usize, event_name: &str, x: f32, y: f32) -> TickResult {
        let Some(id) = self.find_handler_up(&self.doc.borrow(), node_id, event_name) else {
            return TickResult::default();
        };
        self.dispatch_event_at_with_target(id, event_name, x, y, node_id, None)
    }

    /// Call the stored keyboard handler for `(node_id, event_name)` (if any),
    /// then pump the job queue. Returns a [`TickResult`] so callers know whether
    /// to repaint.
    pub(crate) fn dispatch_key_event(
        &self,
        node_id: usize,
        event_name: &str,
        event: &KeyboardEvent,
    ) -> TickResult {
        let Some(id) = self.find_handler_up(&self.doc.borrow(), node_id, event_name) else {
            return TickResult::default();
        };
        self.dispatch_key_event_at(id, event_name, event)
    }

    /// Like [`dispatch_event`], but with an explicit related target payload.
    pub(crate) fn dispatch_event_with_related(
        &self,
        node_id: usize,
        event_name: &str,
        x: f32,
        y: f32,
        target_node_id: usize,
        related_node_id: Option<usize>,
    ) -> TickResult {
        let Some(id) = self.find_handler_up(&self.doc.borrow(), node_id, event_name) else {
            return TickResult::default();
        };
        self.dispatch_event_at_with_target(id, event_name, x, y, target_node_id, related_node_id)
    }

    /// Dispatch to a specific node without bubbling.
    pub(crate) fn dispatch_event_at(
        &self,
        node_id: usize,
        event_name: &str,
        x: f32,
        y: f32,
    ) -> TickResult {
        self.dispatch_event_at_with_target(node_id, event_name, x, y, node_id, None)
    }

    /// Dispatch keyboard event to a specific node without bubbling.
    pub(crate) fn dispatch_key_event_at(
        &self,
        node_id: usize,
        event_name: &str,
        event: &KeyboardEvent,
    ) -> TickResult {
        self.dispatch_key_event_at_with_target(node_id, event_name, event, node_id, None)
    }

    pub(crate) fn dispatch_event_at_with_target(
        &self,
        node_id: usize,
        event_name: &str,
        x: f32,
        y: f32,
        target_node_id: usize,
        related_node_id: Option<usize>,
    ) -> TickResult {
        let persistent = self
            .handlers
            .borrow()
            .get(&(node_id, event_name.to_string()))
            .cloned();

        let Some(p) = persistent else {
            return TickResult::default();
        };

        let mut jobs_ran = 0u32;
        self.context.with(|ctx| {
            if let Ok(func) = p.restore(&ctx) {
                // Build a DOM-like event object: { type, x, y, target,
                // currentTarget, relatedTarget }
                if let Ok(ev) = make_mouse_event(
                    &ctx,
                    event_name,
                    x,
                    y,
                    target_node_id,
                    node_id,
                    related_node_id,
                ) {
                    let _ = enrich_with_input(&ev, &self.inputs, target_node_id);
                    let _ = func.call::<(rquickjs::Value,), ()>((ev.into_value(),));
                } else {
                    let _ = func.call::<(), ()>(());
                }
            }
            // Pump microtasks that the handler may have queued (state updates, etc.)
            for _ in 0..DEFAULT_JOB_BUDGET {
                if !ctx.execute_pending_job() {
                    break;
                }
                jobs_ran += 1;
            }
        });

        TickResult {
            needs_paint: true, // handler ran → DOM likely mutated
            jobs_pending: self.runtime.is_job_pending(),
        }
    }

    pub(crate) fn dispatch_key_event_at_with_target(
        &self,
        node_id: usize,
        event_name: &str,
        event: &KeyboardEvent,
        target_node_id: usize,
        related_node_id: Option<usize>,
    ) -> TickResult {
        let persistent = self
            .handlers
            .borrow()
            .get(&(node_id, event_name.to_string()))
            .cloned();

        let Some(p) = persistent else {
            return TickResult::default();
        };

        let mut jobs_ran = 0u32;
        self.context.with(|ctx| {
            if let Ok(func) = p.restore(&ctx) {
                // Build a DOM-like keyboard event object.
                if let Ok(ev) = make_key_event(
                    &ctx,
                    event_name,
                    event,
                    target_node_id,
                    node_id,
                    related_node_id,
                ) {
                    let _ = enrich_with_input(&ev, &self.inputs, target_node_id);
                    let _ = func.call::<(rquickjs::Value,), ()>((ev.into_value(),));
                } else {
                    let _ = func.call::<(), ()>(());
                }
            }
            for _ in 0..DEFAULT_JOB_BUDGET {
                if !ctx.execute_pending_job() {
                    break;
                }
                jobs_ran += 1;
            }
        });

        TickResult {
            needs_paint: true, // handler ran → DOM likely mutated
            jobs_pending: self.runtime.is_job_pending(),
        }
    }

    /// Dispatch an `"input"` event to the registered handler on `node_id` (or
    /// an ancestor with one), carrying the current value of the input field.
    /// Called by [`Instance`] after every keystroke that mutated the field.
    pub(crate) fn dispatch_input_event(
        &self,
        node_id: usize,
        value: &str,
        caret: usize,
    ) -> TickResult {
        let Some(id) = self.find_handler_up(&self.doc.borrow(), node_id, "input") else {
            return TickResult::default();
        };
        let persistent = self
            .handlers
            .borrow()
            .get(&(id, "input".to_string()))
            .cloned();
        let Some(p) = persistent else {
            return TickResult::default();
        };
        self.context.with(|ctx| {
            if let Ok(func) = p.restore(&ctx) {
                if let Ok(ev) = make_input_event(&ctx, node_id, id, value, caret) {
                    let _ = func.call::<(rquickjs::Value,), ()>((ev.into_value(),));
                }
            }
            for _ in 0..DEFAULT_JOB_BUDGET {
                if !ctx.execute_pending_job() {
                    break;
                }
            }
        });
        TickResult {
            needs_paint: true,
            jobs_pending: self.runtime.is_job_pending(),
        }
    }

    pub(crate) fn dispatch_wheel_event(
        &self,
        node_id: usize,
        event_name: &str,
        x: f32,
        y: f32,
        delta_x: f32,
        delta_y: f32,
        target_node_id: usize,
        related_node_id: Option<usize>,
        scroll_left: f64,
        scroll_top: f64,
    ) -> TickResult {
        let persistent = self
            .handlers
            .borrow()
            .get(&(node_id, event_name.to_string()))
            .cloned();

        let Some(p) = persistent else {
            return TickResult::default();
        };

        let mut jobs_ran = 0u32;
        self.context.with(|ctx| {
            if let Ok(func) = p.restore(&ctx) {
                if let Ok(ev) = make_wheel_event(
                    &ctx,
                    event_name,
                    x,
                    y,
                    delta_x,
                    delta_y,
                    target_node_id,
                    node_id,
                    related_node_id,
                    scroll_left,
                    scroll_top,
                ) {
                    let _ = func.call::<(rquickjs::Value,), ()>((ev.into_value(),));
                } else {
                    let _ = func.call::<(), ()>(());
                }
            }
            for _ in 0..DEFAULT_JOB_BUDGET {
                if !ctx.execute_pending_job() {
                    break;
                }
                jobs_ran += 1;
            }
        });

        TickResult {
            needs_paint: true,
            jobs_pending: self.runtime.is_job_pending(),
        }
    }

    pub(crate) fn dispatch_scroll_event(
        &self,
        node_id: usize,
        x: f32,
        y: f32,
        scroll_left: f64,
        scroll_top: f64,
    ) -> TickResult {
        let mut result = TickResult::default();
        let node_id = if self
            .handlers
            .borrow()
            .contains_key(&(node_id, "scroll".to_string()))
        {
            node_id
        } else if let Some(scroll_ancestor) =
            self.find_handler_up(&self.doc.borrow(), node_id, "scroll")
        {
            // Target a bubbling ancestor with a registered listener.
            scroll_ancestor
        } else {
            return result;
        };

        if let Some(p) = self
            .handlers
            .borrow()
            .get(&(node_id, "scroll".to_string()))
            .cloned()
        {
            self.context.with(|ctx| {
                if let Ok(func) = p.restore(&ctx) {
                    if let Ok(ev) =
                        make_scroll_event(&ctx, x, y, node_id, node_id, scroll_left, scroll_top)
                    {
                        let _ = func.call::<(rquickjs::Value,), ()>((ev.into_value(),));
                    } else {
                        let _ = func.call::<(), ()>(());
                    }
                }
                for _ in 0..DEFAULT_JOB_BUDGET {
                    if !ctx.execute_pending_job() {
                        break;
                    }
                }
            });

            result.needs_paint = true;
            result.jobs_pending = self.runtime.is_job_pending();
        }

        result
    }
}

/// Build a DOM-like event object as a rquickjs `Object`:
/// `{ type, x, y, target, currentTarget, relatedTarget }`.
fn make_mouse_event<'js>(
    ctx: &rquickjs::Ctx<'js>,
    event_name: &str,
    x: f32,
    y: f32,
    target_node_id: usize,
    current_target_node_id: usize,
    related_target_node_id: Option<usize>,
) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("type", event_name)?;
    obj.set("x", x)?;
    obj.set("y", y)?;
    obj.set("target", target_node_id)?;
    obj.set("currentTarget", current_target_node_id)?;
    match related_target_node_id {
        Some(id) => obj.set("relatedTarget", id)?,
        None => obj.set("relatedTarget", Value::new_null(ctx.clone()))?,
    }
    Ok(obj)
}

/// Attach `value`, `selectionStart`, `selectionEnd` to the event object when
/// `target_node_id` refers to a registered `<input>`. Lets JS handlers read
/// `event.value` from any event on an input — mirroring the DOM where
/// `event.target.value` is always available on input events.
fn enrich_with_input<'js>(
    obj: &Object<'js>,
    inputs: &crate::input::InputRegistry,
    target_node_id: usize,
) -> rquickjs::Result<()> {
    let map = inputs.borrow();
    let Some(state) = map.get(&target_node_id) else {
        return Ok(());
    };
    obj.set("value", state.value())?;
    obj.set("selectionStart", state.caret())?;
    obj.set("selectionEnd", state.caret())?;
    Ok(())
}

/// Build a DOM-like `InputEvent`:
/// `{ type: "input", target, currentTarget, value, selectionStart, selectionEnd }`.
fn make_input_event<'js>(
    ctx: &rquickjs::Ctx<'js>,
    target_node_id: usize,
    current_target_node_id: usize,
    value: &str,
    caret: usize,
) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("type", "input")?;
    obj.set("target", target_node_id)?;
    obj.set("currentTarget", current_target_node_id)?;
    obj.set("relatedTarget", Value::new_null(ctx.clone()))?;
    obj.set("value", value)?;
    obj.set("selectionStart", caret)?;
    obj.set("selectionEnd", caret)?;
    Ok(obj)
}

fn make_wheel_event<'js>(
    ctx: &rquickjs::Ctx<'js>,
    event_name: &str,
    x: f32,
    y: f32,
    delta_x: f32,
    delta_y: f32,
    target_node_id: usize,
    current_target_node_id: usize,
    related_target_node_id: Option<usize>,
    scroll_left: f64,
    scroll_top: f64,
) -> rquickjs::Result<Object<'js>> {
    let obj = make_mouse_event(
        ctx,
        event_name,
        x,
        y,
        target_node_id,
        current_target_node_id,
        related_target_node_id,
    )?;
    obj.set("deltaX", delta_x)?;
    obj.set("deltaY", delta_y)?;
    obj.set("scrollX", scroll_left)?;
    obj.set("scrollY", scroll_top)?;
    obj.set("scrollLeft", scroll_left)?;
    obj.set("scrollTop", scroll_top)?;
    Ok(obj)
}

fn make_scroll_event<'js>(
    ctx: &rquickjs::Ctx<'js>,
    x: f32,
    y: f32,
    target_node_id: usize,
    current_target_node_id: usize,
    scroll_left: f64,
    scroll_top: f64,
) -> rquickjs::Result<Object<'js>> {
    let obj = make_mouse_event(
        ctx,
        "scroll",
        x,
        y,
        target_node_id,
        current_target_node_id,
        None,
    )?;
    obj.set("scrollX", scroll_left)?;
    obj.set("scrollY", scroll_top)?;
    obj.set("scrollLeft", scroll_left)?;
    obj.set("scrollTop", scroll_top)?;
    Ok(obj)
}

fn make_key_event<'js>(
    ctx: &rquickjs::Ctx<'js>,
    event_name: &str,
    event: &KeyboardEvent,
    target_node_id: usize,
    current_target_node_id: usize,
    related_target_node_id: Option<usize>,
) -> rquickjs::Result<Object<'js>> {
    let obj = Object::new(ctx.clone())?;
    obj.set("type", event_name)?;
    obj.set("key", event.key.clone())?;
    obj.set("code", event.code.clone())?;
    obj.set("keyCode", event.key_code)?;
    obj.set("repeat", event.repeat)?;
    obj.set("shiftKey", event.shift_key)?;
    obj.set("ctrlKey", event.ctrl_key)?;
    obj.set("altKey", event.alt_key)?;
    obj.set("metaKey", event.meta_key)?;
    obj.set("x", 0.0)?;
    obj.set("y", 0.0)?;
    obj.set("target", target_node_id)?;
    obj.set("currentTarget", current_target_node_id)?;
    match related_target_node_id {
        Some(id) => obj.set("relatedTarget", id)?,
        None => obj.set("relatedTarget", Value::new_null(ctx.clone()))?,
    }
    Ok(obj)
}

#[derive(Debug)]
struct OxideModuleLoader;

impl Loader for OxideModuleLoader {
    fn load<'js>(&mut self, ctx: &Ctx<'js>, path: &str) -> rquickjs::Result<Module<'js>> {
        if path.ends_with(".css") {
            return Err(Error::new_loading(path));
        }

        let path_ref = Path::new(path);
        let source = std::fs::read_to_string(path_ref)
            .map_err(|err| Error::new_loading_message(path, err.to_string()))?;
        #[cfg(feature = "jsx-compiler")]
        let output = if compiler::is_compilable_module(path_ref) {
            compiler::compile_module_source(path_ref, &source)
                .map_err(|err| Error::new_loading_message(path, err.to_string()))?
        } else {
            source
        };
        #[cfg(not(feature = "jsx-compiler"))]
        let output = {
            if is_jsx_or_ts_module(path_ref) {
                return Err(Error::new_loading_message(
                    path,
                    "JSX/TSX/TS module loading requires the `jsx-compiler` feature",
                ));
            }
            source
        };
        Module::declare(ctx.clone(), path, output.as_bytes())
    }
}

#[cfg(not(feature = "jsx-compiler"))]
fn is_jsx_or_ts_module(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "jsx" | "tsx" | "ts"))
}

#[derive(Debug)]
struct CssLoader;

impl Loader for CssLoader {
    fn load<'js>(&mut self, ctx: &Ctx<'js>, path: &str) -> rquickjs::Result<Module<'js>> {
        if !path.ends_with(".css") {
            return Err(Error::new_loading(path));
        }

        let css_text = std::fs::read_to_string(path)
            .map_err(|err| Error::new_loading_message(path, err.to_string()))?;
        let literal = serde_json::to_string(&css_text).expect("css text");
        // Side effect of import: auto-register with the document. The default
        // export still exposes the raw text for callers who want to inline it.
        let source = format!(
            "const __ox_css = {literal};\n\
             if (typeof __ox_register_stylesheet === 'function') {{\n\
                 __ox_register_stylesheet(__ox_css);\n\
             }}\n\
             export default __ox_css;\n"
        );
        Module::declare(ctx.clone(), path, source.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::{BaseDocument, DocumentConfig, LocalName, QualName, ns};
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::mpsc;

    fn unique_tmp_dir(prefix: &str) -> std::path::PathBuf {
        // Write under `target/test-tmp/` so a test that panics before its
        // `remove_dir_all` cleanup doesn't leave an `oxide-dom-<prefix>-…`
        // directory in the project root. `target/` is gitignored, and
        // rquickjs's `FileResolver` resolves all paths relative to the
        // current working directory (see `is_file` in
        // rquickjs-core/src/loader/file_resolver.rs), so using
        // `std::env::temp_dir()` would yield absolute paths that the
        // resolver can't find. Stay under cwd.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time")
            .as_nanos();
        let mut dir = std::path::PathBuf::from("target");
        dir.push("test-tmp");
        dir.push(format!("oxide-dom-{prefix}-{nanos}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn module_resolver_prefers_js_over_tsx_for_extensionless_imports() {
        let root = unique_tmp_dir("resolver-js-over-tsx");
        let module_path = root.join("main.js");
        let js_child = root.join("child.js");
        let tsx_child = root.join("child.tsx");

        std::fs::write(
            &module_path,
            r#"
            import { render } from "oxide-runtime";
            import { tag } from "./child";
            globalThis.__resolved_module = tag;
            function App() {
              const d = __ox_createElement("div");
              __ox_insertNode(d, __ox_createTextNode(globalThis.__resolved_module), null);
              return d;
            }
            render(() => App(), __OX_ROOT__);
            "#,
        )
        .expect("write main");
        std::fs::write(&js_child, "export const tag = \"js\";").expect("write js child");
        std::fs::write(&tsx_child, "export const tag = \"tsx\";").expect("write tsx child");

        let doc = Rc::new(RefCell::new(BaseDocument::new(DocumentConfig::default())));
        let container_id = {
            let mut d = doc.borrow_mut();
            let cid = d.mutate().create_element(
                QualName::new(None, ns!(html), LocalName::from("div")),
                vec![],
            );
            d.mutate().append_children(0, &[cid]);
            cid
        };
        let js = JsContext::new_with_module_base(
            Rc::clone(&doc),
            Some(std::path::Path::new(&module_path)),
        );
        let state = StateHandle::new(json!({}));
        let (tx, _rx) = mpsc::unbounded_channel();

        let module_source = std::fs::read_to_string(&module_path).expect("read main");
        js.mount_with_module_path(
            &module_path.to_string_lossy(),
            &module_source,
            container_id,
            &state,
            tx,
        );

        let resolved: String = js
            .context
            .with(|ctx| ctx.eval("__resolved_module"))
            .expect("resolved module");
        assert_eq!(resolved, "js");
    }

    #[cfg(feature = "jsx-compiler")]
    #[test]
    fn explicit_tsx_import_is_compiled_by_loader() {
        let root = unique_tmp_dir("resolver-tsx-compile");
        let module_path = root.join("main.js");
        let tsx_child = root.join("child.tsx");

        std::fs::write(
            &module_path,
            r#"
            import { render } from "oxide-runtime";
            import { tag } from "./child.tsx";
            globalThis.__resolved_module = tag;
            function App() {
              const d = __ox_createElement("div");
              __ox_insertNode(d, __ox_createTextNode(globalThis.__resolved_module), null);
              return d;
            }
            render(() => App(), __OX_ROOT__);
            "#,
        )
        .expect("write main");
        std::fs::write(&tsx_child, "export const tag: string = \"tsx\";").expect("write tsx child");

        let (doc, _, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _rx) = mpsc::unbounded_channel();
        let source = std::fs::read_to_string(&module_path).expect("read main");
        let js = JsContext::new_with_module_base(Rc::clone(&doc), Some(&module_path));
        js.mount_with_module_path(
            module_path.to_string_lossy().as_ref(),
            &source,
            container_id,
            &state,
            tx,
        );
        let d = doc.borrow();
        let container = d.get_node(container_id).expect("container");
        assert!(!container.children.is_empty(), "tsx import should mount");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn css_import_auto_registers_stylesheet() {
        let root = unique_tmp_dir("resolver-css-auto-register");
        let module_path = root.join("main.js");
        let css_path = root.join("styles.css");

        std::fs::write(&css_path, ".tagged { color: rgb(12, 34, 56); }").expect("write css");
        std::fs::write(
            &module_path,
            r#"
            import { render } from "oxide-runtime";
            import "./styles.css";
            function App() {
              const d = __ox_createElement("div");
              __ox_setProperty(d, "class", "tagged");
              __ox_setProperty(d, "style", "display:block; width:40px; height:40px;");
              return d;
            }
            render(() => App(), __OX_ROOT__);
            "#,
        )
        .expect("write main");

        let (doc, _, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _rx) = mpsc::unbounded_channel();
        let source = std::fs::read_to_string(&module_path).expect("read main");
        let js = JsContext::new_with_module_base(Rc::clone(&doc), Some(&module_path));
        js.mount_with_module_path(
            module_path.to_string_lossy().as_ref(),
            &source,
            container_id,
            &state,
            tx,
        );

        // Drive a style resolve so the imported stylesheet matches against the
        // mounted node and produces a computed color.
        doc.borrow_mut().resolve(0.0);

        let (r, g, b) = {
            let d = doc.borrow();
            let child_id = d
                .get_node(container_id)
                .and_then(|c| c.children.first().copied())
                .expect("child");
            let styles = d
                .get_node(child_id)
                .and_then(|n| n.primary_styles())
                .expect("styles");
            let srgb = styles
                .clone_color()
                .to_color_space(::style::color::ColorSpace::Srgb);
            let c = srgb.components;
            let to_u8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            (to_u8(c.0), to_u8(c.1), to_u8(c.2))
        };
        assert_eq!((r, g, b), (12, 34, 56));
        drop(js);

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn extract_event_name_supports_key_aliases() {
        let (_doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _rx) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const d = __ox_createElement("div");
              return d;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let mapped_key_down: String = js
            .context
            .with(|ctx| ctx.eval("__ox_extractEventName('onKeyDown')"))
            .expect("extract onKeyDown");
        let mapped_key_up: String = js
            .context
            .with(|ctx| ctx.eval("__ox_extractEventName('onKeyUp')"))
            .expect("extract onKeyUp");
        assert_eq!(mapped_key_down, "keydown");
        assert_eq!(mapped_key_up, "keyup");
    }

    #[test]
    fn dispatch_key_event_payload_includes_keyboard_data() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, mut rx) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const btn = __ox_createElement("button");
              __ox_setProperty(btn, "onKeyDown", (event) => {
                sendEvent("down", JSON.stringify(event));
              });
              return btn;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let btn_id = {
            let d = doc.borrow();
            d.get_node(container_id).unwrap().children[0]
        };

        let result = js.dispatch_key_event(
            btn_id,
            "keydown",
            &KeyboardEvent {
                key: "A".to_owned(),
                code: "KeyA".to_owned(),
                key_code: 65,
                repeat: true,
                shift_key: false,
                ctrl_key: false,
                alt_key: true,
                meta_key: false,
            },
        );
        assert!(result.needs_paint);

        let payload = rx
            .try_recv()
            .expect("keydown event should send payload")
            .payload;
        assert_eq!(payload["type"], json!("keydown"));
        assert_eq!(payload["target"], json!(btn_id));
        assert_eq!(payload["key"], json!("A"));
        assert_eq!(payload["code"], json!("KeyA"));
        assert_eq!(payload["keyCode"], json!(65));
        assert_eq!(payload["repeat"], json!(true));
        assert_eq!(payload["altKey"], json!(true));
    }

    #[test]
    fn dispatch_wheel_event_payload_includes_wheel_data() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, mut rx) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const btn = __ox_createElement("button");
              __ox_setProperty(btn, "onWheel", (event) => {
                sendEvent("wheel", JSON.stringify(event));
              });
              return btn;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        // Resolve mounted button id from document container children.
        let btn_id = {
            let d = doc.borrow();
            d.get_node(container_id)
                .and_then(|node| node.children.first().copied())
                .expect("button should be mounted")
        };

        let result = js.dispatch_wheel_event(
            btn_id, "wheel", 12.0, 18.0, 2.5, -3.0, btn_id, None, 0.0, 10.0,
        );
        assert!(result.needs_paint);

        let payload = rx
            .try_recv()
            .expect("wheel event should send payload")
            .payload;
        assert_eq!(payload["type"], json!("wheel"));
        assert_eq!(payload["target"], json!(btn_id));
        assert_eq!(payload["deltaX"], json!(2.5));
        assert_eq!(payload["deltaX"].as_f64(), Some(2.5));
        assert_eq!(payload["deltaY"].as_f64(), Some(-3.0));
        assert_eq!(payload["scrollTop"].as_f64(), Some(10.0));
        assert_eq!(payload["scrollLeft"].as_f64(), Some(0.0));
    }

    #[test]
    fn dispatch_scroll_event_payload_and_bubbles_to_ancestor() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, mut rx) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const parent = __ox_createElement("div");
              const child = __ox_createElement("div");
              __ox_setProperty(parent, "onScroll", (event) => {
                sendEvent("scroll", JSON.stringify(event));
                globalThis.state.hit = (globalThis.state.hit || 0) + 1;
              });
              __ox_insertNode(parent, child, null);
              return parent;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let parent_id = {
            let d = doc.borrow();
            d.get_node(container_id)
                .and_then(|node| node.children.first().copied())
                .expect("parent should be mounted")
        };
        let child_id = {
            let d = doc.borrow();
            d.get_node(parent_id)
                .and_then(|node| node.children.first().copied())
                .expect("child should be mounted")
        };

        let result = js.dispatch_scroll_event(child_id, 8.0, 6.0, 12.0, 34.0);
        assert!(result.needs_paint);

        assert_eq!(state.get("hit"), Some(json!(1)));
        let payload = rx
            .try_recv()
            .expect("scroll event should send payload")
            .payload;
        assert_eq!(payload["type"], json!("scroll"));
        assert_eq!(payload["target"], json!(parent_id));
        assert_eq!(payload["currentTarget"], json!(parent_id));
        assert_eq!(payload["scrollTop"].as_f64(), Some(34.0));
        assert_eq!(payload["scrollLeft"].as_f64(), Some(12.0));
    }

    // Diagnostic: show what the handler map contains after a full Solid mount.
    #[test]
    fn print_handler_map_after_solid_mount() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const btn = __ox_createElement("button");
              __ox_setProperty(btn, "onClick", () => {});
              return btn;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let keys: Vec<String> = js
            .handlers
            .borrow()
            .keys()
            .map(|(id, ev)| format!("{id}:{ev}"))
            .collect();
        let children: Vec<usize> = doc
            .borrow()
            .get_node(container_id)
            .map(|n| n.children.clone())
            .unwrap_or_default();
        // Print for diagnosis (will show in --nocapture output).
        println!("container_id={container_id}, children={children:?}, handler_keys={keys:?}");
        // The test just asserts they're non-empty to give us info.
        assert!(!children.is_empty(), "no children rendered");
    }

    // Diagnostic: trace node IDs during Solid render to find the ID mismatch.
    #[test]
    fn trace_node_ids_during_solid_render() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _) = mpsc::unbounded_channel();

        // Component that records which node_id it gets for the button.
        js.mount(
            r#"
            import { render } from "oxide-runtime";
            globalThis.__btn_id = null;
            function App() {
              const btn = __ox_createElement("button");
              globalThis.__btn_id = btn;
              __ox_setProperty(btn, "onClick", () => {});
              return btn;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        // Read back __btn_id from JS (may be either raw number or {__oxId: number}).
        let btn_id_from_js: Option<i32> = js.context.with(|ctx| {
            if let Ok(id) = ctx.eval::<i32, _>("__btn_id.__oxId") {
                return Some(id);
            }
            ctx.eval("__btn_id").ok()
        });
        let container_children = doc
            .borrow()
            .get_node(container_id)
            .unwrap()
            .children
            .clone();
        let handler_keys: Vec<String> = js
            .handlers
            .borrow()
            .keys()
            .map(|(id, ev)| format!("{id}:{ev}"))
            .collect();

        println!(
            "btn_id_from_js={btn_id_from_js:?}, container_children={container_children:?}, handler_keys={handler_keys:?}"
        );
        // Expect btn_id_from_js == container_children[0] and handler contains that id
        assert!(btn_id_from_js.is_some(), "btn_id was not set by App()");
        let expected_btn = btn_id_from_js.unwrap() as usize;
        assert_eq!(
            container_children,
            vec![expected_btn],
            "container has wrong node"
        );
        assert!(handler_keys.contains(&format!("{expected_btn}:click")));
    }

    // Diagnostic: directly exercise __ox_setProperty via Module eval (not Solid).
    #[test]
    fn set_property_via_module_eval_no_solid() {
        let doc = Rc::new(RefCell::new(BaseDocument::new(DocumentConfig::default())));
        let js = JsContext::new(Rc::clone(&doc));
        let state = StateHandle::new(json!({}));
        let (tx, _) = mpsc::unbounded_channel();

        // Mount without a real component — just set up bridge globals.
        // We use a minimal component that exercises __ox_setProperty with a function.
        js.mount(
            r#"
            // No solid import — call bridge directly
            const btn = __ox_createElement("button");
            __ox_setProperty(btn, "onClick", function() { return 42; });
            __ox_insertNode(__OX_ROOT__, btn, null);
            "#,
            0, // document root as container
            &state,
            tx,
        );

        let keys: Vec<String> = js
            .handlers
            .borrow()
            .keys()
            .map(|(id, ev)| format!("{id}:{ev}"))
            .collect();
        assert!(
            !keys.is_empty(),
            "handler map is empty after direct setProperty call; all keys: {keys:?}"
        );
        assert!(
            js.handlers.borrow().values().count() > 0,
            "should have at least one handler"
        );
    }

    fn make_setup() -> (Rc<RefCell<BaseDocument>>, JsContext, usize) {
        let doc = Rc::new(RefCell::new(BaseDocument::new(DocumentConfig::default())));
        let container_id = {
            let mut d = doc.borrow_mut();
            let cid = d.mutate().create_element(
                QualName::new(None, ns!(html), LocalName::from("div")),
                vec![],
            );
            d.mutate().append_children(0, &[cid]);
            cid
        };
        let js = JsContext::new(Rc::clone(&doc));
        (doc, js, container_id)
    }

    #[test]
    fn mount_static_component() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _rx) = mpsc::unbounded_channel();
        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const d = __ox_createElement("div");
              __ox_setProperty(d, "style", "color:white");
              const t = __ox_createTextNode("hello");
              __ox_insertNode(d, t, null);
              return d;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );
        let d = doc.borrow();
        let container = d.get_node(container_id).unwrap();
        assert!(
            !container.children.is_empty(),
            "container has no children after render"
        );
    }

    #[test]
    fn tick_is_noop_when_idle() {
        let (_doc, js, _cid) = make_setup();
        let state = StateHandle::new(json!({}));
        let res = js.tick(&state, 256);
        assert!(!res.jobs_pending);
    }

    #[test]
    fn rust_state_set_flows_to_js_state_proxy() {
        let (_doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({"counter": 0}));
        let (tx, _) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            globalThis.__read_counter = () => globalThis.state.counter;
            function App() {
              const d = __ox_createElement("div");
              __ox_insertNode(d, __ox_createTextNode("state bridge"), null);
              return d;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let before: Option<i32> = js.context.with(|ctx| ctx.eval("__read_counter()")).unwrap();
        assert_eq!(
            before,
            Some(0),
            "initial state should mirror Rust snapshot at mount"
        );

        state.set("counter", json!(17));
        let tick = js.tick(&state, 256);
        assert!(tick.needs_paint);
        let after: i32 = js.context.with(|ctx| ctx.eval("__read_counter()")).unwrap();
        assert_eq!(after, 17);
    }

    #[test]
    fn js_state_proxy_write_updates_rust_handle() {
        let (_doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            globalThis.__write_state = () => {
              globalThis.state.counter = 42;
              globalThis.state.items = [];
              globalThis.state.items[0] = "first";
            };
            function App() {
              const d = __ox_createElement("div");
              __ox_insertNode(d, __ox_createTextNode("proxy"), null);
              return d;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        js.context
            .with(|ctx| ctx.eval::<(), _>("__write_state()"))
            .unwrap();

        assert_eq!(state.get("counter"), Some(json!(42)));
        assert_eq!(state.get("items.0"), Some(json!("first")));
    }

    #[test]
    fn dispatch_event_on_click_updates_rust_state() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({ "count": 0 }));
        let (tx, _) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const btn = __ox_createElement("button");
              __ox_setProperty(btn, "onClick", () => {
                globalThis.state.count = (globalThis.state.count || 0) + 1;
              });
              return btn;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let btn_id = {
            let d = doc.borrow();
            let container = d.get_node(container_id).unwrap();
            assert!(!container.children.is_empty(), "button was not rendered");
            container.children[0]
        };

        let result = js.dispatch_event(btn_id, "click", 4.0, 5.0);
        assert!(result.needs_paint);
        assert_eq!(state.get("count"), Some(json!(1)));

        let second = js.dispatch_event(btn_id, "click", 4.0, 5.0);
        assert!(second.needs_paint);
        assert_eq!(state.get("count"), Some(json!(2)));
    }

    #[test]
    fn extract_event_name_supports_hover_aliases() {
        let (_doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const d = __ox_createElement("div");
              return d;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let mapped_hover: String = js
            .context
            .with(|ctx| ctx.eval("__ox_extractEventName('onHover')"))
            .expect("extract onHover");
        let mapped_hover_enter: String = js
            .context
            .with(|ctx| ctx.eval("__ox_extractEventName('onHoverEnter')"))
            .expect("extract onHoverEnter");
        let mapped_hover_leave: String = js
            .context
            .with(|ctx| ctx.eval("__ox_extractEventName('onHoverLeave')"))
            .expect("extract onHoverLeave");

        assert_eq!(mapped_hover, "hover");
        assert_eq!(mapped_hover_enter, "hoverenter");
        assert_eq!(mapped_hover_leave, "hoverleave");
        assert_eq!(
            js.context
                .with(|ctx| ctx.eval::<String, _>("__ox_extractEventName('onMouseOver')"))
                .expect("extract onMouseOver"),
            "mouseover"
        );
    }

    #[test]
    fn dispatch_event_payload_includes_hover_related_targets() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, mut rx) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const parent = __ox_createElement("div");
              const child = __ox_createElement("button");
              __ox_setProperty(parent, "onMouseOver", (event) => {
                sendEvent("over", JSON.stringify(event));
              });
              __ox_insertNode(parent, child, null);
              return parent;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let parent_id = {
            let d = doc.borrow();
            d.get_node(container_id).unwrap().children[0]
        };
        let child_id = {
            let d = doc.borrow();
            d.get_node(parent_id).unwrap().children[0]
        };

        let result = js.dispatch_event_with_related(
            child_id,
            "mouseover",
            1.0,
            2.0,
            child_id,
            Some(container_id),
        );
        assert!(result.needs_paint);

        let payload = rx
            .try_recv()
            .expect("mouseover event should send payload")
            .payload;

        assert_eq!(payload["type"], json!("mouseover"));
        assert_eq!(payload["target"], json!(child_id));
        assert_eq!(payload["currentTarget"], json!(parent_id));
        assert_eq!(payload["relatedTarget"], json!(container_id));
    }

    #[test]
    fn handler_stored_on_mount_and_callable() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, mut rx) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const btn = __ox_createElement("button");
              __ox_setProperty(btn, "onClick", () => sendEvent("clicked", "{}"));
              return btn;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        // Button should be a child of the container.
        let btn_id = {
            let d = doc.borrow();
            let container = d.get_node(container_id).unwrap();
            assert!(!container.children.is_empty());
            container.children[0]
        };

        // Handler should be in the map.
        assert!(
            js.handlers
                .borrow()
                .contains_key(&(btn_id, "click".to_string())),
            "click handler not found for btn_id={btn_id}"
        );

        // Dispatch the event.
        let result = js.dispatch_event(btn_id, "click", 10.0, 20.0);
        assert!(result.needs_paint);

        // The handler should have sent an event.
        let ev = rx.try_recv().expect("event should have been sent");
        assert_eq!(ev.name, "clicked");
    }

    #[test]
    fn find_handler_up_walks_ancestors() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, _rx) = mpsc::unbounded_channel();

        // Build: container → parent_div → child_span (handler on parent_div)
        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const parent = __ox_createElement("div");
              __ox_setProperty(parent, "onClick", () => {});
              const child = __ox_createElement("span");
              __ox_insertNode(parent, child, null);
              return parent;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let (parent_id, child_id) = {
            let d = doc.borrow();
            let container = d.get_node(container_id).unwrap();
            let parent = container.children[0];
            let child = d.get_node(parent).unwrap().children[0];
            (parent, child)
        };

        let d = doc.borrow();
        // Clicking on child_id should bubble up to parent_id where the handler lives.
        let found = js.find_handler_up(&d, child_id, "click");
        assert_eq!(found, Some(parent_id), "should bubble up to parent");
    }

    #[test]
    fn dispatch_event_on_child_bubbles_to_parent_handler() {
        let (doc, js, container_id) = make_setup();
        let state = StateHandle::new(json!({}));
        let (tx, mut rx) = mpsc::unbounded_channel();

        js.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const parent = __ox_createElement("div");
              __ox_setProperty(parent, "onClick", () => sendEvent("parent_clicked", "{}"));
              const child = __ox_createElement("span");
              __ox_insertNode(parent, child, null);
              return parent;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            container_id,
            &state,
            tx,
        );

        let child_id = {
            let d = doc.borrow();
            let parent_id = d.get_node(container_id).unwrap().children[0];
            d.get_node(parent_id).unwrap().children[0]
        };

        // Find handler ancestor for the child.
        let handler_node = {
            let d = doc.borrow();
            js.find_handler_up(&d, child_id, "click")
        };
        assert!(handler_node.is_some(), "should find parent's handler");
        let result = js.dispatch_event(handler_node.unwrap(), "click", 5.0, 5.0);
        assert!(result.needs_paint);

        let ev = rx.try_recv().expect("parent handler should fire");
        assert_eq!(ev.name, "parent_clicked");
    }

    #[test]
    fn two_contexts_are_independent() {
        // Two JsContexts must not share handler maps or documents.
        let (doc1, js1, cid1) = make_setup();
        let (doc2, js2, cid2) = make_setup();
        let state = StateHandle::new(json!({}));

        let (tx1, _) = mpsc::unbounded_channel();
        let (tx2, _) = mpsc::unbounded_channel();

        js1.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const btn = __ox_createElement("button");
              __ox_setProperty(btn, "onClick", () => {});
              return btn;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            cid1,
            &state,
            tx1,
        );

        js2.mount(
            r#"
            import { render } from "oxide-runtime";
            function App() {
              const t = __ox_createTextNode("other");
              return t;
            }
            render(() => App(), __OX_ROOT__);
            "#,
            cid2,
            &state,
            tx2,
        );

        // js1 should have a click handler; js2 should not.
        let js1_has_click = js1.handlers.borrow().keys().any(|(_, e)| e == "click");
        let js2_has_click = js2.handlers.borrow().keys().any(|(_, e)| e == "click");
        assert!(js1_has_click, "js1 should have a click handler");
        assert!(!js2_has_click, "js2 should have no click handler");

        // Documents are separate.
        assert!(!Rc::ptr_eq(&doc1, &doc2));
    }
}
