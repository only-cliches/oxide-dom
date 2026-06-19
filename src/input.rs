//! Native `<input>` text-field state.
//!
//! Each input element registers an [`InputState`] in the [`InputRegistry`]
//! held by [`Instance`]. Rust owns the text value and caret position; JS
//! handlers receive `input` / `change` events with `event.value` and
//! `event.target.value` already populated, mirroring the DOM event surface.
//!
//! v1 scope: single-line text and char-level caret/selection. Byte offsets are
//! used only at the Blitz/Parley boundary for glyph layout.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// 500 ms gives the usual cursor blink cadence.
pub(crate) const BLINK_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputType {
    Text,
    Password,
    Checkbox,
    Radio,
    Range,
    Number,
}

impl InputType {
    pub const fn is_text_like(self) -> bool {
        matches!(self, Self::Text | Self::Password | Self::Number)
    }

    pub const fn is_numeric_like(self) -> bool {
        matches!(self, Self::Number | Self::Range)
    }

    pub const fn is_checked_like(self) -> bool {
        matches!(self, Self::Checkbox | Self::Radio)
    }

    pub const fn is_range(self) -> bool {
        matches!(self, Self::Range)
    }
}

/// Per-input editable state.
#[derive(Debug, Clone)]
pub(crate) struct InputState {
    value: String,
    /// Caret position as a **character** index, not a byte offset. Stored in
    /// chars to avoid landing on a UTF-8 boundary halfway through a codepoint
    /// — the cost of a chars().count() per edit is negligible for text-input
    /// scale strings.
    caret_chars: usize,
    /// Whether the cursor is currently drawn (toggled by [`tick_blink`]).
    blink_visible: bool,
    /// Last time blink visibility flipped. Updated as a side-effect of every
    /// edit so typing always shows the caret.
    last_blink: Instant,
    /// Optional placeholder shown when the value is empty and the field is
    /// not focused. Plain string; we don't dim it visually yet — caller can
    /// theme via CSS once we render it as a separate text node.
    placeholder: Option<String>,
    /// `type="password"` masking. When true, the displayed text is `*` per
    /// codepoint; the underlying value is unchanged so events still carry
    /// the real string.
    masked: bool,
    /// Input type.
    input_type: InputType,
    /// Checkbox/radio state.
    checked: bool,
    /// Optional radio group / checkbox name.
    name: Option<String>,
    /// Range/number metadata.
    min: Option<f64>,
    max: Option<f64>,
    step: Option<f64>,
    /// Read-only inputs accept focus + caret movement but ignore character
    /// input, backspace, and delete.
    readonly: bool,
    /// Disabled inputs accept no editing and do not become focus targets.
    disabled: bool,
    /// Selection anchor as a character index. `None` means the selection is
    /// collapsed at `caret_chars`.
    selection_anchor_chars: Option<usize>,
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            value: String::new(),
            caret_chars: 0,
            blink_visible: true,
            last_blink: Instant::now(),
            placeholder: None,
            masked: false,
            input_type: InputType::Text,
            checked: false,
            name: None,
            min: None,
            max: None,
            step: None,
            readonly: false,
            disabled: false,
            selection_anchor_chars: None,
        }
    }
}

#[allow(dead_code)]
impl InputState {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn caret(&self) -> usize {
        self.caret_chars
    }

    /// Back-compat helper for layout/caret painting.
    pub fn caret_byte_index(&self) -> usize {
        self.byte_index_of(self.caret_chars)
    }

    pub fn kind(&self) -> InputType {
        self.input_type
    }

    pub fn is_text_like(&self) -> bool {
        self.input_type.is_text_like()
    }

    pub fn is_checked_like(&self) -> bool {
        self.input_type.is_checked_like()
    }

    pub fn is_range(&self) -> bool {
        self.input_type.is_range()
    }

