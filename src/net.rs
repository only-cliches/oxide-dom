//! Network/resource provider used by blitz-dom to fetch images, stylesheets,
//! and `@font-face` files.
//!
//! Blitz's `BaseDocument` defers all resource fetching to a `NetProvider`. By
//! default solite installs [`SoliteNetProvider`], which serves three things
//! synchronously:
//!
//! 1. **In-memory registered URLs** — `register(url, bytes)` keeps a byte blob
//!    under any URL the provider should answer for. Used by
//!    [`Instance::register_font_bytes`](crate::Instance::register_font_bytes)
//!    to expose a `.ttf`/`.otf` file under `solite-font://...` so the engine's
//!    own `@font-face` fetch path picks it up.
//! 2. **`file://` URLs** — the file is read off disk in the calling thread.
//! 3. **`data:` URLs** — decoded inline.
//! 4. **`http` / `https` URLs** — fetched synchronously with a short timeout.
//!
//! After every fetch the provider records a [`FetchEvent`] in its outbox so
//! the [`Instance`](crate::Instance) can dispatch `load`/`error` events to JS
//! `<img>` handlers without poking at blitz's private pending-images map.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use blitz_traits::net::{Bytes, NetHandler, NetProvider, Request};

/// Outcome of a single fetch attempt, recorded so the [`Instance`] can
/// translate it into a JS `load`/`error` event.
#[derive(Debug, Clone)]
pub(crate) struct FetchEvent {
    pub resolved_url: String,
    pub ok: bool,
    pub error: Option<String>,
}

#[derive(Default)]
struct ProviderState {
    /// In-memory registered resources keyed by URL string.
    registered: HashMap<String, Bytes>,
    /// Outbox drained by `Instance::tick()`.
    outbox: VecDeque<FetchEvent>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(err) => err.into_inner(),
    }
}

/// Synchronous resource provider for solite. Cheap to clone (shared inner
/// state). See module docs.
#[derive(Clone, Default)]
pub(crate) struct SoliteNetProvider {
    inner: Arc<Mutex<ProviderState>>,
}

impl SoliteNetProvider {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register raw bytes that the provider should return for `url`.
    ///
    /// Used by font registration to make `.ttf`/`.otf` bytes fetchable via
    /// a CSS `@font-face` `src: url("solite-font://…")` declaration.
    pub(crate) fn register(&self, url: impl Into<String>, bytes: impl Into<Bytes>) {
        let mut state = lock(&self.inner);
        state.registered.insert(url.into(), bytes.into());
    }

    /// Drain pending fetch events. Called from `Instance::tick()`.
    pub(crate) fn drain_events(&self) -> Vec<FetchEvent> {
        let mut state = lock(&self.inner);
        state.outbox.drain(..).collect()
    }

    fn record_with_error(&self, resolved_url: String, ok: bool, error: Option<String>) {
        lock(&self.inner).outbox.push_back(FetchEvent {
            resolved_url,
            ok,
            error,
        });
    }

    fn report_success(&self, resolved_url: String, bytes: Bytes, handler: Box<dyn NetHandler>) {
        handler.bytes(resolved_url.clone(), bytes);
        self.record_with_error(resolved_url, true, None);
    }

    fn report_failure(
        &self,
        resolved_url: String,
        handler: Box<dyn NetHandler>,
        error: impl ToString,
    ) {
        handler.bytes(resolved_url.clone(), Bytes::new());
        self.record_with_error(resolved_url, false, Some(error.to_string()));
    }

    fn fetch_http(&self, resolved_url: &str, handler: Box<dyn NetHandler>) {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(3))
            .build();
        let Ok(client) = client else {
            self.report_failure(
                resolved_url.to_string(),
                handler,
                "failed to initialize HTTP client",
            );
            return;
        };

        let response = client.get(resolved_url).send();
        let Ok(response) = response else {
            self.report_failure(resolved_url.to_string(), handler, response.unwrap_err());
            return;
        };

