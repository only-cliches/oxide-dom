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

/// Information about a select popup needed for rendering.
#[derive(Debug, Clone)]
pub struct SelectPopupGeometry {
    pub select_id: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub options: Vec<PopupOption>,
    pub selected_index: Option<usize>,
    pub active_index: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct PopupOption {
    pub label: String,
    pub disabled: bool,
}

impl SelectPopupGeometry {
    /// Calculate the height needed for all options (with some padding).
    pub fn popup_height(&self) -> f32 {
        let option_height = 24.0; // Standard option height
        let vertical_padding = 4.0;
        (self.options.len() as f32 * option_height) + vertical_padding * 2.0
    }

    /// Get the bounds for a specific option in the popup (x, y, width, height).
    pub fn option_bounds(&self, index: usize) -> Option<(f32, f32, f32, f32)> {
        if index >= self.options.len() {
            return None;
        }
        let option_height = 24.0;
        let vertical_padding = 4.0;
        let y = self.y + vertical_padding + (index as f32 * option_height);
        Some((self.x, y, self.width, option_height))
    }

    /// Find which option is at the given coordinates, if any.
    pub fn option_at_point(&self, x: f32, y: f32) -> Option<usize> {
        if x < self.x || x > self.x + self.width {
            return None;
        }
        let option_height = 24.0;
        let vertical_padding = 4.0;
        if y < self.y + vertical_padding {
            return None;
        }
        let relative_y = y - (self.y + vertical_padding);
        let index = (relative_y / option_height) as usize;
        if index < self.options.len() {
            Some(index)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct SelectOption {
    pub value: String,
    pub label: String,
    pub disabled: bool,
    pub selected: bool,
}

impl SelectOption {
    pub fn new(value: String, label: String, disabled: bool) -> Self {
        Self {
            value,
            label,
            disabled,
            selected: false,
        }
    }
}

/// Per-select editable state.
#[derive(Debug, Clone)]
pub struct SelectState {
    pub options: Vec<SelectOption>,
    pub selected_index: Option<usize>,
    pub disabled: bool,
    pub name: Option<String>,
    pub open: bool,
    pub active_index: Option<usize>,
}

impl Default for SelectState {
    fn default() -> Self {
        Self {
            options: Vec::new(),
            selected_index: None,
            disabled: false,
            name: None,
            open: false,
            active_index: None,
        }
    }
}

impl SelectState {
    pub fn value(&self) -> Option<String> {
        self.selected_index
            .and_then(|idx| self.options.get(idx))
            .map(|opt| opt.value.clone())
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

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