    pub fn set_input_type(&mut self, input_type: &str) {
        let next = match input_type.to_ascii_lowercase().as_str() {
            "password" => InputType::Password,
            "checkbox" => InputType::Checkbox,
            "radio" => InputType::Radio,
            "range" => InputType::Range,
            "number" => InputType::Number,
            _ => InputType::Text,
        };

        self.input_type = next;
        self.masked = matches!(next, InputType::Password);
        self.clear_selection();
        self.caret_chars = 0;
        if next == InputType::Range {
            self.set_numeric_defaults();
        }
        self.touch_blink_on();
    }

    pub fn is_numeric_like(&self) -> bool {
        self.input_type.is_numeric_like()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
    }

    pub fn checked(&self) -> bool {
        self.checked
    }

    pub fn set_checked(&mut self, checked: bool) {
        if self.checked != checked {
            self.checked = checked;
            self.touch_blink_on();
        }
    }

    pub fn is_radio(&self) -> bool {
        matches!(self.input_type, InputType::Radio)
    }

    #[allow(dead_code)]
    pub fn is_checkbox(&self) -> bool {
        matches!(self.input_type, InputType::Checkbox)
    }

    pub fn toggle_checked(&mut self) -> bool {
        if self.disabled || self.readonly {
            return false;
        }
        let next = !self.checked;
        self.set_checked(next);
        true
    }

    pub fn set_min(&mut self, min: Option<f64>) {
        self.min = min;
        self.touch_blink_on();
    }

    pub fn set_max(&mut self, max: Option<f64>) {
        self.max = max;
        self.touch_blink_on();
    }

    pub fn set_step(&mut self, step: Option<f64>) {
        self.step = match step {
            Some(step) if step > 0.0 => Some(step),
            Some(_) => None,
            None => None,
        };
        self.touch_blink_on();
    }

    pub fn set_numeric_defaults(&mut self) {
        if self.input_type == InputType::Range {
            if self.min.is_none() {
                self.min = Some(0.0);
            }
            if self.max.is_none() {
                self.max = Some(100.0);
            }
            if self.step.is_none() {
                self.step = Some(1.0);
            }
            if self.value.is_empty() {
                self.value = InputState::format_number(0.0);
                self.caret_chars = self.value.chars().count();
            }
        }
    }

    fn normalize_number_string_for_input(value: &str) -> bool {
        if value.is_empty() {
            return true;
        }
        let mut chars = value.chars().peekable();
        if matches!(chars.peek(), Some('-')) {
            let _ = chars.next();
            if chars.peek().is_none() {
                return true;
            }
        }
        let mut seen_dot = false;
        for ch in chars {
            if ch.is_ascii_digit() {
                continue;
            }
            if ch == '.' && !seen_dot {
                seen_dot = true;
                continue;
            }
            return false;
        }
        true
    }

    fn clamp_number(&self, value: f64) -> f64 {
        let mut next = value;
        if let Some(min) = self.min {
            next = next.max(min);
        }
        if let Some(max) = self.max {
            next = next.min(max);
        }
        next
    }

    pub fn numeric_value(&self) -> Option<f64> {
        self.value.parse::<f64>().ok()
    }

    fn step_value(&self) -> f64 {
        self.step.unwrap_or(1.0)
    }

    fn effective_range_min(&self) -> Option<f64> {
        if self.input_type != InputType::Range {
            return None;
        }
        Some(self.min.unwrap_or(0.0))
    }

    fn effective_range_max(&self) -> Option<f64> {
        if self.input_type != InputType::Range {
            return None;
        }
        Some(self.max.unwrap_or(100.0))
    }

    fn format_number(value: f64) -> String {
        if value == -0.0 {
            "0".to_string()
        } else {
            value.to_string()
        }
    }

    fn numeric_value_or_default(&self, default: f64) -> f64 {
        self.numeric_value().unwrap_or(default)
    }

    #[allow(dead_code)]
    pub fn is_password(&self) -> bool {
        self.input_type == InputType::Password
    }

