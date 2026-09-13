//! Embeds the built React single-page application and serves it with a
//! history-API fallback (unknown paths return index.html). The dist directory
//! is produced by `make web-build` (Vite); a placeholder is committed so the
//! binary always builds even without a frontend build step.

use std::borrow::Cow;

use axum::body::Body;
use axum::extract::Request;
use axum::response::Response;
use http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use rust_embed::Embed;

/// The Vite build output, embedded at compile time. `include-exclude` is not
/// configured, so the whole tree ships, `.gz` siblings included; an absent or
/// empty `assets/` directory simply embeds fewer files.
#[derive(Embed)]
#[folder = "src/webui/dist/"]
struct Dist;

pub(crate) trait Files {
    /// The file's bytes, or `None` when the path names a directory or nothing.
    fn get(&self, name: &str) -> Option<Cow<'static, [u8]>>;
}

/// The embedded `dist` tree.
pub(crate) struct EmbeddedFiles;

impl Files for EmbeddedFiles {
    fn get(&self, name: &str) -> Option<Cow<'static, [u8]>> {
        Dist::get(name).map(|f| f.data)
    }
}

/// The SPA entry document, read once at startup.
fn index_html() -> Cow<'static, [u8]> {
    EmbeddedFiles
        .get("index.html")
        .expect("webui: dist/index.html is missing from the embedded assets")
}

/// Serves the embedded SPA. Requests for existing files are served directly;
/// everything else falls back to index.html so client-side routing works on
/// deep links.
///
/// Two response optimizations apply. Vite writes content-hashed filenames
/// under assets/, so those are immutable and cached for a year. The build also
/// emits a precompressed .gz sibling for text assets
/// (web/scripts/precompress.mjs); when the client accepts gzip and a sibling
/// exists it is served with Content-Encoding: gzip, and identity requests keep
/// getting the original. Both paths degrade cleanly when the .gz files are
/// absent (placeholder dist).
pub async fn handler(req: Request) -> Response {
    let path = req.uri().path().trim_start_matches('/').to_string();
    if !path.is_empty()
        && let Some(resp) = serve_file(req.headers(), req.method(), &EmbeddedFiles, &path)
    {
        return resp;
    }
    // SPA fallback: the route is client-side, so the entry document must be
    // revalidated on every navigation to pick up new asset hashes.
    let mut resp = Response::new(Body::from(index_html().into_owned()));
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    resp
}

/// Writes the embedded file at `name`, preferring a precompressed `.gz`
/// sibling for gzip-accepting clients, and reports `None` when the path
/// matched no real file.
pub(crate) fn serve_file(
    headers: &HeaderMap,
    method: &Method,
    files: &dyn Files,
    name: &str,
) -> Option<Response> {
    let identity = files.get(name)?;

    let mut out = HeaderMap::new();
    if let Some(ct) = content_type(name)
        && let Ok(v) = HeaderValue::from_str(ct)
    {
        out.insert(header::CONTENT_TYPE, v);
    }
    // Content-hashed asset names never change meaning; everything else (e.g.
    // favicons at the root) stays revalidated.
    if name.starts_with("assets/") {
        out.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("public, max-age=31536000, immutable"),
        );
    } else {
        out.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }

    let mut body = identity;
    if let Some(gz) = files.get(&format!("{name}.gz")) {
        // The response is negotiated whenever a sibling exists, so Vary must
        // be present on the identity variant too or a shared cache could pin
        // one encoding for every client.
        out.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
        if accepts_gzip(headers) {
            out.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
            body = gz;
        }
    }

    let size = body.len();
    out.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(body.into_owned())
    };
    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::OK;
    *resp.headers_mut() = out;
    Some(resp)
}

/// Reports whether the request advertises gzip support. A plain substring
/// match over Accept-Encoding is safe here: "gzip" appearing with a q=0 weight
/// is vanishingly rare from real clients, and such a client still receives a
/// well-formed gzip response it advertised by name.
pub(crate) fn accepts_gzip(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("gzip"))
}

