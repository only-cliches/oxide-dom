//! Runtime custom-font registration.
//!
//! Blitz-dom already registers fonts from CSS `@font-face` rules through its
//! `NetProvider`. We piggyback on that path: bytes are stored in the
//! [`SoliteNetProvider`](crate::net) under a synthetic `solite-font://<id>` URL
//! and a tiny `@font-face` rule is added so blitz's stylesheet ingestion
//! pipeline issues the fetch, applies the family-name override, and
//! invalidates any inline text that should reflow.
//!
//! This avoids reaching into blitz's `pub(crate)` `font_ctx`, and a future
//! version of blitz that improves font-loading semantics will keep working
//! without changes here.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::net::SoliteNetProvider;

static FONT_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Result of installing a custom font: a synthetic URL plus the `@font-face`
/// CSS rule the caller should hand to blitz's stylesheet pipeline.
pub(crate) struct RegisteredFont {
    pub css: String,
}

/// Register `bytes` under a fresh `solite-font://<id>` URL on `provider` and
/// produce a `@font-face` rule that points at it.
///
/// `family` is the CSS-visible family name; CSS `font-family: '<family>'`
/// will match this font.
pub(crate) fn register(
    provider: &SoliteNetProvider,
    family: &str,
    bytes: Vec<u8>,
    format: FontFormat,
) -> RegisteredFont {
    let id = FONT_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    // `solite-font` is non-special in the URL crate, so we use a // authority
    // so it parses with a recognisable path: `solite-font://font-<id>.<ext>`.
    let url = format!("solite-font://font-{id}.{}", format.extension());
    provider.register(url.clone(), bytes);
    let escaped_family = family.replace('\'', "\\'");
    let css = format!(
        "@font-face {{ font-family: '{escaped_family}'; src: url(\"{url}\") format(\"{}\"); }}",
        format.css_format()
    );
    RegisteredFont { css }
}

/// Subset of CSS `format()` values we need to map to.
#[derive(Debug, Clone, Copy)]
pub enum FontFormat {
    Truetype,
    Opentype,
    Woff,
    Woff2,
}

impl FontFormat {
    fn extension(self) -> &'static str {
        match self {
            FontFormat::Truetype => "ttf",
            FontFormat::Opentype => "otf",
            FontFormat::Woff => "woff",
            FontFormat::Woff2 => "woff2",
        }
    }

    fn css_format(self) -> &'static str {
        match self {
            FontFormat::Truetype => "truetype",
            FontFormat::Opentype => "opentype",
            FontFormat::Woff => "woff",
            FontFormat::Woff2 => "woff2",
        }
    }

    pub(crate) fn from_path(path: &Path) -> Option<Self> {
        match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("ttf") => Some(FontFormat::Truetype),
            Some("otf") => Some(FontFormat::Opentype),
            Some("woff") => Some(FontFormat::Woff),
            Some("woff2") => Some(FontFormat::Woff2),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blitz_traits::net::NetProvider;

    fn parse_url(css: &str) -> Option<String> {
        let start = css.find("url(\"")? + "url(\"".len();
        let end = start + css[start..].find('"')?;
        Some(css[start..end].to_string())
    }

    #[test]
    fn register_returns_css_referencing_registered_url() {
        let provider = SoliteNetProvider::new();
        let registered = register(&provider, "MyFamily", vec![1u8, 2, 3], FontFormat::Truetype);
        assert!(registered.css.contains("font-family: 'MyFamily'"));
        assert!(registered.css.contains("format(\"truetype\")"));

        let url = parse_url(&registered.css).expect("url present");
        // The provider must answer for this exact URL.
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        struct Sink {
            cap: std::sync::Arc<std::sync::Mutex<Option<Vec<u8>>>>,
        }
        impl blitz_traits::net::NetHandler for Sink {
            fn bytes(self: Box<Self>, _url: String, bytes: blitz_traits::net::Bytes) {
                *self.cap.lock().unwrap() = Some(bytes.to_vec());
            }
        }
        provider.fetch(
            0,
            blitz_traits::net::Request::get(url::Url::parse(&url).expect("registered url parses")),
            Box::new(Sink {
                cap: std::sync::Arc::clone(&captured),
            }),
        );
        let bytes = captured.lock().unwrap().take().expect("handler received");
        assert_eq!(bytes, vec![1u8, 2, 3]);
    }

    #[test]
    fn from_path_handles_common_extensions() {
        assert!(matches!(
            FontFormat::from_path(Path::new("foo.ttf")),
            Some(FontFormat::Truetype)
        ));
        assert!(matches!(
            FontFormat::from_path(Path::new("foo.OTF")),
            Some(FontFormat::Opentype)
        ));
        assert!(matches!(
            FontFormat::from_path(Path::new("foo.WoFF2")),
            Some(FontFormat::Woff2)
        ));
        assert!(FontFormat::from_path(Path::new("foo.png")).is_none());
    }

    #[test]
    fn family_names_with_quotes_are_escaped() {
        let provider = SoliteNetProvider::new();
        let registered = register(&provider, "Foo's Font", vec![], FontFormat::Woff2);
        assert!(registered.css.contains("'Foo\\'s Font'"));
    }
}
