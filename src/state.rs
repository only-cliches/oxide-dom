use serde_json::Value;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
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

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => err.into_inner(),
    }
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
            let mut v = lock(&self.inner.value);
            let mut current = &mut *v;
            set_path(&mut current, &parse_path(path), value.clone());
        }
        lock(&self.inner.patches).push((path.to_owned(), value));
        self.inner.dirty.store(true, Ordering::Release);
        if let Some(wake) = &self.inner.wake {
            wake.notify_one();
        }
    }

    /// Read the value at dot-separated `path`.
    pub fn get(&self, path: &str) -> Option<Value> {
        get_path(lock(&self.inner.value).deref(), &parse_path(path))
    }

    /// Snapshot the full state tree.
    pub fn snapshot(&self) -> Value {
        lock(&self.inner.value).clone()
    }

    /// Drain pending patches (called by `tick()`).
    pub(crate) fn drain_patches(&self) -> Vec<(String, Value)> {
        if !self.inner.dirty.load(Ordering::Acquire) {
            return Vec::new();
        }
        let patches = std::mem::take(&mut *lock(&self.inner.patches));
        self.inner.dirty.store(false, Ordering::Release);
        patches
    }

    /// Mirror a JS-side write without re-queuing it (avoids feedback loop).
    pub(crate) fn mirror_js_write(&self, path: &str, value: Value) {
        let mut v = lock(&self.inner.value);
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

    let Some((first, rest)) = path.split_first() else {
        return;
    };
    match first {
        PathSegment::Key(key) => {
            if !root.is_object() {
                *root = Value::Object(serde_json::Map::new());
            }
            let Some(obj) = root.as_object_mut() else {
                return;
            };
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
            let Some(arr) = root.as_array_mut() else {
                return;
            };
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
    fn state_set_path_matrix_covers_nested_object_and_array_forms() {
        let h = StateHandle::new(json!({}));

        h.set("counter", json!(0));
        h.set("counter", json!(17));
        h.set("meta.state", json!("ready"));
        h.set("meta.state", json!("updated"));
        h.set("rows.0", json!("first"));
        h.set("rows.1", json!("second"));
        h.set("rows.1", json!("second-overwrite"));
        h.set("meta.nested.value", json!(3));
        h.set("list.0.inner", json!(1));
        h.set("list.1.inner", json!(2));
        h.set("rows.3", json!("third"));
        h.set("root", json!({"arr":[10,20],"nested":{"flag":true}}));

        assert_eq!(h.get("counter"), Some(json!(17)));
        assert_eq!(h.get("counter"), Some(json!(17)));
        assert_eq!(h.get("meta.state"), Some(json!("updated")));
        assert_eq!(h.get("rows.1"), Some(json!("second-overwrite")));
        assert_eq!(h.get("meta.nested.value"), Some(json!(3)));
        assert_eq!(h.get("list.0.inner"), Some(json!(1)));
        assert_eq!(h.get("list.1.inner"), Some(json!(2)));
        assert_eq!(h.get("root.arr.0"), Some(json!(10)));
        assert_eq!(h.get("root.arr.1"), Some(json!(20)));
        assert_eq!(h.get("root.nested"), Some(json!({"flag": true})));
        assert_eq!(h.get("rows.2"), Some(json!(null)));
        assert_eq!(
            h.get("rows"),
            Some(json!(["first", "second-overwrite", null, "third"]))
        );
    }

    #[test]
    fn state_drain_patches_clears_dirty() {
        let h = StateHandle::new(json!({}));
        h.set("a", json!(1));
        let patches = h.drain_patches();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0], ("a".to_owned(), json!(1)));
        assert!(
            h.drain_patches().is_empty(),
            "dirty flag should clear after drain"
        );
    }

    #[test]
    fn state_drain_patches_preserves_enqueue_order() {
        let h = StateHandle::new(json!({}));
        h.set("a", json!(1));
        h.set("b", json!(2));
        h.set("a", json!(3));
        h.set("rows.0", json!("first"));

        let patches = h.drain_patches();
        assert_eq!(patches.len(), 4);
        assert_eq!(
            patches[0].0, "a",
            "set order should preserve first-write-to-root-level path 0"
        );
        assert_eq!(patches[1].0, "b");
        assert_eq!(patches[2].0, "a");
        assert_eq!(patches[3].0, "rows.0");
        assert_eq!(patches[2].1, json!(3));
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
    fn state_mirror_js_write_does_not_queue_patches() {
        let h = StateHandle::new(json!({}));
        h.mirror_js_write("js.only", json!("present"));
        h.mirror_js_write("rows.0", json!("a"));
        h.mirror_js_write("rows.1", json!("b"));
        assert_eq!(h.get("js.only"), Some(json!("present")));
        assert_eq!(h.get("rows.1"), Some(json!("b")));
        assert_eq!(
            h.drain_patches(),
            vec![],
            "JS-origin writes should not enqueue Rust patches"
        );
        h.set("rows.2", json!("c"));
        let patches = h.drain_patches();
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0], ("rows.2".to_owned(), json!("c")));
    }

    #[test]
    fn state_clone_shares_inner() {
        let h1 = StateHandle::new(json!({"n": 0}));
        let h2 = h1.clone();
        h1.set("n", json!(99));
        assert_eq!(h2.get("n"), Some(json!(99)));
    }
}
