//! Tracks `<img>` lifecycle so the JS bridge can dispatch `load` / `error`
//! events on them.
//!
//! Blitz-dom fetches the image bytes through [`SoliteNetProvider`](crate::net),
//! then applies the decoded image to the node on the next `resolve()` cycle.
//! Neither layer signals JS, so this module bridges the gap:
//!
//! 1. The bridge calls [`ImgWatcher::register`] each time a non-empty `src`
//!    attribute lands on an `<img>` element. The watcher records the node id
//!    plus the URL string the document resolved it to.
//! 2. Each tick the [`Instance`](crate::Instance) calls
//!    [`ImgWatcher::collect_pending`], which inspects every watched node's
//!    [`SpecialElementData`] (set to `Image` by blitz once the bytes are
//!    decoded) and emits a [`ImgEvent::Load`]. URLs whose fetch failed —
//!    reported via [`crate::net::FetchEvent`] — produce an [`ImgEvent::Error`].
//!    Each transition is emitted at most once per `(node_id, src)` pair.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use blitz_dom::BaseDocument;
use blitz_dom::node::SpecialElementData;

use crate::net::FetchEvent;

/// State per `<img>` node.
#[derive(Debug, Clone, PartialEq)]
struct ImgEntry {
    /// Resolved URL the node is currently waiting on, or empty if `src` was
    /// cleared. `resolved_url` is the same string blitz uses in its image
    /// cache, so it can be matched against [`FetchEvent::resolved_url`].
    resolved_url: String,
    /// Whether we've already dispatched a `load` event for the
    /// `(node_id, resolved_url)` pair.
    loaded_dispatched: bool,
    /// Whether we've already dispatched an `error` event.
    errored_dispatched: bool,
}

/// Event emitted to the JS layer for an image lifecycle transition.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ImgEvent {
    Load { node_id: usize },
    Error { node_id: usize },
}

#[derive(Default)]
pub(crate) struct ImgWatcher {
    /// `node_id` → current entry. Cleared if the node is dropped (best-effort:
    /// stale ids are ignored by [`collect_pending`]).
    entries: HashMap<usize, ImgEntry>,
    /// Resolved URLs the provider reported as failed but the watcher hasn't
    /// drained yet. Holding a set is fine — only one error event per URL.
    failed_urls: HashSet<String>,
}

pub(crate) type ImgWatcherHandle = Rc<RefCell<ImgWatcher>>;

pub(crate) fn new_handle() -> ImgWatcherHandle {
    Rc::new(RefCell::new(ImgWatcher::default()))
}

impl ImgWatcher {
    /// Track `node_id` with a fresh `resolved_url`. Resets the dispatch state
    /// so the next successful load fires another `load` event (matching
    /// browser semantics for `src` mutation).
    pub(crate) fn register(&mut self, node_id: usize, resolved_url: String) {
        // If the URL hasn't changed, keep the existing dispatch flags so we
        // don't double-fire on benign reassignments.
        if let Some(existing) = self.entries.get(&node_id) {
            if existing.resolved_url == resolved_url {
                return;
            }
        }
        self.entries.insert(
            node_id,
            ImgEntry {
                resolved_url,
                loaded_dispatched: false,
                errored_dispatched: false,
            },
        );
    }

    /// Stop tracking a node — called when `src` is cleared.
    pub(crate) fn clear(&mut self, node_id: usize) {
        self.entries.remove(&node_id);
    }

    /// Absorb provider events into `failed_urls`. Successful fetches don't
    /// need to be remembered — we infer "loaded" from the node's element data
    /// directly.
    pub(crate) fn ingest_fetch_events(&mut self, events: Vec<FetchEvent>) {
        for ev in events {
            if !ev.ok {
                self.failed_urls.insert(ev.resolved_url);
            }
        }
    }