    /// Byte index into the **displayed** text for the current caret position.
    ///
    /// For normal inputs this equals `caret_byte_index()` (a byte offset into
    /// the raw value). For password inputs the displayed text is a sequence of
    /// bullet chars (`\u{2022}`, 3 bytes each in UTF-8), so the display byte
    /// index is `caret_chars * 3`.
    pub fn display_caret_byte_index(&self) -> usize {
        if self.input_type == InputType::Password {
            self.caret_chars * '\u{2022}'.len_utf8()
        } else {
            self.caret_byte_index()
        }
    }

    /// Set the slider value from a drag/click fraction (0.0 = min, 1.0 = max).
    /// Snaps to the configured `step`, clamps to `[min, max]`.
    pub fn set_value_from_range_fraction(&mut self, fraction: f64) -> bool {
        if self.input_type != InputType::Range {
            return false;
        }
        let min = self.effective_range_min().unwrap_or(0.0);
        let max = self.effective_range_max().unwrap_or(100.0);
        let step = self.step_value();
        let raw = min + (max - min) * fraction.clamp(0.0, 1.0);
        let stepped = (raw / step).round() * step;
        self.set_numeric_value(stepped)
    }

    pub fn set_numeric_value(&mut self, raw: f64) -> bool {
        let value = InputState::format_number(self.clamp_number(raw));
        if self.value == value {
            return false;
        }
        self.value = value;
        self.caret_chars = self.value.chars().count();
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    pub fn step_number(&mut self, direction: i8) -> bool {
        let current = self.numeric_value_or_default(0.0);
        self.set_numeric_value(current + direction as f64 * self.step_value())
    }

    pub fn move_range_to_extreme(&mut self, at_maximum: bool) -> bool {
        if self.input_type != InputType::Range {
            return false;
        }
        let target = if at_maximum {
            self.effective_range_max().unwrap_or(100.0)
        } else {
            self.effective_range_min().unwrap_or(0.0)
        };
        self.set_numeric_value(target)
    }

    pub fn insert_numeric_char(&mut self, ch: char) -> bool {
        if self.readonly || self.disabled {
            return false;
        }
        if !matches!(ch, '0'..='9' | '.' | '-') {
            return false;
        }
        self.delete_selection_inner();
        let byte_index = self.byte_index_of(self.caret_chars);
        let mut next = self.value.clone();

        if ch == '-' {
            if self.caret_chars != 0 || next.contains('-') {
                return false;
            }
            next.insert(0, '-');
        } else {
            next.insert(byte_index, ch);
        }

        if !Self::normalize_number_string_for_input(&next) {
            return false;
        }

        self.value = next;
        self.caret_chars += 1;
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    #[allow(dead_code)]
    pub fn commit_numeric_preview(&mut self, preview: String) -> bool {
        if self.readonly || self.disabled {
            return false;
        }
        if !Self::normalize_number_string_for_input(&preview) {
            return false;
        }
        if self.value == preview {
            return false;
        }
        self.value = preview;
        self.caret_chars = self.value.chars().count();
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    #[allow(dead_code)]
    pub fn set_numeric_value_from_text(&mut self, text: &str) -> bool {
        if self.readonly || self.disabled {
            return false;
        }
        let value = text.trim();
        if value.is_empty() {
            return false;
        }
        if Self::normalize_number_string_for_input(value) {
            if let Some(parsed) = value.parse::<f64>().ok() {
                return self.set_numeric_value(parsed);
            }
        }
        if self.value != value {
            self.value = value.to_string();
            self.caret_chars = self.value.chars().count();
            self.clear_selection();
            self.touch_blink_on();
            true
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn update_range_value(&mut self, raw: &str) -> bool {
        if self.input_type == InputType::Range {
            self.set_numeric_value_from_text(raw)
        } else {
            false
        }
    }

    #[allow(dead_code)]
    pub fn insert_numeric_str(&mut self, text: &str) -> bool {
        if self.readonly || self.disabled {
            return false;
        }
        let mut preview = self.value.clone();
        self.delete_selection_inner();
        let byte_index = self.byte_index_of(self.caret_chars);
        preview.insert_str(byte_index, text);
        if !Self::normalize_number_string_for_input(&preview) {
            return false;
        }
        self.value.insert_str(byte_index, text);
        self.caret_chars += text.chars().count();
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    pub fn selection_start(&self) -> usize {
        let (start, _) = self.selection_range();
        start
    }

    pub fn selection_end(&self) -> usize {
        let (_, end) = self.selection_range();
        end
    }

    #[allow(dead_code)]
    pub fn selection_byte_range(&self) -> (usize, usize) {
        let (start, end) = self.selection_range();
        (self.byte_index_of(start), self.byte_index_of(end))
    }

    pub fn has_selection(&self) -> bool {
        self.selection_anchor_chars
            .is_some_and(|anchor| anchor != self.caret_chars)
    }

    #[allow(dead_code)]
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection_range();
        (start != end)
            .then(|| self.value[self.byte_index_of(start)..self.byte_index_of(end)].to_string())
    }

    pub fn len_chars(&self) -> usize {
        self.value.chars().count()
    }

    pub fn set_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        if self.value == value {
            return;
        }
        match self.input_type {
            InputType::Checkbox | InputType::Radio => {
                if value.is_empty() {
                    self.value = String::new();
                } else {
                    self.value = value;
                }
            }
            InputType::Range | InputType::Number => {
                self.value = if Self::normalize_number_string_for_input(&value) {
                    if value.is_empty() {
                        value
                    } else {
                        self.clamp_number(value.parse().unwrap_or(0.0)).to_string()
                    }
                } else {
                    value
                };
                self.caret_chars = self.value.chars().count();
            }
            _ => {
                self.value = value;
            }
        }
        if !self.input_type.is_text_like() {
            self.selection_anchor_chars = None;
        } else {
            self.caret_chars = self.len_chars();
        }
        self.clear_selection();
        self.touch_blink_on();
    }

    pub fn set_placeholder(&mut self, placeholder: Option<String>) {
        self.placeholder = placeholder;
    }

    #[allow(dead_code)]
    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }

    #[allow(dead_code)]
    pub fn readonly(&self) -> bool {
        self.readonly
    }

    pub fn set_disabled(&mut self, disabled: bool) {
        self.disabled = disabled;
        if self.disabled {
            self.clear_selection();
            self.caret_chars = 0;
        }
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }

    /// Insert a single character at the caret. Returns true on success.
    pub fn insert(&mut self, ch: char) -> bool {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        self.delete_selection_inner();
        let byte = self.byte_index_of(self.caret_chars);
        self.value.insert(byte, ch);
        self.caret_chars += 1;
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    /// Insert a string at the caret (used for paste / multi-char keys).
    pub fn insert_str(&mut self, s: &str) -> bool {
        if self.readonly || self.disabled || !self.input_type.is_text_like() || s.is_empty() {
            return false;
        }
        self.delete_selection_inner();
        let byte = self.byte_index_of(self.caret_chars);
        self.value.insert_str(byte, s);
        self.caret_chars += s.chars().count();
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    /// Delete the character before the caret. No-op at start of field.
    pub fn backspace(&mut self) -> bool {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if self.delete_selection_inner() {
            self.touch_blink_on();
            return true;
        }
        if self.caret_chars == 0 {
            return false;
        }
        let start = self.byte_index_of(self.caret_chars - 1);
        let end = self.byte_index_of(self.caret_chars);
        self.value.replace_range(start..end, "");
        self.caret_chars -= 1;
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    /// Delete the character at the caret. No-op at end of field.
    pub fn delete_forward(&mut self) -> bool {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if self.delete_selection_inner() {
            self.touch_blink_on();
            return true;
        }
        if self.caret_chars >= self.len_chars() {
            return false;
        }
        let start = self.byte_index_of(self.caret_chars);
        let end = self.byte_index_of(self.caret_chars + 1);
        self.value.replace_range(start..end, "");
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    pub fn move_left(&mut self) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if self.has_selection() {
            self.caret_chars = self.selection_start();
            self.clear_selection();
            self.touch_blink_on();
            return true;
        }
        self.move_left_extending(false)
    }

    pub fn move_left_extending(&mut self, extend: bool) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if !extend && self.has_selection() {
            return self.move_left();
        }
        if self.caret_chars == 0 {
            return false;
        }
        self.prepare_selection_extension(extend);
        self.caret_chars -= 1;
        if !extend {
            self.clear_selection();
        }
        self.touch_blink_on();
        true
    }

    pub fn move_right(&mut self) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if self.has_selection() {
            self.caret_chars = self.selection_end();
            self.clear_selection();
            self.touch_blink_on();
            return true;
        }
        self.move_right_extending(false)
    }

    pub fn move_right_extending(&mut self, extend: bool) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if !extend && self.has_selection() {
            return self.move_right();
        }
        if self.caret_chars >= self.len_chars() {
            return false;
        }
        self.prepare_selection_extension(extend);
        self.caret_chars += 1;
        if !extend {
            self.clear_selection();
        }
        self.touch_blink_on();
        true
    }

    /// Move the caret left by one word, optionally extending the selection.
    /// Browser-style: first skip non-word chars to the left, then skip word
    /// chars, landing at the start of the previous word (or string start).
    pub fn move_word_left_extending(&mut self, extend: bool) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        let chars: Vec<char> = self.value.chars().collect();
        let mut c = self.caret_chars;
        if c == 0 {
            // Even at the left edge, collapsing an existing selection counts
            // as movement so Shift-Ctrl-Left at start behaves consistently.
            if !extend && self.has_selection() {
                self.clear_selection();
                self.touch_blink_on();
                return true;
            }
            return false;
        }
        while c > 0 && !is_word_char(chars[c - 1]) {
            c -= 1;
        }
        while c > 0 && is_word_char(chars[c - 1]) {
            c -= 1;
        }
        if c == self.caret_chars && (!extend || !self.has_selection()) {
            return false;
        }
        self.prepare_selection_extension(extend);
        self.caret_chars = c;
        if !extend {
            self.clear_selection();
        }
        self.touch_blink_on();
        true
    }

    /// Move the caret right by one word, optionally extending the selection.
    /// Browser-style: skip non-word chars to the right, then skip word
    /// chars, landing past the end of the next word (or string end).
    pub fn move_word_right_extending(&mut self, extend: bool) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        let chars: Vec<char> = self.value.chars().collect();
        let n = chars.len();
        let mut c = self.caret_chars;
        if c >= n {
            if !extend && self.has_selection() {
                self.clear_selection();
                self.touch_blink_on();
                return true;
            }
            return false;
        }
        while c < n && !is_word_char(chars[c]) {
            c += 1;
        }
        while c < n && is_word_char(chars[c]) {
            c += 1;
        }
        if c == self.caret_chars && (!extend || !self.has_selection()) {
            return false;
        }
        self.prepare_selection_extension(extend);
        self.caret_chars = c;
        if !extend {
            self.clear_selection();
        }
        self.touch_blink_on();
        true
    }

    /// Ctrl+Backspace: delete the word to the left of the caret. If there's a
    /// selection, delete it instead (matching browsers).
    pub fn delete_word_left(&mut self) -> bool {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if self.delete_selection_inner() {
            self.touch_blink_on();
            return true;
        }
        // Compute the target by re-using the word-left algorithm on a
        // collapsed caret.
        let chars: Vec<char> = self.value.chars().collect();
        let mut c = self.caret_chars;
        let start = c;
        while c > 0 && !is_word_char(chars[c - 1]) {
            c -= 1;
        }
        while c > 0 && is_word_char(chars[c - 1]) {
            c -= 1;
        }
        if c == start {
            return false;
        }
        let start_byte = self.byte_index_of(c);
        let end_byte = self.byte_index_of(start);
        self.value.replace_range(start_byte..end_byte, "");
        self.caret_chars = c;
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    /// Ctrl+Delete: delete the word to the right of the caret. If there's a
    /// selection, delete it instead (matching browsers).
    pub fn delete_word_right(&mut self) -> bool {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if self.delete_selection_inner() {
            self.touch_blink_on();
            return true;
        }
        let chars: Vec<char> = self.value.chars().collect();
        let n = chars.len();
        let start = self.caret_chars;
        let mut c = start;
        while c < n && !is_word_char(chars[c]) {
            c += 1;
        }
        while c < n && is_word_char(chars[c]) {
            c += 1;
        }
        if c == start {
            return false;
        }
        let start_byte = self.byte_index_of(start);
        let end_byte = self.byte_index_of(c);
        self.value.replace_range(start_byte..end_byte, "");
        // Caret stays put — text to the right of it was removed.
        self.clear_selection();
        self.touch_blink_on();
        true
    }

    pub fn move_home(&mut self) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        self.move_home_extending(false)
    }

    pub fn move_home_extending(&mut self, extend: bool) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if self.caret_chars == 0 && (!extend || !self.has_selection()) {
            return false;
        }
        self.prepare_selection_extension(extend);
        self.caret_chars = 0;
        if !extend {
            self.clear_selection();
        }
        self.touch_blink_on();
        true
    }