        let response = response.error_for_status();
        let mut response = match response {
            Ok(response) => response,
            Err(error) => {
                self.report_failure(resolved_url.to_string(), handler, error);
                return;
            }
        };

        let mut bytes = Vec::new();
        if let Err(error) = response.read_to_end(&mut bytes) {
            self.report_failure(resolved_url.to_string(), handler, error);
            return;
        }

        self.report_success(resolved_url.to_string(), bytes.into(), handler);
    }

    fn lookup_registered(&self, url: &str) -> Option<Bytes> {
        lock(&self.inner).registered.get(url).cloned()
    }
}

impl NetProvider for SoliteNetProvider {
    fn fetch(&self, _doc_id: usize, request: Request, handler: Box<dyn NetHandler>) {
        let resolved = request.url.as_str().to_string();

        // 1. Inline-registered bytes (font registration, test fixtures).
        if let Some(bytes) = self.lookup_registered(&resolved) {
            self.report_success(resolved, bytes, handler);
            return;
        }

        // 2. Scheme-specific synchronous fetch.
        match request.url.scheme() {
            "file" => match request.url.to_file_path() {
                Ok(path) => match read_file(&path) {
                    Ok(bytes) => {
                        self.report_success(resolved.clone(), Bytes::from(bytes), handler);
                    }
                    Err(_) => {
                        self.report_failure(resolved.clone(), handler, "failed to read file URL");
                    }
                },
                Err(()) => {
                    self.report_failure(resolved.clone(), handler, "invalid file URL");
                }
            },
            "data" => match data_url::DataUrl::process(&resolved) {
                Ok(data_url) => match data_url.decode_to_vec() {
                    Ok((bytes, _)) => {
                        self.report_success(resolved.clone(), Bytes::from(bytes), handler);
                    }
                    Err(_) => {
                        self.report_failure(resolved.clone(), handler, "invalid data URL payload");
                    }
                },
                Err(_) => {
                    self.report_failure(resolved.clone(), handler, "invalid data URL");
                }
            },
            "http" | "https" => self.fetch_http(&resolved, handler),
            _ => self.report_failure(
                resolved.clone(),
                handler,
                format!("unsupported URL scheme: {}", request.url.scheme()),
            ),
        }
    }
}

