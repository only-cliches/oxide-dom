use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use blitz_dom::BaseDocument;
use blitz_dom::{LocalName, QualName, ns};
use rquickjs::{Ctx, Function, Persistent, Result as JsResult};

use crate::input::{InputRegistry, InputState};

#[cfg(test)]
use rquickjs::Value;

/// (node_id, event_name) → persistent handler function.
pub(crate) type HandlerMap = HashMap<(usize, String), Persistent<Function<'static>>>;

fn parse_bool_attr(value: &str) -> bool {
    let value = value.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | "0" | "false" | "off" => false,
        _ => true,
    }
}

pub(crate) struct DomBridge {
    doc: Rc<RefCell<BaseDocument>>,
    pub handlers: Rc<RefCell<HandlerMap>>,
    pub inputs: InputRegistry,
    pub selects: crate::select::SelectRegistry,
}

fn html_qual(tag: &str) -> QualName {
    QualName::new(None, ns!(html), LocalName::from(tag))
}

fn attr_qual(name: &str) -> QualName {
    QualName::new(None, ns!(), LocalName::from(name))
}

/// Returns the nearest ancestor `<select>` element if `node_id` is an `<option>`.
fn find_parent_select(doc: &BaseDocument, node_id: usize) -> Option<usize> {
    let mut id = node_id;
    while let Some(node) = doc.get_node(id) {
        if let Some(parent_id) = node.parent {
            if let Some(parent) = doc.get_node(parent_id) {
                if let Some(elem) = parent.element_data() {
                    if elem.name.local.as_ref() == "select" {
                        return Some(parent_id);
                    }
                }
            }
            id = parent_id;
        } else {
            break;
        }
    }
    None
}

/// Rebuild the select state from the current DOM subtree.
/// Walks all `<option>` children of the select and updates:
/// - options list
/// - selected_index
///
/// Returns the computed display text that should be updated in the synthetic display node.
fn rebuild_select_state_from_dom(
    doc: &BaseDocument,
    selects: &mut std::collections::HashMap<usize, crate::select::SelectState>,
    select_id: usize,
) -> Option<String> {
    let Some(select_node) = doc.get_node(select_id) else {
        return None;
    };

    let mut options = Vec::new();
    let mut selected_index = None;
    let children = select_node.children.clone();

    for child_id in children {
        if let Some(child) = doc.get_node(child_id) {
            if let Some(elem) = child.element_data() {
                if elem.name.local.as_ref() == "option" {
                    // Extract option attributes
                    let value = elem
                        .attrs
                        .iter()
                        .find(|a| a.name.local.as_ref() == "value")
                        .map(|a| a.value.to_string())
                        .unwrap_or_else(|| {
                            // Fallback to the option's text-node child if no value attribute.
                            doc.get_node(child_id)
                                .and_then(|n| n.children.first().copied())
                                .and_then(|grandchild_id| doc.get_node(grandchild_id))
                                .and_then(|n| n.text_data())
                                .map(|t| t.content.clone())
                                .unwrap_or_default()
                        });

                    // Option text lives in the first text-node child, not on the
                    // element itself.
                    let label = doc
                        .get_node(child_id)
                        .and_then(|n| n.children.first().copied())
                        .and_then(|grandchild_id| doc.get_node(grandchild_id))
                        .and_then(|n| n.text_data())
                        .map(|t| t.content.clone())
                        .unwrap_or_else(|| value.clone());

                    let disabled = elem
                        .attrs
                        .iter()
                        .find(|a| a.name.local.as_ref() == "disabled")
                        .is_some();

                    let is_selected = elem
                        .attrs
                        .iter()
                        .find(|a| a.name.local.as_ref() == "selected")
                        .is_some();

                    let hidden = elem
                        .attrs
                        .iter()
                        .find(|a| a.name.local.as_ref() == "hidden")
                        .is_some();

                    if is_selected && selected_index.is_none() {
                        selected_index = Some(options.len());
                    }

                    options.push(
                        crate::select::SelectOption::new(value, label, disabled).with_hidden(hidden),
                    );
                }
            }
        }
    }

    // Default to first enabled option if none explicitly selected
    if selected_index.is_none() {
        selected_index = options.iter().position(|opt| !opt.disabled);
    }

    if let Some(select_state) = selects.get_mut(&select_id) {
        select_state.set_options(options);
        select_state.set_selected_index(selected_index);
        Some(select_state.current_label())
    } else {
        None
    }
}

