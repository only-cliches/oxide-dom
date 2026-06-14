use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

struct StateInner {
    /// Current full state snapshot (source of truth on Rust side).
    value: Mutex<Value>,
    /// Path/value patches queued by host `set()` calls, drained by `tick()`.
    patches: Mutex<Vec<(String, Value)>>,
    /// Set to true when patches are queued.
    dirty: AtomicBool,
    /// Optional wake signal for async hosts.
    wake: Option<Arc<Notify>>,
}

/// Cloneable handle to the shared reactive state.
///
/// The host can call `set` from any thread; `tick()` on the owning [`Instance`]
/// drains the patches and applies them to the JS `createStore`.
#[derive(Clone)]
pub struct StateHandle {
    inner: Arc<StateInner>,
}

impl StateHandle {
    /// Create a detached state handle without wake notifications.
    ///
    /// Primarily useful for tests and standalone state usage.
    pub fn new(initial: Value) -> Self {
        Self {
            inner: Arc::new(StateInner {
                value: Mutex::new(initial),
                patches: Mutex::new(Vec::new()),
                dirty: AtomicBool::new(false),
                wake: None,
            }),
        }
    }

    pub(crate) fn new_with_wake(initial: Value, wake: Arc<Notify>) -> Self {
        Self {
            inner: Arc::new(StateInner {
                value: Mutex::new(initial),
                patches: Mutex::new(Vec::new()),
                dirty: AtomicBool::new(false),
                wake: Some(wake),
            }),
        }
    }

    /// Write `value` at dot-separated `path`. Queues a patch for the next `tick()`.
    pub fn set(&self, path: &str, value: Value) {
        {
            let mut v = self.inner.value.lock().unwrap();
            let mut current = &mut *v;
            set_path(&mut current, &parse_path(path), value.clone());
        }
        self.inner
            .patches
            .lock()
            .unwrap()
            .push((path.to_owned(), value));
        self.inner.dirty.store(true, Ordering::Release);
        if let Some(wake) = &self.inner.wake {
            wake.notify_one();
        }
    }

    /// Read the value at dot-separated `path`.
    pub fn get(&self, path: &str) -> Option<Value> {
        get_path(&self.inner.value.lock().unwrap(), &parse_path(path))
    }

    /// Snapshot the full state tree.
    pub fn snapshot(&self) -> Value {
        self.inner.value.lock().unwrap().clone()
    }

    /// Drain pending patches (called by `tick()`).
    pub(crate) fn drain_patches(&self) -> Vec<(String, Value)> {
        if !self.inner.dirty.load(Ordering::Acquire) {
            return Vec::new();
        }
        let patches = std::mem::take(&mut *self.inner.patches.lock().unwrap());
        self.inner.dirty.store(false, Ordering::Release);
        patches
    }

    /// Mirror a JS-side write without re-queuing it (avoids feedback loop).
    pub(crate) fn mirror_js_write(&self, path: &str, value: Value) {
        let mut v = self.inner.value.lock().unwrap();
        set_path(&mut v, &parse_path(path), value);
    }
}

fn parse_path(path: &str) -> Vec<PathSegment> {
    if path.is_empty() {
        return Vec::new();
    }
    path.split('.').map(PathSegment::from).collect()
}

#[derive(Clone, Debug)]
enum PathSegment {
    Key(String),
    Index(usize),
}

impl PathSegment {
    fn from(part: &str) -> Self {
        match part.parse::<usize>() {
            Ok(index) => Self::Index(index),
            Err(_) => Self::Key(part.to_owned()),
        }
    }
}

fn set_path(root: &mut Value, path: &[PathSegment], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }

    let (first, rest) = path.split_first().unwrap();
    match first {
        PathSegment::Key(key) => {
            if !root.is_object() {
                *root = Value::Object(serde_json::Map::new());
            }
            let obj = root.as_object_mut().unwrap();
            let entry = obj.entry(key.clone()).or_insert_with(|| {
                if let Some(next) = rest.first() {
                    next.initial_node()
                } else {
                    Value::Null
                }
            });
            set_path(entry, rest, value);
        }
        PathSegment::Index(index) => {
            if !root.is_array() {
                *root = Value::Array(vec![]);
            }
            let arr = root.as_array_mut().unwrap();
            if arr.len() <= *index {
                arr.resize_with(index + 1, || Value::Null);
            }
            set_path(&mut arr[*index], rest, value);
        }
    }
}

fn get_path(root: &Value, path: &[PathSegment]) -> Option<Value> {
    if path.is_empty() {
        return Some(root.clone());
    }
    let mut current = root;
    for segment in path {
        current = match segment {
            PathSegment::Key(key) => current.get(key)?,
            PathSegment::Index(index) => current.get(index)?,
        };
    }
    Some(current.clone())
}

impl PathSegment {
    fn initial_node(&self) -> Value {
        match self {
            Self::Key(_) => Value::Object(serde_json::Map::new()),
            Self::Index(_) => Value::Array(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_set_get_roundtrip() {
        let h = StateHandle::new(json!({"count": 0}));
        h.set("count", json!(42));
        assert_eq!(h.get("count"), Some(json!(42)));
    }

    #[test]
    fn state_set_nested_and_array_path() {
        let h = StateHandle::new(json!({}));
        h.set("counter.value", json!(1));
        h.set("items.0.name", json!("first"));
        h.set("items.1", json!(2));

        assert_eq!(h.get("counter.value"), Some(json!(1)));
        assert_eq!(h.get("items.0.name"), Some(json!("first")));
        assert_eq!(h.get("items.1"), Some(json!(2)));
    }

    #[test]
    fn state_set_root_replace_and_get() {
        let h = StateHandle::new(json!({"count": 1}));
        h.set("", json!({"count": 2, "nested": {"ok": true}}));
        assert_eq!(h.get(""), Some(json!({"count": 2, "nested": {"ok": true}})));
        assert_eq!(h.get("nested.ok"), Some(json!(true)));
    }

    #[test]
    fn state_drain_patches_clears_dirty() {
        let h = StateHandle::new(json!({}));
        h.set("a", json!(1));
        let patches = h.drain_patches();
        assert_eq!(patches.len(), 1);
        let again = h.drain_patches();
        assert!(again.is_empty(), "dirty flag should clear after drain");
    }

    #[test]
    fn state_snapshot() {
        let h = StateHandle::new(json!({"x": 1, "y": 2}));
        let snap = h.snapshot();
        assert_eq!(snap["x"], 1);
    }

    #[test]
    fn state_drain_patches() {
        let h = StateHandle::new(json!({}));
        h.set("a", json!(1));
        h.set("b", json!(2));
        let patches = h.drain_patches();
        assert_eq!(patches.len(), 2);
        // Second drain is empty
        assert!(h.drain_patches().is_empty());
    }

    #[test]
    fn state_clone_shares_inner() {
        let h1 = StateHandle::new(json!({"n": 0}));
        let h2 = h1.clone();
        h1.set("n", json!(99));
        assert_eq!(h2.get("n"), Some(json!(99)));
    }
}