    /// Inspect each tracked `<img>` and produce one event per fresh
    /// load/error transition. The watcher remembers what it dispatched so
    /// repeat calls don't duplicate.
    pub(crate) fn collect_pending(&mut self, doc: &BaseDocument) -> Vec<ImgEvent> {
        let mut out = Vec::new();
        let mut to_remove = Vec::new();
        for (&node_id, entry) in self.entries.iter_mut() {
            let Some(node) = doc.get_node(node_id) else {
                to_remove.push(node_id);
                continue;
            };
            let Some(elem) = node.element_data() else {
                continue;
            };

            // 1. Loaded? Blitz sets `SpecialElementData::Image` when bytes
            //    decode successfully.
            let has_image = matches!(elem.special_data, SpecialElementData::Image(_));
            if has_image && !entry.loaded_dispatched {
                entry.loaded_dispatched = true;
                out.push(ImgEvent::Load { node_id });
                continue;
            }

            // 2. Errored? Provider recorded the URL as failed.
            if !entry.errored_dispatched && self.failed_urls.contains(&entry.resolved_url) {
                entry.errored_dispatched = true;
                out.push(ImgEvent::Error { node_id });
            }
        }
        for id in to_remove {
            self.entries.remove(&id);
        }
        out
    }

    /// Number of nodes currently tracked. Used by tests.
    #[cfg(test)]
    pub(crate) fn tracked_count(&self) -> usize {
        self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::FetchEvent;
    use blitz_dom::{BaseDocument, DocumentConfig, LocalName, QualName, ns};

    fn img_doc(initial_src: Option<&str>) -> (BaseDocument, usize) {
        let mut doc = BaseDocument::new(DocumentConfig::default());
        let attrs = match initial_src {
            Some(src) => vec![blitz_dom::Attribute {
                name: QualName::new(None, ns!(), LocalName::from("src")),
                value: src.into(),
            }],
            None => vec![],
        };
        let id = doc.mutate().create_element(
            QualName::new(None, ns!(html), LocalName::from("img")),
            attrs,
        );
        (doc, id)
    }

    #[test]
    fn fresh_register_starts_unflagged() {
        let mut w = ImgWatcher::default();
        w.register(7, "file:///a.png".into());
        assert_eq!(w.tracked_count(), 1);
    }

    #[test]
    fn re_registering_same_url_is_idempotent_for_dispatch_state() {
        let (doc, id) = img_doc(None);
        let mut w = ImgWatcher::default();
        w.register(id, "file:///a.png".into());
        // Pretend we already dispatched.
        w.ingest_fetch_events(vec![FetchEvent {
            resolved_url: "file:///a.png".into(),
            ok: false,
            error: None,
        }]);
        let first = w.collect_pending(&doc);
        assert_eq!(first, vec![ImgEvent::Error { node_id: id }]);
        // Re-registering the SAME url should not fire again.
        w.register(id, "file:///a.png".into());
        assert!(w.collect_pending(&doc).is_empty());
    }

    #[test]
    fn changing_url_resets_dispatch_state() {
        let (doc, id) = img_doc(None);
        let mut w = ImgWatcher::default();
        w.register(id, "file:///a.png".into());
        w.ingest_fetch_events(vec![FetchEvent {
            resolved_url: "file:///a.png".into(),
            ok: false,
            error: None,
        }]);
        assert_eq!(
            w.collect_pending(&doc),
            vec![ImgEvent::Error { node_id: id }]
        );
        // Move to a different url; previous error must not stick to it.
        w.register(id, "file:///b.png".into());
        assert!(w.collect_pending(&doc).is_empty());
        w.ingest_fetch_events(vec![FetchEvent {
            resolved_url: "file:///b.png".into(),
            ok: false,
            error: None,
        }]);
        assert_eq!(
            w.collect_pending(&doc),
            vec![ImgEvent::Error { node_id: id }]
        );
    }

    #[test]
    fn clear_removes_node_from_tracking() {
        let mut w = ImgWatcher::default();
        w.register(1, "file:///a.png".into());
        assert_eq!(w.tracked_count(), 1);
        w.clear(1);
        assert_eq!(w.tracked_count(), 0);
    }

    #[test]
    fn missing_node_id_is_dropped_silently() {
        let doc = BaseDocument::new(DocumentConfig::default());
        let mut w = ImgWatcher::default();
        // Register a node id that doesn't exist in `doc`.
        w.register(999, "file:///a.png".into());
        // collect_pending should drop the stale entry rather than panic.
        assert!(w.collect_pending(&doc).is_empty());
        assert_eq!(w.tracked_count(), 0);
    }
}
