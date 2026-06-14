//! Native `<input>` text-field state.
//!
//! Each input element registers an [`InputState`] in the [`InputRegistry`]
//! held by [`Instance`]. Rust owns the text value and caret position; JS
//! handlers receive `input` / `change` events with `event.value` and
//! `event.target.value` already populated, mirroring the DOM event surface.
//!
//! v1 scope: single-line text, char-level caret, no selection range, no IME,
//! no clipboard. Mouse hits position the caret at the end of the visible text
//! (refinement to per-glyph hit-testing is a follow-up).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// 500 ms gives the usual cursor blink cadence.
pub(crate) const BLINK_INTERVAL: Duration = Duration::from_millis(500);

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
    /// Read-only inputs accept focus + caret movement but ignore character
    /// input, backspace, and delete.
    readonly: bool,
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
            readonly: false,
        }
    }
}

impl InputState {
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn caret(&self) -> usize {
        self.caret_chars
    }

    pub fn caret_byte_index(&self) -> usize {
        self.byte_index_of(self.caret_chars)
    }

    pub fn len_chars(&self) -> usize {
        self.value.chars().count()
    }

    pub fn set_value(&mut self, value: impl Into<String>) {
        let value = value.into();
        if self.value == value {
            return;
        }
        self.value = value;
        self.caret_chars = self.len_chars();
        self.touch_blink_on();
    }

    pub fn set_placeholder(&mut self, placeholder: Option<String>) {
        self.placeholder = placeholder;
    }

    pub fn set_masked(&mut self, masked: bool) {
        self.masked = masked;
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        self.readonly = readonly;
    }

    pub fn readonly(&self) -> bool {
        self.readonly
    }

    /// Insert a single character at the caret. Returns true on success.
    pub fn insert(&mut self, ch: char) -> bool {
        if self.readonly {
            return false;
        }
        let byte = self.byte_index_of(self.caret_chars);
        self.value.insert(byte, ch);
        self.caret_chars += 1;
        self.touch_blink_on();
        true
    }

    /// Insert a string at the caret (used for paste / multi-char keys).
    pub fn insert_str(&mut self, s: &str) -> bool {
        if self.readonly || s.is_empty() {
            return false;
        }
        let byte = self.byte_index_of(self.caret_chars);
        self.value.insert_str(byte, s);
        self.caret_chars += s.chars().count();
        self.touch_blink_on();
        true
    }

    /// Delete the character before the caret. No-op at start of field.
    pub fn backspace(&mut self) -> bool {
        if self.readonly || self.caret_chars == 0 {
            return false;
        }
        let start = self.byte_index_of(self.caret_chars - 1);
        let end = self.byte_index_of(self.caret_chars);
        self.value.replace_range(start..end, "");
        self.caret_chars -= 1;
        self.touch_blink_on();
        true
    }

    /// Delete the character at the caret. No-op at end of field.
    pub fn delete_forward(&mut self) -> bool {
        if self.readonly || self.caret_chars >= self.len_chars() {
            return false;
        }
        let start = self.byte_index_of(self.caret_chars);
        let end = self.byte_index_of(self.caret_chars + 1);
        self.value.replace_range(start..end, "");
        self.touch_blink_on();
        true
    }

    pub fn move_left(&mut self) -> bool {
        if self.caret_chars == 0 {
            return false;
        }
        self.caret_chars -= 1;
        self.touch_blink_on();
        true
    }

    pub fn move_right(&mut self) -> bool {
        if self.caret_chars >= self.len_chars() {
            return false;
        }
        self.caret_chars += 1;
        self.touch_blink_on();
        true
    }

    pub fn move_home(&mut self) -> bool {
        if self.caret_chars == 0 {
            return false;
        }
        self.caret_chars = 0;
        self.touch_blink_on();
        true
    }

    pub fn move_end(&mut self) -> bool {
        let end = self.len_chars();
        if self.caret_chars == end {
            return false;
        }
        self.caret_chars = end;
        self.touch_blink_on();
        true
    }

    pub fn place_caret_at_end(&mut self) {
        self.caret_chars = self.len_chars();
        self.touch_blink_on();
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
        let display: String = if self.masked {
            self.value.chars().map(|_| '\u{2022}').collect() // bullet
        } else {
            self.value.clone()
        };
        if display.is_empty() {
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
}

#[cfg(test)]
impl InputState {
    /// Mutates internal blink state so the next `tick_blink()` call flips in
    /// deterministic tests.
    pub fn force_blink_for_test(&mut self, elapsed: std::time::Duration) {
        self.last_blink = std::time::Instant::now() - elapsed;
    }
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