/// The MIME type for a file name.
///
fn content_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())?;
    let builtin = match ext.as_str() {
        "avif" => Some("image/avif"),
        "css" => Some("text/css; charset=utf-8"),
        "gif" => Some("image/gif"),
        "htm" | "html" => Some("text/html; charset=utf-8"),
        "jpeg" | "jpg" => Some("image/jpeg"),
        "js" | "mjs" => Some("text/javascript; charset=utf-8"),
        "json" => Some("application/json"),
        "pdf" => Some("application/pdf"),
        "png" => Some("image/png"),
        "svg" => Some("image/svg+xml"),
        "wasm" => Some("application/wasm"),
        "webp" => Some("image/webp"),
        "xml" => Some("text/xml; charset=utf-8"),
        "ico" => Some("image/vnd.microsoft.icon"),
        "gz" => Some("application/gzip"),
        _ => None,
    };
    builtin.or_else(|| {
        mime_guess::from_path(name)
            .first_raw()
            // `first_raw` yields a `&'static str` from the crate's table.
            .map(|m| m as &'static str)
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::HashMap;

    use axum::body::to_bytes;
    use http::{HeaderValue, Request, header};

    use crate::webui::*;

    /// Issues a request through the SPA handler.
    async fn call(method: Method, uri: &str, accept_encoding: Option<&str>) -> Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(enc) = accept_encoding {
            builder = builder.header(header::ACCEPT_ENCODING, enc);
        }
        handler(builder.body(Body::empty()).unwrap()).await
    }

    /// The response body as a string.
    async fn body_string(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn spa_fallback() {
        // Root serves index.html.
        let resp = call(Method::GET, "/", None).await;
        assert_eq!(resp.status(), StatusCode::OK, "root");
        assert!(
            body_string(resp).await.contains(r#"<div id="root">"#),
            "root body"
        );

        // Unknown client-side route falls back to index.html (history API).
        let resp = call(Method::GET, "/repositories/42", None).await;
        assert_eq!(resp.status(), StatusCode::OK, "deep link");
        assert!(
            body_string(resp).await.contains(r#"<div id="root">"#),
            "deep link body"
        );
    }

    /// A filesystem with an asset, its gzip sibling, and a root file without one,
    /// so `serve_file`'s negotiation is tested independently of what the embedded
    /// dist currently contains.
    struct SyntheticFiles(HashMap<&'static str, &'static [u8]>);

    impl SyntheticFiles {
        fn new() -> SyntheticFiles {
            let mut m: HashMap<&'static str, &'static [u8]> = HashMap::new();
            m.insert("assets/app-abc123.js", b"console.log('full source')");
            m.insert("assets/app-abc123.js.gz", b"gzip-bytes");
            m.insert("favicon.svg", b"<svg/>");
            SyntheticFiles(m)
        }
    }

    impl Files for SyntheticFiles {
        fn get(&self, name: &str) -> Option<Cow<'static, [u8]>> {
            self.0.get(name).map(|b| Cow::Borrowed(*b))
        }
    }

    /// Builds the request headers `serve_file` reads.
    fn headers(accept_encoding: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(enc) = accept_encoding {
            h.insert(header::ACCEPT_ENCODING, HeaderValue::from_str(enc).unwrap());
        }
        h
    }

    #[tokio::test]
    async fn serve_file_gzip_negotiation() {
        let fsys = SyntheticFiles::new();

        // Gzip-accepting client gets the precompressed sibling.
        let resp = serve_file(
            &headers(Some("gzip, br")),
            &Method::GET,
            &fsys,
            "assets/app-abc123.js",
        )
        .expect("expected file to be served");
        assert_eq!(
            resp.headers().get(header::CONTENT_ENCODING).unwrap(),
            "gzip",
            "gzip variant not served"
        );
        assert_eq!(
            resp.headers().get(header::VARY).unwrap(),
            "Accept-Encoding",
            "Vary missing on gzip variant"
        );
        assert!(
            resp.headers()
                .get(header::CACHE_CONTROL)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("immutable"),
            "asset not immutable"
        );
        assert_eq!(
            resp.headers().get(header::CONTENT_LENGTH).unwrap(),
            "10",
            "Content-Length must be the gzip size"
        );
        assert_eq!(body_string(resp).await, "gzip-bytes");

        // Identity client gets the original, still with Vary so shared caches keep
        // the variants apart.
        let resp = serve_file(&headers(None), &Method::GET, &fsys, "assets/app-abc123.js")
            .expect("expected file to be served");
        assert!(
            resp.headers().get(header::CONTENT_ENCODING).is_none(),
            "identity variant wrong"
        );
        assert_eq!(
            resp.headers().get(header::VARY).unwrap(),
            "Accept-Encoding",
            "Vary missing on identity variant"
        );
        assert!(body_string(resp).await.contains("full source"));

        // A file with no sibling serves identity with no Vary and stays no-cache
        // at the root.
        let resp = serve_file(&headers(Some("gzip")), &Method::GET, &fsys, "favicon.svg")
            .expect("expected favicon to be served");
        assert!(
            resp.headers().get(header::VARY).is_none()
                && resp.headers().get(header::CONTENT_ENCODING).is_none(),
            "no-sibling file must not negotiate"
        );
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache",
            "root file cache-control"
        );
    }

    #[tokio::test]
    async fn serve_file_edge_cases() {
        let fsys = SyntheticFiles::new();

        // HEAD sets headers but writes no body.
        let resp = serve_file(&headers(None), &Method::HEAD, &fsys, "assets/app-abc123.js")
            .expect("expected HEAD to match the file");
        assert!(
            resp.headers().get(header::CONTENT_LENGTH).is_some(),
            "HEAD lost Content-Length"
        );
        assert_eq!(body_string(resp).await, "", "HEAD wrote a body");

        // Missing files and directories report None so the caller can fall back.
        assert!(
            serve_file(&headers(None), &Method::GET, &fsys, "nope.js").is_none(),
            "missing file must not be served"
        );
        assert!(
            serve_file(&headers(None), &Method::GET, &fsys, "assets").is_none(),
            "directory must not be served"
        );
    }

    #[test]
    fn accepts_gzip_cases() {
        for (value, want) in [
            ("gzip", true),
            ("gzip, br", true),
            ("br, gzip;q=0.5", true),
            ("", false),
            ("br", false),
            ("identity, deflate", false),
        ] {
            let h = headers(if value.is_empty() { None } else { Some(value) });
            assert_eq!(accepts_gzip(&h), want, "accepts_gzip({value:?})");
        }
    }

    /// Pins the entry document to no-cache so new asset hashes roll out on the
    /// next navigation.
    #[tokio::test]
    async fn spa_fallback_cache_control() {
        let resp = call(Method::GET, "/workspace/anything", None).await;
        assert_eq!(
            resp.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-cache",
            "fallback cache-control"
        );
    }

    /// Real files in the embedded tree are served with their own type and are not
    /// swallowed by the SPA fallback.
    #[tokio::test]
    async fn embedded_file_is_served_directly() {
        let resp = call(Method::GET, "/favicon.svg", None).await;
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/svg+xml"
        );
    }
}
