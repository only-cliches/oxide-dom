//! Sequential focus navigation (Tab / Shift+Tab) with `tabindex` support.
//!
//! Mirrors the HTML focus algorithm:
//!
//! 1. **Default focusable**: `<input>`, `<select>`, `<button>`, `<textarea>`,
//!    and `<a href="…">`, when not disabled.
//! 2. **`tabindex="0"`** makes any element focusable in document order.
//! 3. **`tabindex="N"` with N > 0** makes the element focusable with
//!    priority order: lower positive values first, ties broken by document
//!    order.
//! 4. **`tabindex="-1"` (or any negative)** removes the element from
//!    sequential navigation; click/JS-driven focus still works (out of
//!    scope for the Tab traversal here).
//!
//! The collector returns the tab order as a flat `Vec<usize>` so
//! `Instance::focus_adjacent_control` can index into it.

use blitz_dom::BaseDocument;
use blitz_dom::node::SpecialElementData;

use crate::input::InputRegistry;
use crate::select::SelectRegistry;

/// Possible classifications for an element relative to sequential focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabSlot {
    /// `tabindex < 0` — skipped by Tab traversal.
    Skipped,
    /// Default focusable OR `tabindex == 0`. Document order.
    Natural,
    /// `tabindex > 0`. Priority value; lower first.
    Priority(i32),
}

/// Walk the document and return the sequential tab order.
///
/// Document order is established by [`BaseDocument::visit`]. Within that
/// pass we collect `(node_id, slot, doc_index)` triples, then sort:
/// `Priority(N)` first (ascending N, ties by `doc_index`), then `Natural`
/// in `doc_index` order. `Skipped` and non-focusable nodes are dropped.
pub(crate) fn collect_tab_order(
    doc: &BaseDocument,
    inputs: &InputRegistry,
    selects: &SelectRegistry,
) -> Vec<usize> {
    let inputs = inputs.borrow();
    let selects = selects.borrow();
    let mut entries: Vec<(usize, TabSlot, usize)> = Vec::new();
    let mut doc_index = 0usize;
    doc.visit(|node_id, node| {
        let Some(elem) = node.element_data() else {
            return;
        };
        let tag = elem.name.local.as_ref();

        // Attribute lookups — uses `Attribute::iter` since we don't have a
        // typed accessor for `tabindex`.
        let tabindex_attr: Option<&str> = elem
            .attrs
            .iter()
            .find(|a| a.name.local.as_ref() == "tabindex")
            .map(|a| a.value.as_ref());
        let has_disabled_attr = elem
            .attrs
            .iter()
            .any(|a| a.name.local.as_ref() == "disabled");

        let tab_value: Option<i32> = tabindex_attr.and_then(|s| s.trim().parse::<i32>().ok());

        // Default focusability: matches WHATWG sequential-focus criteria
        // narrowed to the controls solite understands.
        let is_default_focusable = match tag {
            // Inputs and selects: rely on the registered state so we honor
            // both the `disabled` attribute and JS-driven `state.disabled`.
            _ if inputs.get(&node_id).is_some_and(|s| !s.disabled()) => true,
            _ if selects.get(&node_id).is_some_and(|s| !s.disabled()) => true,
            "button" => !has_disabled_attr && !is_hidden(elem),
            "a" => elem.attrs.iter().any(|a| a.name.local.as_ref() == "href"),
            // <textarea> doesn't exist in solite yet; left out
            // intentionally rather than mismodelled.
            _ => false,
        };

        let slot = match (tab_value, is_default_focusable) {
            (Some(n), _) if n < 0 => TabSlot::Skipped,
            (Some(0), _) => TabSlot::Natural,
            (Some(n), _) if n > 0 => TabSlot::Priority(n),
            (None, true) => TabSlot::Natural,
            // tabindex not set AND not default-focusable → not in Tab order.
            _ => return,
        };

        if matches!(slot, TabSlot::Skipped) {
            doc_index += 1;
            return;
        }

        // Suppress invisible nodes: open `<option>` popups, hidden options,
        // and `display:none` style shells should not steal focus.
        if is_special_invisible(node) {
            doc_index += 1;
            return;
        }

        entries.push((node_id, slot, doc_index));
        doc_index += 1;
    });

    entries.sort_by(|a, b| match (a.1, b.1) {
        (TabSlot::Priority(x), TabSlot::Priority(y)) => x.cmp(&y).then(a.2.cmp(&b.2)),
        (TabSlot::Priority(_), _) => std::cmp::Ordering::Less,
        (_, TabSlot::Priority(_)) => std::cmp::Ordering::Greater,
        _ => a.2.cmp(&b.2),
    });

    entries.into_iter().map(|(id, _, _)| id).collect()
}

fn is_hidden(elem: &blitz_dom::ElementData) -> bool {
    elem.attrs.iter().any(|a| a.name.local.as_ref() == "hidden")
}

/// Skip synthetic stylesheet nodes and option-popup wrappers from the focus
/// order — these exist in the tree but should never receive Tab focus.
fn is_special_invisible(node: &blitz_dom::Node) -> bool {
    let Some(elem) = node.element_data() else {
        return false;
    };
    let tag = elem.name.local.as_ref();
    if matches!(tag, "style" | "script" | "head" | "title" | "link" | "meta") {
        return true;
    }
    matches!(elem.special_data, SpecialElementData::Stylesheet(_))
}