fn read_file(path: &PathBuf) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Compute a sensible default base URL for resolving relative `<img src>` /
/// `url(...)` paths. Falls back to `file:///` if the current dir is unknown.
pub(crate) fn default_base_url() -> String {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
    url::Url::from_directory_path(&cwd)
        .map(|u| u.to_string())
        .unwrap_or_else(|_| "file:///".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::sync::Mutex;

    /// Simple sink so we can assert on the bytes the NetProvider hands to
    /// the inner handler.
    struct Sink {
        capture: Arc<Mutex<Option<(String, Vec<u8>)>>>,
    }
    impl NetHandler for Sink {
        fn bytes(self: Box<Self>, resolved_url: String, bytes: Bytes) {
            *self.capture.lock().unwrap() = Some((resolved_url, bytes.to_vec()));
        }
    }

    fn boxed_sink() -> (Box<Sink>, Arc<Mutex<Option<(String, Vec<u8>)>>>) {
        let capture = Arc::new(Mutex::new(None));
        (
            Box::new(Sink {
                capture: Arc::clone(&capture),
            }),
            capture,
        )
    }

    fn make_request(url: &str) -> Request {
        Request::get(url::Url::parse(url).expect("valid url"))
    }

    fn run_http_server(body: &'static [u8], status: u16) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local HTTP listener");
        let port = listener.local_addr().expect("local_addr").port();

        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut request = [0u8; 512];
                let _ = stream.read(&mut request);

                let status_text = if status == 404 { "Not Found" } else { "OK" };
                let response = format!(
                    "HTTP/1.1 {status} {status_text}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body);
            }
        });

        port
    }

    #[test]
    fn registered_url_returns_registered_bytes_and_records_success() {
        let provider = SoliteNetProvider::new();
        provider.register("solite-font://test", b"PAYLOAD".to_vec());

        let (handler, captured) = boxed_sink();
        provider.fetch(0, make_request("solite-font://test"), handler);

        let (url, bytes) = captured.lock().unwrap().take().expect("handler received");
        assert_eq!(url, "solite-font://test");
        assert_eq!(bytes, b"PAYLOAD");

        let events = provider.drain_events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].resolved_url, "solite-font://test");
        assert!(events[0].ok);
    }

    #[test]
    fn missing_file_url_returns_empty_bytes_and_records_failure() {
        let provider = SoliteNetProvider::new();
        let (handler, captured) = boxed_sink();
        provider.fetch(
            0,
            make_request("file:///tmp/this-file-definitely-does-not-exist-solite.png"),
            handler,
        );

        let (_url, bytes) = captured.lock().unwrap().take().expect("handler received");
        assert!(bytes.is_empty(), "expected empty bytes on file-not-found");

        let events = provider.drain_events();
        assert_eq!(events.len(), 1);
        assert!(!events[0].ok);
    }

    #[test]
    fn data_url_decodes_to_payload_and_records_success() {
        let provider = SoliteNetProvider::new();
        let (handler, captured) = boxed_sink();
        // base64("hello") = aGVsbG8=
        provider.fetch(
            0,
            make_request("data:application/octet-stream;base64,aGVsbG8="),
            handler,
        );
        let (_url, bytes) = captured.lock().unwrap().take().expect("handler received");
        assert_eq!(bytes, b"hello");
        let events = provider.drain_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].ok);
    }

    #[test]
    fn http_scheme_fetches_body_and_records_success() {
        let provider = SoliteNetProvider::new();
        let (handler, captured) = boxed_sink();
        let port = run_http_server(b"HTTP-PAYLOAD", 200);
        provider.fetch(
            0,
            make_request(&format!("http://127.0.0.1:{port}/img.png")),
            handler,
        );

        let (_url, bytes) = captured.lock().unwrap().take().expect("handler received");
        assert_eq!(bytes, b"HTTP-PAYLOAD");

        let events = provider.drain_events();
        assert_eq!(events.len(), 1);
        assert!(events[0].ok);
        assert_eq!(events[0].error, None);
    }

    #[test]
    fn http_404_is_reported_as_failure() {
        let provider = SoliteNetProvider::new();
        let (handler, captured) = boxed_sink();
        let port = run_http_server(b"missing", 404);
        provider.fetch(
            0,
            make_request(&format!("http://127.0.0.1:{port}/missing")),
            handler,
        );

        let _ = captured.lock().unwrap().take().expect("handler received");

        let events = provider.drain_events();
        assert_eq!(events.len(), 1);
        assert!(!events[0].ok);
        assert!(
            events[0]
                .error
                .as_ref()
                .expect("failure should include status")
                .contains("404")
        );
    }

    #[test]
    fn unknown_scheme_records_supported_error() {
        let provider = SoliteNetProvider::new();
        let (handler, captured) = boxed_sink();
        provider.fetch(0, make_request("ftp://example.com/x.png"), handler);

        let _ = captured.lock().unwrap().take().expect("handler received");

        let events = provider.drain_events();
        assert_eq!(events.len(), 1);
        assert!(!events[0].ok);
        assert_eq!(
            events[0]
                .error
                .as_ref()
                .expect("failure should include error"),
            "unsupported URL scheme: ftp"
        );
    }

    #[test]
    fn drain_events_is_consumed_once() {
        let provider = SoliteNetProvider::new();
        provider.register("solite-font://x", vec![1u8, 2, 3]);
        let (handler, _) = boxed_sink();
        provider.fetch(0, make_request("solite-font://x"), handler);
        assert_eq!(provider.drain_events().len(), 1);
        assert!(provider.drain_events().is_empty());
    }
}