    pub fn move_end(&mut self) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        self.move_end_extending(false)
    }

    pub fn move_end_extending(&mut self, extend: bool) -> bool {
        if self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        let end = self.len_chars();
        if self.caret_chars == end && (!extend || !self.has_selection()) {
            return false;
        }
        self.prepare_selection_extension(extend);
        self.caret_chars = end;
        if !extend {
            self.clear_selection();
        }
        self.touch_blink_on();
        true
    }

    pub fn place_caret_at_end(&mut self) {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return;
        }
        self.caret_chars = self.len_chars();
        self.clear_selection();
        self.touch_blink_on();
    }

    pub fn place_caret(&mut self, char_idx: usize) {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return;
        }
        self.caret_chars = char_idx.min(self.len_chars());
        self.clear_selection();
        self.touch_blink_on();
    }

    pub fn set_selection(&mut self, anchor: usize, focus: usize) {
        if !self.input_type.is_text_like() {
            return;
        }
        let len = self.len_chars();
        let anchor = anchor.min(len);
        let focus = focus.min(len);
        self.caret_chars = focus;
        self.selection_anchor_chars = (anchor != focus).then_some(anchor);
        self.touch_blink_on();
    }

    pub fn select_all(&mut self) -> bool {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        let len = self.len_chars();
        if len == 0 || (self.selection_anchor_chars == Some(0) && self.caret_chars == len) {
            return false;
        }
        self.selection_anchor_chars = Some(0);
        self.caret_chars = len;
        self.touch_blink_on();
        true
    }

    pub fn delete_selection(&mut self) -> bool {
        if self.readonly || self.disabled || !self.input_type.is_text_like() {
            return false;
        }
        if self.delete_selection_inner() {
            self.touch_blink_on();
            true
        } else {
            false
        }
    }

    pub fn char_index_for_byte(&self, byte_idx: usize) -> usize {
        if byte_idx >= self.value.len() {
            return self.len_chars();
        }
        self.value
            .char_indices()
            .take_while(|(idx, _)| *idx < byte_idx)
            .count()
    }

    pub fn word_range_at(&self, char_idx: usize) -> Option<(usize, usize)> {
        let chars: Vec<char> = self.value.chars().collect();
        if chars.is_empty() {
            return None;
        }

        let idx = char_idx.min(chars.len().saturating_sub(1));
        if !is_word_char(chars[idx]) {
            return Some((idx, idx + 1));
        }

        let mut start = idx;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }

        let mut end = idx + 1;
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }

        Some((start, end))
    }

    /// Advance the blink timer. Returns true if visibility flipped (caller
    /// should mark `needs_paint`).
    pub fn tick_blink(&mut self, now: Instant) -> bool {
        if now.duration_since(self.last_blink) >= BLINK_INTERVAL {
            self.blink_visible = !self.blink_visible;
            self.last_blink = now;
            true
        } else {
            false
        }
    }

    pub fn blink_visible(&self) -> bool {
        self.blink_visible
    }

    /// Absolute deadline at which the next blink toggle should fire.
    pub fn next_blink_at(&self) -> Instant {
        self.last_blink + BLINK_INTERVAL
    }

    /// Force the cursor visible. Used after edits / focus so the user sees an
    /// immediate response instead of waiting for the next blink boundary.
    fn touch_blink_on(&mut self) {
        self.blink_visible = true;
        self.last_blink = Instant::now();
    }

    /// Render the text that should appear inside the input element this
    /// frame: the value or placeholder. The caret is painted separately by
    /// the renderer so its position and blink do not depend on text layout.
    pub fn render(&self, _focused: bool) -> (String, bool) {
        let masked = self.masked || self.input_type == InputType::Password;
        let display: String = match self.input_type {
            InputType::Password => self.value.chars().map(|_| '\u{2022}').collect(),
            InputType::Text | InputType::Number => {
                if masked {
                    self.value.chars().map(|_| '\u{2022}').collect()
                } else {
                    self.value.clone()
                }
            }
            // Keep numeric text for DOM/value-sync while still rendering via
            // a custom painter to avoid visible range text.
            InputType::Range => self.value.clone(),
            InputType::Checkbox => {
                if self.checked {
                    "true".to_string()
                } else {
                    "false".to_string()
                }
            }
            InputType::Radio => {
                if self.checked {
                    "on".to_string()
                } else {
                    "off".to_string()
                }
            }
        };
        if matches!(
            self.input_type,
            InputType::Text | InputType::Password | InputType::Number
        ) && display.is_empty()
        {
            if let Some(ref ph) = self.placeholder {
                return (ph.clone(), true);
            }
        }
        (display, false)
    }

    fn byte_index_of(&self, char_idx: usize) -> usize {
        self.value
            .char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(self.value.len())
    }

    fn selection_range(&self) -> (usize, usize) {
        let anchor = self.selection_anchor_chars.unwrap_or(self.caret_chars);
        (anchor.min(self.caret_chars), anchor.max(self.caret_chars))
    }

    fn clear_selection(&mut self) {
        self.selection_anchor_chars = None;
    }

    fn prepare_selection_extension(&mut self, extend: bool) {
        if extend {
            self.selection_anchor_chars.get_or_insert(self.caret_chars);
        } else {
            self.clear_selection();
        }
    }

    fn delete_selection_inner(&mut self) -> bool {
        let (start, end) = self.selection_range();
        if start == end {
            return false;
        }
        let start_byte = self.byte_index_of(start);
        let end_byte = self.byte_index_of(end);
        self.value.replace_range(start_byte..end_byte, "");
        self.caret_chars = start;
        self.clear_selection();
        true
    }
}

