//! Translation from winit's `Key`/`text` pair into the string values that
//! solite's `KeyboardEvent.key` expects (`"Tab"`, `"Enter"`, `"a"`, `" "`,
//! `"Escape"`, …).
//!
//! Used by every winit-driven example so behavior stays consistent.

use winit::keyboard::{Key, NamedKey};

/// Convert winit's logical key (+ optional `text`) into the string value
/// that should land in [`solite::KeyboardEvent::key`].
///
/// **Named keys MUST be checked before `text`.** winit fills `text` with
/// control characters for them (Tab → `"\t"`, Enter → `"\r"`, Escape →
/// `"\x1b"`, Backspace → `"\u{8}"`). The browser
/// [`KeyboardEvent.key`](https://developer.mozilla.org/en-US/docs/Web/API/UI_Events/Keyboard_event_key_values)
/// contract, which `Instance::dispatch_key` matches against (`"Tab"`,
/// `"Enter"`, `"Escape"`, …), needs the WHATWG name. Letting `text` win
/// would silently mis-route Tab navigation and word-jump shortcuts.
#[allow(dead_code)]
pub fn key_to_string(logical_key: &Key, text: Option<&str>) -> String {
    if let Key::Named(named) = logical_key {
        return match named {
            // Space is the one named key the browser spec emits as the
            // literal character rather than its `NamedKey::Space` Debug
            // rendering.
            NamedKey::Space => " ".to_string(),
            _ => format!("{named:?}"),
        };
    }

    if let Some(text) = text.filter(|text| !text.is_empty()) {
        return text.to_string();
    }

    match logical_key {
        Key::Character(text) => text.to_string(),
        Key::Named(named) => format!("{named:?}"),
        Key::Unidentified(_) => "Unidentified".to_string(),
        Key::Dead(Some(c)) => c.to_string(),
        Key::Dead(None) => String::new(),
    }
}