/// Write `display_text` into the synthetic text node that sits as the first
/// child of `select_id`. Uses `set_node_text` so blitz marks the subtree dirty
/// and the next layout/style pass picks up the new content.
fn sync_select_display_text(doc: &mut BaseDocument, select_id: usize, display_text: &str) {
    let Some(display_node_id) = doc.get_node(select_id).and_then(|n| n.children.first().copied()) else {
        return;
    };
    doc.mutate().set_node_text(display_node_id, display_text);
}


/// Returns the `<style>` element id that `node_id` belongs to, if any.
///
/// Checks the node itself, then walks up the parent chain. This lets us treat
/// `setText` on a text child of `<style>` and `insertNode` into `<style>`
/// uniformly: both refresh the same stylesheet.
fn enclosing_style_element(doc: &BaseDocument, node_id: usize) -> Option<usize> {
    let mut id = Some(node_id);
    while let Some(current) = id {
        let node = doc.get_node(current)?;
        if node
            .element_data()
            .is_some_and(|elem| elem.name.local.as_ref() == "style")
        {
            return Some(current);
        }
        id = node.parent;
    }
    None
}

impl DomBridge {
    pub fn new(
        doc: Rc<RefCell<BaseDocument>>,
        handlers: Rc<RefCell<HandlerMap>>,
        inputs: InputRegistry,
        selects: crate::select::SelectRegistry,
    ) -> Self {
        Self {
            doc,
            handlers,
            inputs,
            selects,
        }
    }