#[cfg(test)]
impl InputState {
    /// Mutates internal blink state so the next `tick_blink()` call flips in
    /// deterministic tests.
    pub fn force_blink_for_test(&mut self, elapsed: std::time::Duration) {
        self.last_blink = std::time::Instant::now() - elapsed;
    }
}

/// Browser-style "word character": Unicode alphanumeric or underscore.
/// Used by Ctrl+ArrowLeft/Right / Ctrl+Backspace word navigation as well
/// as double-click word selection.
fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// Map of node-id → InputState, shared between the bridge (where inputs are
/// created and value-attribute assignments routed) and the Instance (where
/// key events are intercepted and event payloads are built).
pub(crate) type InputRegistry = Rc<RefCell<HashMap<usize, InputState>>>;

pub(crate) fn new_registry() -> InputRegistry {
    Rc::new(RefCell::new(HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_typed_chars_grow_value() {
        let mut s = InputState::default();
        assert!(s.insert('h'));
        assert!(s.insert('i'));
        assert_eq!(s.value(), "hi");
        assert_eq!(s.caret(), 2);
    }

    #[test]
    fn backspace_deletes_previous_char() {
        let mut s = InputState::default();
        s.set_value("ab");
        assert!(s.backspace());
        assert_eq!(s.value(), "a");
        assert_eq!(s.caret(), 1);
        assert!(s.backspace());
        assert_eq!(s.value(), "");
        assert!(!s.backspace(), "no-op at start of field");
    }

    #[test]
    fn arrows_clamp_at_ends() {
        let mut s = InputState::default();
        s.set_value("abc");
        assert!(!s.move_right(), "already at end");
        assert!(s.move_left());
        assert!(s.move_left());
        assert!(s.move_left());
        assert!(!s.move_left(), "already at start");
        assert_eq!(s.caret(), 0);
    }

    #[test]
    fn shift_arrows_extend_selection_and_typing_replaces_it() {
        let mut s = InputState::default();
        s.set_value("abcd");
        assert!(s.move_left_extending(true));
        assert!(s.move_left_extending(true));
        assert_eq!((s.selection_start(), s.selection_end()), (2, 4));
        assert_eq!(s.selected_text().as_deref(), Some("cd"));
        assert!(s.insert('X'));
        assert_eq!(s.value(), "abX");
        assert_eq!(s.caret(), 3);
        assert!(!s.has_selection());
    }

    #[test]
    fn delete_removes_selected_text() {
        let mut s = InputState::default();
        s.set_value("abcdef");
        s.set_selection(2, 5);
        assert!(s.delete_forward());
        assert_eq!(s.value(), "abf");
        assert_eq!(s.caret(), 2);
    }

    #[test]
    fn select_all_uses_full_char_range() {
        let mut s = InputState::default();
        s.set_value("hé");
        assert!(s.select_all());
        assert_eq!((s.selection_start(), s.selection_end()), (0, 2));
        assert_eq!(s.selection_byte_range(), (0, 3));
    }

    #[test]
    fn word_range_expands_from_char_index() {
        let mut s = InputState::default();
        s.set_value("hello world");
        assert_eq!(s.word_range_at(2), Some((0, 5)));
        assert_eq!(s.word_range_at(8), Some((6, 11)));
    }

    #[test]
    fn multibyte_caret_uses_char_boundaries() {
        let mut s = InputState::default();
        s.insert_str("héllo");
        assert_eq!(s.caret(), 5);
        s.move_left(); // caret at 4, between 'l' and 'o'
        s.backspace(); // delete 'l' at index 3
        assert_eq!(s.value(), "hélo");
        // Now delete the 'é' — which is 2 bytes — across the multibyte boundary.
        s.move_left(); // caret at 2, before 'l'
        s.backspace(); // delete 'é' at index 1
        assert_eq!(s.value(), "hlo");
        assert_eq!(s.caret(), 1);
    }

    #[test]
    fn readonly_blocks_edits_but_allows_caret() {
        let mut s = InputState::default();
        s.set_value("hi");
        s.set_readonly(true);
        assert!(!s.insert('x'));
        assert!(!s.backspace());
        assert!(s.move_left(), "caret movement is still allowed");
    }

    #[test]
    fn render_keeps_caret_out_of_text() {
        let mut s = InputState::default();
        s.set_value("ab");
        // Caret defaults to end of value after set_value.
        let (text, ph) = s.render(true);
        assert_eq!(text, "ab");
        assert!(!ph);
    }

    #[test]
    fn render_shows_placeholder_when_empty() {
        let mut s = InputState::default();
        s.set_placeholder(Some("type here".into()));
        let (text, ph) = s.render(false);
        assert_eq!(text, "type here");
        assert!(ph);
        let (text, ph) = s.render(true);
        assert_eq!(text, "type here");
        assert!(ph);
        s.insert('a');
        let (text, ph) = s.render(true);
        assert_eq!(text, "a");
        assert!(!ph);
    }

    #[test]
    fn render_masks_password_chars() {
        let mut s = InputState::default();
        s.set_value("hunter2");
        s.set_masked(true);
        let (text, _) = s.render(false);
        assert!(text.chars().all(|c| c == '\u{2022}'));
        assert_eq!(s.value(), "hunter2");
    }
}
