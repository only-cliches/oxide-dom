//! Native `<select>` element state.
//!
//! Each select element registers a [`SelectState`] in the [`SelectRegistry`]
//! held by [`Instance`]. Rust owns the options list, selected index, and open state;
//! JS handlers receive `input` and `change` events with `event.value` populated.
//!
//! v1 scope: single-select dropdown (no multiple, no size, no optgroup).
//! Type-ahead, Alt+Down to open, Alt+Up to close, and PageUp/PageDown step
//! are supported.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Maximum gap between type-ahead keystrokes before the buffer resets. Matches
/// common browser behavior (~500ms).
const TYPE_AHEAD_TIMEOUT: Duration = Duration::from_millis(500);

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
    /// Buffer of characters typed in rapid succession for type-ahead
    /// option search. Reset when [`TYPE_AHEAD_TIMEOUT`] elapses between
    /// keystrokes.
    type_ahead_buffer: String,
    type_ahead_last_at: Option<Instant>,
}

impl SelectState {
    fn is_user_selectable_option(option: &SelectOption) -> bool {
        !option.disabled && !option.hidden
    }

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

    pub fn find_index_by_value(&self, value: &str) -> Option<usize> {
        self.options.iter().position(|opt| opt.value == value)
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
            .position(Self::is_user_selectable_option)
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
        while attempts < len as usize && !Self::is_user_selectable_option(&self.options[next]) {
            next = (next as i32 + direction).rem_euclid(len) as usize;
            attempts += 1;
        }

        if Self::is_user_selectable_option(&self.options[next]) {
            self.selected_index = Some(next);
            true
        } else {
            false
        }
    }

    /// Step the selection by `delta` positions, skipping disabled/hidden
    /// options and clamping at the ends (no wrap, matching how browsers
    /// handle PageUp/PageDown on a closed select).
    pub fn step_selection(&mut self, delta: i32) -> bool {
        if self.options.is_empty() {
            return false;
        }
        let len = self.options.len() as i32;
        let current = self.selected_index.unwrap_or(0) as i32;
        let mut target = (current + delta).clamp(0, len - 1);
        // Walk toward the original direction looking for an enabled slot.
        let step = if delta >= 0 { 1 } else { -1 };
        let mut attempts = 0;
        while attempts < len && !Self::is_user_selectable_option(&self.options[target as usize]) {
            target += step;
            if target < 0 || target >= len {
                // Bounce: reverse direction so a request to "go down 10"
                // still lands on the last enabled option when the tail is
                // disabled.
                target = (current + delta).clamp(0, len - 1) - step;
                while target >= 0
                    && target < len
                    && !Self::is_user_selectable_option(&self.options[target as usize])
                {
                    target -= step;
                }
                break;
            }
            attempts += 1;
        }
        if target < 0 || target >= len {
            return false;
        }
        let target = target as usize;
        if !Self::is_user_selectable_option(&self.options[target]) {
            return false;
        }
        if self.selected_index == Some(target) {
            return false;
        }
        self.selected_index = Some(target);
        true
    }

    /// Push `ch` into the type-ahead buffer (resetting on timeout) and pick
    /// the next matching option. Returns the chosen option's index, if any.
    /// When the same single letter has been typed twice in succession we
    /// "cycle": jump to the next option starting with that letter.
    pub fn type_ahead(&mut self, ch: char, now: Instant) -> Option<usize> {
        let timed_out = self
            .type_ahead_last_at
            .is_none_or(|prev| now.duration_since(prev) > TYPE_AHEAD_TIMEOUT);
        if timed_out {
            self.type_ahead_buffer.clear();
        }
        let lower = ch.to_lowercase().next().unwrap_or(ch);
        self.type_ahead_buffer.push(lower);
        self.type_ahead_last_at = Some(now);

        let single_letter_cycle = self.type_ahead_buffer.chars().count() > 1
            && self.type_ahead_buffer.chars().all(|c| c == lower);
        // Reset the buffer to a single char when we're cycling so the same
        // key keeps advancing.
        if single_letter_cycle {
            self.type_ahead_buffer.clear();
            self.type_ahead_buffer.push(lower);
        }

        let prefix = self.type_ahead_buffer.clone();
        let start_from = self.selected_index.map(|i| i + 1).unwrap_or(0);
        let len = self.options.len();
        if len == 0 {
            return None;
        }
        // Two passes when cycling: from start_from to end, then 0..start_from.
        // When NOT cycling (multi-letter prefix), search from the start so a
        // longer prefix anchored at the same option stays consistent.
        let scan = |start: usize, len: usize, options: &[SelectOption], prefix: &str| {
            (start..len + start).find_map(|i| {
                let idx = i % len;
                let opt = &options[idx];
                if !Self::is_user_selectable_option(opt) {
                    return None;
                }
                let lower_label: String =
                    opt.label.chars().flat_map(|c| c.to_lowercase()).collect();
                lower_label.starts_with(prefix).then_some(idx)
            })
        };
        let target = if single_letter_cycle {
            scan(start_from, len, &self.options, &prefix)
        } else if prefix.chars().count() == 1 {
            scan(start_from, len, &self.options, &prefix)
        } else {
            scan(0, len, &self.options, &prefix)
        };

        if let Some(idx) = target {
            self.selected_index = Some(idx);
            self.active_index = Some(idx);
        }
        target
    }

    pub fn jump_to_extreme(&mut self, to_end: bool) -> bool {
        if self.options.is_empty() {
            return false;
        }

        let target = if to_end {
            // Find last enabled option
            self.options
                .iter()
                .rposition(Self::is_user_selectable_option)
        } else {
            // Find first enabled option
            self.options
                .iter()
                .position(Self::is_user_selectable_option)
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

    #[test]
    fn hidden_options_are_skipped_for_navigation() {
        let mut state = SelectState::default();
        state.set_options(vec![
            SelectOption::new("placeholder".into(), "Choose..".into(), true).with_hidden(true),
            SelectOption::new("a".into(), "Option A".into(), false),
            SelectOption::new("b".into(), "Option B".into(), false),
        ]);
        state.selected_index = Some(0);

        assert_eq!(state.find_first_enabled(), Some(1));
        assert!(state.move_selection(1));
        assert_eq!(state.selected_index, Some(1));
    }

    #[test]
    fn first_enabled_skips_hidden_placeholder() {
        let mut state = SelectState::default();
        state.set_options(vec![
            SelectOption::new("placeholder".into(), "Choose..".into(), false).with_hidden(true),
            SelectOption::new("a".into(), "Option A".into(), false),
        ]);

        assert_eq!(state.find_first_enabled(), Some(1));
    }
}