    /// Register all bridge globals on `ctx`.
    ///
    /// The design for property/event dispatch uses a thin JS wrapper
    /// (`__ox_setProperty`) so that Rust only ever receives strongly-typed
    /// arguments — avoiding the lifetime complications of `Value<'js>` inside
    /// a `Function::new` closure.
    pub fn install<'js>(&self, ctx: Ctx<'js>) -> JsResult<()> {
        let globals = ctx.globals();

        // ── __ox_register_stylesheet — auto-applied CSS imports ───────────────
        //
        // Invoked by the CSS module loader as a side effect of `import "./x.css"`
        // so the rules become active without the component needing to mount a
        // `<style>` element explicitly. The default export still returns the
        // raw CSS text, so callers who want full control can choose not to
        // import for side effects or to emit their own `<style>` instead.
        {
            let doc = Rc::clone(&self.doc);
            globals.set(
                "__ox_register_stylesheet",
                Function::new(ctx.clone(), move |css: String| -> JsResult<()> {
                    doc.borrow_mut().add_user_agent_stylesheet(&css);
                    Ok(())
                }),
            )?;
        }

        // ── createElement ─────────────────────────────────────────────────────
        //
        // For `<input>`, we additionally register the node in the input
        // registry and seed it with an empty inner text node that the
        // Instance updates as the user edits. The text node is owned by
        // the engine — JS shouldn't mutate it, but if it does, the next
        // edit overwrites it on the next render.
        //
        // For `<select>`, we similarly register the node in the select registry
        // and create a synthetic text display node that shows the selected option.
        // Real `<option>` children are kept as normal DOM children (with display: none).
        {
            let doc = Rc::clone(&self.doc);
            let inputs = Rc::clone(&self.inputs);
            let selects = Rc::clone(&self.selects);
            globals.set(
                "__ox_createElement",
                Function::new(ctx.clone(), move |tag: String| -> JsResult<usize> {
                    let mut d = doc.borrow_mut();
                    let id = d.mutate().create_element(html_qual(&tag), vec![]);
                    if tag.eq_ignore_ascii_case("input") {
                        let text_id = d.create_text_node("");
                        d.mutate().append_children(id, &[text_id]);
                        drop(d);
                        inputs.borrow_mut().insert(id, InputState::default());
                    } else if tag.eq_ignore_ascii_case("select") {
                        let text_id = d.create_text_node("");
                        d.mutate().append_children(id, &[text_id]);
                        drop(d);
                        selects.borrow_mut().insert(id, crate::select::SelectState::default());
                    }
                    Ok(id)
                }),
            )?;
        }

        // ── createTextNode ────────────────────────────────────────────────────
        {
            let doc = Rc::clone(&self.doc);
            globals.set(
                "__ox_createTextNode",
                Function::new(ctx.clone(), move |text: String| -> JsResult<usize> {
                    Ok(doc.borrow_mut().create_text_node(&text))
                }),
            )?;
        }

        // ── __ox_setAttr — string/boolean/number attributes ───────────────────
        //
        // For input-managed attributes (`value`, `placeholder`, `type`,
        // `checked`, `name`, `min`, `max`, `step`, `readonly`, `disabled`),
        // update the InputState too so the engine's editable text mirrors
        // what JS asked for and subsequent `input` events carry the new
        // value back.
        //
        // For select-managed attributes (`name`, `disabled`) and option attributes
        // (`value`, `selected`, `disabled`), update SelectState and trigger
        // option list rebuilding.
        {
            let doc = Rc::clone(&self.doc);
            let inputs = Rc::clone(&self.inputs);
            let selects = Rc::clone(&self.selects);
            globals.set(
                "__ox_setAttr",
                Function::new(
                    ctx.clone(),
                    move |node_id: usize, key: String, value: String| -> JsResult<()> {
                        doc.borrow_mut()
                            .mutate()
                            .set_attribute(node_id, attr_qual(&key), &value);
                        match key.as_str() {
                            "value" => {
                                let handled_input = inputs
                                    .borrow_mut()
                                    .get_mut(&node_id)
                                    .map(|state| state.set_value(value))
                                    .is_some();
                                if !handled_input {
                                    let parent_select_id = find_parent_select(&doc.borrow(), node_id);
                                    if let Some(parent_select_id) = parent_select_id {
                                        let borrow = doc.borrow();
                                        let display_text = rebuild_select_state_from_dom(&borrow, &mut selects.borrow_mut(), parent_select_id);
                                        drop(borrow);
                                        if let Some(display_text) = display_text {
                                            sync_select_display_text(&mut doc.borrow_mut(), parent_select_id, &display_text);
                                        }
                                    }
                                }
                            }
                            "placeholder" => {
                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_placeholder(Some(value));
                                }
                            }
                            "type" => {
                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_input_type(&value);
                                }
                            }
                            "checked" => {
                                let checked = parse_bool_attr(&value);
                                let (is_radio, group_name) = {
                                    let state = inputs.borrow();
                                    let Some(state) = state.get(&node_id) else {
                                        return Ok(());
                                    };
                                    (
                                        state.kind() == crate::input::InputType::Radio,
                                        state
                                            .name()
                                            .map(str::to_owned)
                                            .filter(|name| !name.is_empty()),
                                    )
                                };

                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_checked(checked);
                                }

                                // Sync blitz-dom's internal CheckboxInput so the visual
                                // state stays in sync with programmatic JS updates.
                                if let Some(node) = doc.borrow_mut().get_node_mut(node_id) {
                                    if let Some(el) = node.element_data_mut() {
                                        if let Some(is_checked) = el.checkbox_input_checked_mut() {
                                            *is_checked = checked;
                                        }
                                    }
                                }

                                if is_radio
                                    && checked
                                    && let Some(group_name) = group_name
                                {
                                    let mut map = inputs.borrow_mut();
                                    for (other_id, other_state) in map.iter_mut() {
                                        if *other_id == node_id {
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
                            }
                            "name" => {
                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_name(if value.is_empty() {
                                        None
                                    } else {
                                        Some(value)
                                    });
                                }
                            }
                            "min" => {
                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_min(value.parse::<f64>().ok());
                                }
                            }
                            "max" => {
                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_max(value.parse::<f64>().ok());
                                }
                            }
                            "step" => {
                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_step(value.parse::<f64>().ok());
                                }
                            }
                            "readonly" => {
                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_readonly(parse_bool_attr(&value));
                                }
                            }
                            "disabled" => {
                                if let Some(state) = inputs.borrow_mut().get_mut(&node_id) {
                                    state.set_disabled(parse_bool_attr(&value));
                                }
                                if let Some(state) = selects.borrow_mut().get_mut(&node_id) {
                                    state.set_disabled(parse_bool_attr(&value));
                                }
                            }
                            _ => {
                                // Handle option-specific attributes by finding the parent select
                                if let Some(parent_select_id) = find_parent_select(&doc.borrow(), node_id) {
                                    match key.as_str() {
                                        "value" | "selected" => {
                                            let borrow = doc.borrow();
                                            let display_text = rebuild_select_state_from_dom(&borrow, &mut selects.borrow_mut(), parent_select_id);
                                            drop(borrow);
                                            if let Some(display_text) = display_text {
                                                sync_select_display_text(&mut doc.borrow_mut(), parent_select_id, &display_text);
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        Ok(())
                    },
                ),
            )?;
        }

        // ── __ox_getAttr — read an attribute value ────────────────────────────
        {
            let doc = Rc::clone(&self.doc);
            globals.set(
                "__ox_getAttr",
                Function::new(
                    ctx.clone(),
                    move |node_id: usize, key: String| -> JsResult<Option<String>> {
                        Ok(doc
                            .borrow()
                            .get_node(node_id)
                            .and_then(|node| node.element_data())
                            .and_then(|elem| {
                                elem.attrs
                                    .iter()
                                    .find(|a| a.name.local.as_ref() == key)
                                    .map(|a| a.value.to_string())
                            }))
                    },
                ),
            )?;
        }

        // ── __ox_setHandler — event handler (receives Persistent<Function>) ───
        //
        // `Persistent<Function<'static>>` implements `FromJs<'js>` via rquickjs,
        // so the JS→Rust conversion and `Persistent::save` happen automatically.
        {
            let handlers = Rc::clone(&self.handlers);
            globals.set(
                "__ox_setHandler",
                Function::new(
                    ctx.clone(),
                    move |node_id: usize,
                          event_name: String,
                          handler: Persistent<Function<'static>>|
                          -> JsResult<()> {
                        handlers.borrow_mut().insert((node_id, event_name), handler);
                        Ok(())
                    },
                ),
            )?;
        }

        // ── __ox_setProperty — JS dispatcher ─────────────────────────────────
        //
        // Decides at runtime whether the value is a handler or a plain attr and
        // forwards to the appropriate Rust function above.
        //
        // Special-cases:
        //   - `className` → `class` (React/JSX convention).
        //   - `class:foo={cond}` (Solid directive) → toggles `foo` in the class
        //     attribute. Anything truthy adds; anything falsy removes.
        //   - `style:foo={value}` (Solid directive) → sets `foo: <value>` in the
        //     style attribute, leaving other declarations alone.
        ctx.eval::<(), _>(
            r#"
            function __ox_tokenize(str) {
                return str ? str.split(/\s+/).filter(Boolean) : [];
            }
            globalThis.__ox_toggleClass = function(nodeId, token, active) {
                var current = __ox_getAttr(nodeId, 'class') || '';
                var tokens = __ox_tokenize(current);
                var idx = tokens.indexOf(token);
                if (active) {
                    if (idx < 0) tokens.push(token);
                } else {
                    if (idx >= 0) tokens.splice(idx, 1);
                }
                __ox_setAttr(nodeId, 'class', tokens.join(' '));
            };
            globalThis.__ox_setStyleDecl = function(nodeId, prop, value) {
                var current = __ox_getAttr(nodeId, 'style') || '';
                var decls = current.split(';').map(function(s){return s.trim();}).filter(Boolean);
                var found = false;
                var out = [];
                for (var i = 0; i < decls.length; i++) {
                    var ci = decls[i].indexOf(':');
                    if (ci < 0) { out.push(decls[i]); continue; }
                    var name = decls[i].slice(0, ci).trim();
                    if (name === prop) {
                        if (value !== null && value !== undefined && value !== '') {
                            out.push(prop + ': ' + String(value));
                        }
                        found = true;
                    } else {
                        out.push(decls[i]);
                    }
                }
                if (!found && value !== null && value !== undefined && value !== '') {
                    out.push(prop + ': ' + String(value));
                }
                __ox_setAttr(nodeId, 'style', out.join('; '));
            };
            globalThis.__ox_setProperty = function(nodeId, key, value) {
                if (key === 'className') key = 'class';
                if (key.startsWith('class:')) {
                    __ox_toggleClass(nodeId, key.slice(6), Boolean(value));
                    return;
                }
                if (key.startsWith('style:')) {
                    __ox_setStyleDecl(nodeId, key.slice(6), value);
                    return;
                }
                if (typeof value === 'function') {
                    var event = __ox_extractEventName(key);
                    if (event !== null) {
                        __ox_setHandler(nodeId, event, value);
                    }
                    // Non-event function properties (e.g. "ref") are ignored.
                } else if (value !== null && value !== undefined) {
                    __ox_setAttr(nodeId, key, String(value));
                } else {
                    // null/undefined → remove handler if any
                    var event = __ox_extractEventName(key);
                    if (event !== null) {
                        __ox_removeHandler(nodeId, event);
                    }
                }
            };
            "#,
        )?;

        // ── __ox_extractEventName — JS helper ─────────────────────────────────
        ctx.eval::<(), _>(
            r#"
            globalThis.__ox_extractEventName = function(key) {
                let event = null;
                if (key.startsWith('on:')) event = key.slice(3).toLowerCase();
                else if (key.startsWith('on') && key.length > 2) event = key.slice(2).toLowerCase();
                else return null;

                switch (event) {
                    case 'hoverenter':
                    case 'hoverleave':
                        return event;
                    case 'hover':
                        // Backward-compatible alias used by the previous API.
                        return 'hover';
                    default:
                        return event;
                }
            };
            "#,
        )?;

        // ── __ox_removeHandler — removes a stored handler ─────────────────────
        {
            let handlers = Rc::clone(&self.handlers);
            globals.set(
                "__ox_removeHandler",
                Function::new(
                    ctx.clone(),
                    move |node_id: usize, event_name: String| -> JsResult<()> {
                        handlers.borrow_mut().remove(&(node_id, event_name));
                        Ok(())
                    },
                ),
            )?;
        }

        // ── insertNode ────────────────────────────────────────────────────────
        //
        // When inserting into a `<style>` element (or inserting a `<style>`
        // element with text already attached), reprocess the stylesheet so the
        // CSS contents become active.
        //
        // When inserting an `<option>` into a `<select>`, rebuild the select state.
        {
            let doc = Rc::clone(&self.doc);
            let selects = Rc::clone(&self.selects);
            globals.set(
                "__ox_insertNode",
                Function::new(
                    ctx.clone(),
                    move |parent: usize, node: usize, anchor: Option<usize>| -> JsResult<()> {
                        let mut borrow = doc.borrow_mut();
                        {
                            let mut m = borrow.mutate();
                            match anchor {
                                Some(a) => m.insert_nodes_before(a, &[node]),
                                None => m.append_children(parent, &[node]),
                            }
                        }
                        // If either the parent or the inserted node is a style
                        // element, ensure its stylesheet is current.
                        if let Some(style_id) = enclosing_style_element(&borrow, parent)
                            .or_else(|| enclosing_style_element(&borrow, node))
                        {
                            borrow.upsert_stylesheet_for_node(style_id);
                        }

                        // If parent is a select directly, or an option inside a
                        // select (so the inserted node contributes to an option's
                        // label/value), rebuild the select.
                        let select_to_rebuild = {
                            let parent_node = borrow.get_node(parent);
                            let tag = parent_node
                                .and_then(|n| n.element_data())
                                .map(|e| e.name.local.as_ref().to_owned());
                            match tag.as_deref() {
                                Some("select") => Some(parent),
                                Some("option") => find_parent_select(&borrow, parent),
                                _ => None,
                            }
                        };
                        if let Some(select_id) = select_to_rebuild {
                            let display_text = rebuild_select_state_from_dom(&borrow, &mut selects.borrow_mut(), select_id);
                            drop(borrow);
                            if let Some(display_text) = display_text {
                                sync_select_display_text(&mut doc.borrow_mut(), select_id, &display_text);
                            }
                        }

                        Ok(())
                    },
                ),
            )?;
        }

        // ── removeNode ────────────────────────────────────────────────────────
        //
        // After removing a node, if it was an option in a select, rebuild the select state.
        {
            let doc = Rc::clone(&self.doc);
            let selects = Rc::clone(&self.selects);
            globals.set(
                "__ox_removeNode",
                Function::new(
                    ctx.clone(),
                    move |_parent: usize, node: usize| -> JsResult<()> {
                        // Find parent select before removal
                        let parent_select = {
                            let borrow = doc.borrow();
                            find_parent_select(&borrow, node)
                        };

                        doc.borrow_mut().mutate().remove_and_drop_node(node);

                        // Rebuild select state if this was an option
                        if let Some(select_id) = parent_select {
                            let borrow = doc.borrow();
                            let display_text = rebuild_select_state_from_dom(&borrow, &mut selects.borrow_mut(), select_id);
                            drop(borrow);
                            if let Some(display_text) = display_text {
                                sync_select_display_text(&mut doc.borrow_mut(), select_id, &display_text);
                            }
                        }

                        Ok(())
                    },
                ),
            )?;
        }

        // ── setText ───────────────────────────────────────────────────────────
        //
        // If this text node lives under a `<style>` element, refresh the
        // attached stylesheet so the new CSS source takes effect.
        // If it's a text child of an `<option>`, rebuild the parent select.
        {
            let doc = Rc::clone(&self.doc);
            let selects = Rc::clone(&self.selects);
            globals.set(
                "__ox_setText",
                Function::new(
                    ctx.clone(),
                    move |node_id: usize, text: String| -> JsResult<()> {
                        let parent_select = {
                            let borrow = doc.borrow();
                            if let Some(parent_id) = borrow.get_node(node_id).and_then(|n| n.parent) {
                                if let Some(parent) = borrow.get_node(parent_id) {
                                    if parent.element_data().is_some_and(|e| e.name.local.as_ref() == "option") {
                                        find_parent_select(&borrow, parent_id)
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        };

                        let mut borrow = doc.borrow_mut();
                        borrow.mutate().set_node_text(node_id, &text);
                        if let Some(style_id) = enclosing_style_element(&borrow, node_id) {
                            borrow.upsert_stylesheet_for_node(style_id);
                        }
                        drop(borrow);

                        if let Some(select_id) = parent_select {
                            let borrow = doc.borrow();
                            let display_text = rebuild_select_state_from_dom(&borrow, &mut selects.borrow_mut(), select_id);
                            drop(borrow);
                            if let Some(display_text) = display_text {
                                sync_select_display_text(&mut doc.borrow_mut(), select_id, &display_text);
                            }
                        }

                        Ok(())
                    },
                ),
            )?;
        }

        // ── Tree-traversal ops (used by Solid's reconciler) ───────────────────
        {
            let doc = Rc::clone(&self.doc);
            globals.set(
                "__ox_getFirstChild",
                Function::new(
                    ctx.clone(),
                    move |node_id: usize| -> JsResult<Option<usize>> {
                        Ok(doc
                            .borrow()
                            .get_node(node_id)
                            .and_then(|n| n.children.first().copied()))
                    },
                ),
            )?;
        }
        {
            let doc = Rc::clone(&self.doc);
            globals.set(
                "__ox_getNextSibling",
                Function::new(
                    ctx.clone(),
                    move |node_id: usize| -> JsResult<Option<usize>> {
                        let borrow = doc.borrow();
                        Ok(borrow.get_node(node_id).and_then(|node| {
                            let parent = borrow.get_node(node.parent?)?;
                            let pos = parent.children.iter().position(|&c| c == node_id)?;
                            parent.children.get(pos + 1).copied()
                        }))
                    },
                ),
            )?;
        }
        {
            let doc = Rc::clone(&self.doc);
            globals.set(
                "__ox_getParentNode",
                Function::new(
                    ctx.clone(),
                    move |node_id: usize| -> JsResult<Option<usize>> {
                        Ok(doc.borrow().get_node(node_id).and_then(|n| n.parent))
                    },
                ),
            )?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_dom::{BaseDocument, DocumentConfig};
    use rquickjs::{Context, Runtime};

    fn make_bridge() -> (
        Rc<RefCell<BaseDocument>>,
        Rc<RefCell<HandlerMap>>,
        DomBridge,
    ) {
        let doc = Rc::new(RefCell::new(BaseDocument::new(DocumentConfig::default())));
        let handlers = Rc::new(RefCell::new(HandlerMap::new()));
        let inputs = crate::input::new_registry();
        let selects = crate::select::new_registry();
        let bridge = DomBridge::new(Rc::clone(&doc), Rc::clone(&handlers), inputs, selects);
        (doc, handlers, bridge)
    }

    fn setup() -> (
        Rc<RefCell<BaseDocument>>,
        Rc<RefCell<HandlerMap>>,
        DomBridge,
        Runtime,
        Context,
    ) {
        let (doc, handlers, bridge) = make_bridge();
        let rt = Runtime::new().unwrap();
        let ctx = Context::full(&rt).unwrap();
        (doc, handlers, bridge, rt, ctx)
    }

    // Helper: clear handler Persistents while ctx/rt are still alive, preventing
    // QuickJS's "gc_obj_list not empty" abort on Runtime drop.
    fn cleanup(handlers: &Rc<RefCell<HandlerMap>>, ctx: &Context) {
        ctx.with(|_ctx| {
            handlers.borrow_mut().clear();
        });
    }

    #[test]
    fn bridge_installs_all_globals() {
        let (_, _, bridge, _rt, ctx) = setup();
        ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let g = ctx.globals();
            for name in &[
                "__ox_createElement",
                "__ox_createTextNode",
                "__ox_setProperty",
                "__ox_setAttr",
                "__ox_setHandler",
                "__ox_insertNode",
                "__ox_removeNode",
                "__ox_setText",
                "__ox_getFirstChild",
                "__ox_getNextSibling",
                "__ox_getParentNode",
            ] {
                let _: Value = g
                    .get(*name)
                    .unwrap_or_else(|_| panic!("{name} not installed"));
            }
        });
    }

    #[test]
    fn create_element_and_text_node() {
        let (doc, _, bridge, _rt, ctx) = setup();
        ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let elem_id: usize = ctx.eval("__ox_createElement('div')").unwrap();
            let text_id: usize = ctx.eval("__ox_createTextNode('hello')").unwrap();
            assert_ne!(elem_id, text_id);
            let d = doc.borrow();
            assert!(d.get_node(elem_id).is_some());
            assert!(d.get_node(text_id).is_some());
        });
    }

    #[test]
    fn insert_node_appends_child() {
        let (doc, _, bridge, _rt, ctx) = setup();
        ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let child_id: usize = ctx
                .eval("const d = __ox_createElement('div'); __ox_insertNode(0, d, null); d")
                .unwrap();
            let d = doc.borrow();
            assert!(d.get_node(0).unwrap().children.contains(&child_id));
        });
    }

    #[test]
    fn set_text_updates_content() {
        let (doc, _, bridge, _rt, ctx) = setup();
        ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let text_id: usize = ctx
                .eval("const t = __ox_createTextNode('old'); __ox_setText(t, 'new'); t")
                .unwrap();
            let d = doc.borrow();
            if let blitz_dom::NodeData::Text(ref td) = d.get_node(text_id).unwrap().data {
                assert_eq!(td.content, "new");
            } else {
                panic!("expected text node");
            }
        });
    }

    #[test]
    fn set_property_string_sets_attribute() {
        let (doc, _, bridge, _rt, ctx) = setup();
        ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let node_id: usize = ctx.eval("__ox_createElement('div')").unwrap();
            let _: Value = ctx
                .eval(format!("__ox_setProperty({node_id}, 'style', 'color:red')"))
                .unwrap();
            let d = doc.borrow();
            let elem = d.get_node(node_id).unwrap().element_data().unwrap();
            assert!(
                elem.attrs.iter().any(|a| a.name.local.as_ref() == "style"),
                "style attribute should be set"
            );
        });
    }

    #[test]
    fn set_property_function_stores_handler() {
        let (_, handlers, bridge, _rt, ctx) = setup();
        ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let node_id: usize = ctx.eval("__ox_createElement('button')").unwrap();
            let _: Value = ctx
                .eval(format!("__ox_setProperty({node_id}, 'onClick', () => 42)"))
                .unwrap();
            let map = handlers.borrow();
            assert!(
                map.contains_key(&(node_id, "click".to_string())),
                "handler must be stored under 'click'"
            );
        });
        cleanup(&handlers, &ctx); // free Persistents before ctx/rt drop
    }

    #[test]
    fn set_property_on_colon_event() {
        let (_, handlers, bridge, _rt, ctx) = setup();
        ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let node_id: usize = ctx.eval("__ox_createElement('button')").unwrap();
            let _: Value = ctx
                .eval(format!(
                    "__ox_setProperty({node_id}, 'on:mousedown', () => 0)"
                ))
                .unwrap();
            let map = handlers.borrow();
            assert!(map.contains_key(&(node_id, "mousedown".to_string())));
        });
        cleanup(&handlers, &ctx);
    }

    #[test]
    fn handler_callable_via_persistent() {
        let (_, handlers, bridge, _rt, ctx) = setup();
        let node_id: usize = ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let nid: usize = ctx.eval("__ox_createElement('button')").unwrap();
            let _: Value = ctx
                .eval(format!(
                    "globalThis.__count = 0; __ox_setProperty({nid}, 'onClick', () => {{ __count++ }})"
                ))
                .unwrap();
            nid
        });

        // Call via a clone so we don't consume the Persistent before cleanup.
        let persistent = handlers
            .borrow()
            .get(&(node_id, "click".to_string()))
            .cloned()
            .unwrap();
        ctx.with(|ctx| {
            let func = persistent.clone().restore(&ctx).unwrap();
            func.call::<(), ()>(()).unwrap();
        });
        let count: i32 = ctx.with(|ctx| ctx.eval("__count").unwrap());
        assert_eq!(count, 1, "handler should have run");

        // Free the local clone and the map entry while ctx/rt are alive.
        drop(persistent);
        cleanup(&handlers, &ctx);
    }

    #[test]
    fn set_property_null_removes_handler() {
        let (_, handlers, bridge, _rt, ctx) = setup();
        ctx.with(|ctx| {
            bridge.install(ctx.clone()).unwrap();
            let node_id: usize = ctx.eval("__ox_createElement('button')").unwrap();
            let _: Value = ctx
                .eval(format!("__ox_setProperty({node_id}, 'onClick', () => 1)"))
                .unwrap();
            assert!(
                handlers
                    .borrow()
                    .contains_key(&(node_id, "click".to_string()))
            );
            // Passing null removes the handler (and drops the Persistent inside ctx.with).
            let _: Value = ctx
                .eval(format!("__ox_setProperty({node_id}, 'onClick', null)"))
                .unwrap();
            assert!(
                !handlers
                    .borrow()
                    .contains_key(&(node_id, "click".to_string())),
                "handler should be removed after null"
            );
        });
        // Map is already empty; no Persistents to free.
    }
}
