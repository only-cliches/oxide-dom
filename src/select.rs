//! Native `<select>` element state.
//!
//! Each select element registers a [`SelectState`] in the [`SelectRegistry`]
//! held by [`Instance`]. Rust owns the options list, selected index, and open state;
//! JS handlers receive `input` and `change` events with `event.value` populated.
//!
//! v1 scope: single-select dropdown (no multiple, no size, no optgroup, no typeahead).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

/// CSS classes used on the DOM nodes that make up an open select popup.
/// Style rules for these live in [`POPUP_UA_CSS`].
pub const POPUP_CLASS: &str = "ox-select-popup";
pub const POPUP_OPTION_CLASS: &str = "ox-select-popup-option";
pub const POPUP_OPTION_ACTIVE_CLASS: &str = "ox-active";
pub const POPUP_OPTION_SELECTED_CLASS: &str = "ox-selected";
pub const POPUP_OPTION_DISABLED_CLASS: &str = "ox-disabled";

/// User-agent stylesheet that makes select popups visible and clickable.
/// `position: relative` on `<select>` lets the absolutely-positioned popup
/// child anchor to the select's box.
pub const POPUP_UA_CSS: &str = r#"
select { position: relative; }
.ox-select-popup {
    position: absolute;
    /* top: 100% resolves to the parent's padding-box bottom — i.e. just
       above the parent's bottom border — so a naive 100% leaves the
       popup's top edge overlapping the <select>'s border. Nudge by the
       border width so the popup sits flush below the box. */
    top: calc(100% + 1px);
    left: 0;
    width: 100%;
    background: #ffffff;
    border: 1px solid #808080;
    color: #000000;
    z-index: 1000;
    box-sizing: border-box;
    padding: 4px 0;
}
.ox-select-popup-option {
    display: block;
    box-sizing: border-box;
    width: 100%;
    padding: 6px 10px;
    cursor: pointer;
    line-height: 1.4;
}
.ox-select-popup-option.ox-selected { background: #c8dcff; }
.ox-select-popup-option.ox-active { background: #b0c8ff; }
.ox-select-popup-option.ox-disabled { color: #808080; cursor: default; }
"#;

#[derive(Debug, Clone)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub disabled: bool,
    /// Mirrors the source `<option hidden>` attribute. The option keeps its
    /// slot in [`SelectState::options`] (so `selected_index` and form
    /// submission still see it), but the popup overlay skips it when
    /// mounting the dropdown — matching how browsers treat a hidden
    /// placeholder option.
    pub hidden: bool,
    #[allow(dead_code)]
    pub selected: bool,
}

impl SelectOption {
    pub fn new(value: String, label: String, disabled: bool) -> Self {
        Self {
            value,
            label,
            disabled,
            hidden: false,
            selected: false,
        }
    }

    pub fn with_hidden(mut self, hidden: bool) -> Self {
        self.hidden = hidden;
        self
    }
}

/// Per-select editable state.
#[derive(Debug, Clone, Default)]
pub struct SelectState {
    pub options: Vec<SelectOption>,
    pub selected_index: Option<usize>,
    pub disabled: bool,
    #[allow(dead_code)]
    pub name: Option<String>,
    pub open: bool,
    pub active_index: Option<usize>,
    /// Node id of the popup overlay `<div>` while the dropdown is open.
    /// `None` while closed.
    pub popup_root_id: Option<usize>,
    /// One slot per entry in `options`, in the same order. `Some(node_id)`
    /// is the popup option div for that entry; `None` means the option is
    /// hidden (no div mounted) but still occupies its slot so
    /// `selected_index` stays valid and form submission keeps seeing it.
    pub option_node_ids: Vec<Option<usize>>,
}

impl SelectState {
    pub fn value(&self) -> Option<String> {
        self.selected_index
            .and_then(|idx| self.options.get(idx))
            .map(|opt| opt.value.clone())
    }

    #[allow(dead_code)]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    #[allow(dead_code)]
    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    pub fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
    }

    pub fn selected_index(&self) -> Option<usize> {
        self.selected_index
    }

    pub fn set_selected_index(&mut self, index: Option<usize>) {
        self.selected_index = index;
    }

    /// Returns the value of the currently selected option, if any.
    pub fn selected_value(&self) -> Option<&str> {
        self.selected_index
            .and_then(|idx| self.options.get(idx))
            .map(|opt| opt.value.as_str())
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn set_open(&mut self, open: bool) {
        self.open = open;
        if !open {
            self.active_index = None;
        }
    }

    pub fn active_index(&self) -> Option<usize> {
        self.active_index
    }

    pub fn set_active_index(&mut self, index: Option<usize>) {
        self.active_index = index;
    }

    pub fn current_label(&self) -> String {
        self.selected_index
            .and_then(|idx| self.options.get(idx))
            .map(|opt| opt.label.clone())
            .unwrap_or_default()
    }

    pub fn set_options(&mut self, options: Vec<SelectOption>) {
        self.options = options;
        // Reset selected_index if it's out of bounds
        if let Some(idx) = self.selected_index {
            if idx >= self.options.len() {
                self.selected_index = None;
            }
        }
    }

    pub fn find_first_enabled(&self) -> Option<usize> {
        self.options
            .iter()
            .position(|opt| !opt.disabled)
    }

    pub fn move_selection(&mut self, direction: i32) -> bool {
        let current = self.selected_index.unwrap_or(0);
        let len = self.options.len() as i32;

        if len == 0 {
            return false;
        }

        let mut next = (current as i32 + direction).rem_euclid(len) as usize;

        // Skip disabled options
        let mut attempts = 0;
        while attempts < len as usize && self.options[next].disabled {
            next = (next as i32 + direction).rem_euclid(len) as usize;
            attempts += 1;
        }

        if !self.options[next].disabled {
            self.selected_index = Some(next);
            true
        } else {
            false
        }
    }

    pub fn jump_to_extreme(&mut self, to_end: bool) -> bool {
        if self.options.is_empty() {
            return false;
        }

        let target = if to_end {
            // Find last enabled option
            self.options
                .iter()
                .rposition(|opt| !opt.disabled)
        } else {
            // Find first enabled option
            self.options
                .iter()
                .position(|opt| !opt.disabled)
        };

        if let Some(idx) = target {
            self.selected_index = Some(idx);
            true
        } else {
            false
        }
    }
}

/// Map of node-id → SelectState, shared between the bridge (where selects are
/// created and option children are managed) and the Instance (where key events
/// are intercepted and event payloads are built).
pub(crate) type SelectRegistry = Rc<RefCell<HashMap<usize, SelectState>>>;

pub(crate) fn new_registry() -> SelectRegistry {
    Rc::new(RefCell::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_selection_wraps_around() {
        let mut state = SelectState::default();
        state.set_options(vec![
            SelectOption::new("a".into(), "Option A".into(), false),
            SelectOption::new("b".into(), "Option B".into(), false),
            SelectOption::new("c".into(), "Option C".into(), false),
        ]);
        state.selected_index = Some(2);

        assert!(state.move_selection(1));
        assert_eq!(state.selected_index, Some(0));
    }

    #[test]
    fn move_selection_skips_disabled() {
        let mut state = SelectState::default();
        state.set_options(vec![
            SelectOption::new("a".into(), "Option A".into(), false),
            SelectOption::new("b".into(), "Option B".into(), true),
            SelectOption::new("c".into(), "Option C".into(), false),
        ]);
        state.selected_index = Some(0);

        assert!(state.move_selection(1));
        assert_eq!(state.selected_index, Some(2));
    }

    #[test]
    fn jump_to_extreme() {
        let mut state = SelectState::default();
        state.set_options(vec![
            SelectOption::new("a".into(), "Option A".into(), false),
            SelectOption::new("b".into(), "Option B".into(), false),
            SelectOption::new("c".into(), "Option C".into(), false),
        ]);

        assert!(state.jump_to_extreme(true));
        assert_eq!(state.selected_index, Some(2));
        assert!(state.jump_to_extreme(false));
        assert_eq!(state.selected_index, Some(0));
    }

    #[test]
    fn value_returns_selected_option_value() {
        let mut state = SelectState::default();
        state.set_options(vec![
            SelectOption::new("val1".into(), "Label 1".into(), false),
            SelectOption::new("val2".into(), "Label 2".into(), false),
        ]);
        state.selected_index = Some(1);

        assert_eq!(state.value(), Some("val2".into()));
    }

    #[test]
    fn find_first_enabled() {
        let mut state = SelectState::default();
        state.set_options(vec![
            SelectOption::new("a".into(), "Option A".into(), true),
            SelectOption::new("b".into(), "Option B".into(), false),
            SelectOption::new("c".into(), "Option C".into(), false),
        ]);

        assert_eq!(state.find_first_enabled(), Some(1));
    }
}
