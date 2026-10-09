use super::*;

static VIEWER_HTML_ETAG: LazyLock<HeaderValue> =
    LazyLock::new(|| strong_etag(VIEWER_HTML.as_bytes()));
static VIEWER_CSS_ETAG: LazyLock<HeaderValue> =
    LazyLock::new(|| strong_etag(VIEWER_CSS.as_bytes()));
static VIEWER_JS_ETAG: LazyLock<HeaderValue> = LazyLock::new(|| strong_etag(VIEWER_JS.as_bytes()));
static MARKDOWN_JS_ETAG: LazyLock<HeaderValue> =
    LazyLock::new(|| strong_etag(MARKDOWN_JS.as_bytes()));
static TOOL_OUTPUT_JS_ETAG: LazyLock<HeaderValue> =
    LazyLock::new(|| strong_etag(TOOL_OUTPUT_JS.as_bytes()));
static SERVICE_WORKER_ETAG: LazyLock<HeaderValue> =
    LazyLock::new(|| strong_etag(SERVICE_WORKER.as_bytes()));
static MANIFEST_ETAG: LazyLock<HeaderValue> = LazyLock::new(|| strong_etag(MANIFEST.as_bytes()));
static ICON_SVG_ETAG: LazyLock<HeaderValue> = LazyLock::new(|| strong_etag(ICON_SVG.as_bytes()));
static ICON_192_ETAG: LazyLock<HeaderValue> = LazyLock::new(|| strong_etag(ICON_192));
static ICON_512_ETAG: LazyLock<HeaderValue> = LazyLock::new(|| strong_etag(ICON_512));
static MASKABLE_512_ETAG: LazyLock<HeaderValue> = LazyLock::new(|| strong_etag(MASKABLE_512));
static APPLE_TOUCH_ICON_ETAG: LazyLock<HeaderValue> =
    LazyLock::new(|| strong_etag(APPLE_TOUCH_ICON));
static MONO_FONT_ETAG: LazyLock<HeaderValue> = LazyLock::new(|| strong_etag(MONO_FONT));
static ENCODED_ETAGS: LazyLock<Mutex<BTreeMap<(String, String), HeaderValue>>> =
    LazyLock::new(|| Mutex::new(BTreeMap::new()));

const MAX_STATIC_ASSET_BYTES: usize = 2 * 1024 * 1024;

/// Every asset the browser application is built from. They are real files
/// under `src/web/` and `src/icons/` rather than string literals, so the
/// JavaScript can be read, formatted and tested as JavaScript, and so the
/// content-security policy below can forbid inline script outright.
pub(super) const VIEWER_HTML: &str = include_str!("../web/viewer.html");
pub(super) const VIEWER_CSS: &str = include_str!("../web/viewer.css");
pub(super) const VIEWER_JS: &str = include_str!("../web/viewer.js");
pub(super) const MARKDOWN_JS: &str = include_str!("../web/markdown.js");
pub(super) const TOOL_OUTPUT_JS: &str = include_str!("../web/tool-output.js");
/// A fake DOM for running the shipped renderers under Node. It is deliberately
/// not served: it exists so `cargo test` can exercise `markdown.js` without a
/// browser.
#[cfg(test)]
pub(super) const TEST_DOM_JS: &str = include_str!("../web/test-dom.js");
pub(super) const SERVICE_WORKER: &str = include_str!("../web/service-worker.js");
pub(super) const MANIFEST: &str = include_str!("../web/manifest.webmanifest");
pub(super) const ICON_SVG: &str = include_str!("../../src/icons/icon.svg");
pub(super) const ICON_192: &[u8] = include_bytes!("../../src/icons/icon-192.png");
pub(super) const ICON_512: &[u8] = include_bytes!("../../src/icons/icon-512.png");
pub(super) const MASKABLE_512: &[u8] = include_bytes!("../../src/icons/maskable-512.png");
pub(super) const APPLE_TOUCH_ICON: &[u8] = include_bytes!("../../src/icons/apple-touch-icon.png");
pub(super) const MONO_FONT: &[u8] = include_bytes!("../../src/fonts/jetbrains-mono.woff2");

/// What the browser is permitted to load and execute.
///
/// `default-src 'none'` refuses everything not named below, so a future asset
/// has to be allowed deliberately. Script and style come only from this
/// origin, which is why none of either may be inline. `img-src` allows `blob:`
/// for browser-local attachment previews and keeps `data:` for legacy image
/// content rendered in a transcript.
pub(super) const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
script-src 'self'; \
style-src 'self'; \
img-src 'self' data: blob:; \
font-src 'self'; \
connect-src 'self'; \
manifest-src 'self'; \
base-uri 'none'; \
form-action 'none'; \
frame-ancestors 'none'";

pub(super) async fn viewer() -> Response<Body> {
    static_response(
        "text/html; charset=utf-8",
        VIEWER_HTML,
        false,
        &VIEWER_HTML_ETAG,
    )
}

pub(super) async fn viewer_css() -> Response<Body> {
    static_response(
        "text/css; charset=utf-8",
        VIEWER_CSS,
        false,
        &VIEWER_CSS_ETAG,
    )
}

pub(super) async fn viewer_js() -> Response<Body> {
    static_response(
        "text/javascript; charset=utf-8",
        VIEWER_JS,
        false,
        &VIEWER_JS_ETAG,
    )
}

pub(super) async fn markdown_js() -> Response<Body> {
    static_response(
        "text/javascript; charset=utf-8",
        MARKDOWN_JS,
        false,
        &MARKDOWN_JS_ETAG,
    )
}

pub(super) async fn tool_output_js() -> Response<Body> {
    static_response(
        "text/javascript; charset=utf-8",
        TOOL_OUTPUT_JS,
        false,
        &TOOL_OUTPUT_JS_ETAG,
    )
}

pub(super) async fn manifest() -> Response<Body> {
    static_response("application/manifest+json", MANIFEST, false, &MANIFEST_ETAG)
}

/// The worker itself is never cached: a stale worker is what keeps a phone on
/// a superseded application, and it is the one asset that can never be fixed
/// by a later upgrade.
pub(super) async fn service_worker() -> Response<Body> {
    static_response(
        "text/javascript; charset=utf-8",
        SERVICE_WORKER,
        true,
        &SERVICE_WORKER_ETAG,
    )
}

pub(super) async fn icon() -> Response<Body> {
    static_response("image/svg+xml", ICON_SVG, false, &ICON_SVG_ETAG)
}

pub(super) async fn icon_192() -> Response<Body> {
    binary_response("image/png", ICON_192, &ICON_192_ETAG)
}

pub(super) async fn icon_512() -> Response<Body> {
    binary_response("image/png", ICON_512, &ICON_512_ETAG)
}

pub(super) async fn maskable_512() -> Response<Body> {
    binary_response("image/png", MASKABLE_512, &MASKABLE_512_ETAG)
}

pub(super) async fn apple_touch_icon() -> Response<Body> {
    binary_response("image/png", APPLE_TOUCH_ICON, &APPLE_TOUCH_ICON_ETAG)
}

pub(super) async fn mono_font() -> Response<Body> {
    binary_response("font/woff2", MONO_FONT, &MONO_FONT_ETAG)
}

pub(super) fn static_response(
    content_type: &'static str,
    body: &'static str,
    no_store: bool,
    etag: &'static HeaderValue,
) -> Response<Body> {
    finish_static(
        Response::new(Body::from(body)),
        content_type,
        no_store,
        etag,
    )
}

pub(super) fn binary_response(
    content_type: &'static str,
    body: &'static [u8],
    etag: &'static HeaderValue,
) -> Response<Body> {
    finish_static(Response::new(Body::from(body)), content_type, false, etag)
}

/// Cacheable assets still revalidate. `no-cache` means "ask first", not "do
/// not store", so an upgraded viewer is picked up on the next load while an
/// unchanged one costs one conditional request.
pub(super) fn finish_static(
    mut response: Response<Body>,
    content_type: &'static str,
    no_store: bool,
    etag: &'static HeaderValue,
) -> Response<Body> {
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if no_store { "no-store" } else { "no-cache" }),
    );
    headers.insert(ETAG, etag.clone());
    response
}

/// Compression changes the representation bytes, so the final validator and
/// conditional request check must run outside the compression layer. Only
/// static responses carry ETags; streaming events and live JSON pass through.
pub(super) async fn finalize_static_etag(request: Request, next: Next) -> Response<Body> {
    let request_headers = request.headers().clone();
    let mut response = next.run(request).await;
    if response.status() != StatusCode::OK {
        return response;
    }
    let Some(raw_etag) = response.headers().get(ETAG).cloned() else {
        return response;
    };
    if let Some(encoding) = response.headers().get(CONTENT_ENCODING).cloned() {
        let encoding_name = match encoding.to_str() {
            Ok(name) => name.to_owned(),
            Err(error) => {
                tracing::warn!(%error, "invalid content encoding on a static asset");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        let raw_name = raw_etag.to_str().unwrap_or_default().to_owned();
        let key = (raw_name, encoding_name);
        let cached_etag = ENCODED_ETAGS
            .lock()
            .expect("encoded ETag cache lock is not poisoned")
            .get(&key)
            .cloned();
        if let Some(etag) = cached_etag {
            response.headers_mut().insert(ETAG, etag.clone());
            if if_none_match(&request_headers, &etag) {
                mark_not_modified(&mut response);
            }
            return response;
        }

        let body = std::mem::replace(response.body_mut(), Body::empty());
        let bytes = match axum::body::to_bytes(body, MAX_STATIC_ASSET_BYTES).await {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, "failed to buffer compressed static asset for its ETag");
                return StatusCode::INTERNAL_SERVER_ERROR.into_response();
            }
        };
        let etag = ENCODED_ETAGS
            .lock()
            .expect("encoded ETag cache lock is not poisoned")
            .entry(key)
            .or_insert_with(|| strong_etag(&bytes))
            .clone();
        response.headers_mut().insert(ETAG, etag.clone());
        if if_none_match(&request_headers, &etag) {
            mark_not_modified(&mut response);
        } else {
            *response.body_mut() = Body::from(bytes);
        }
        return response;
    }

    response.headers_mut().insert(ETAG, raw_etag.clone());
    if if_none_match(&request_headers, &raw_etag) {
        mark_not_modified(&mut response);
    }
    response
}

fn mark_not_modified(response: &mut Response<Body>) {
    *response.status_mut() = StatusCode::NOT_MODIFIED;
    response.headers_mut().remove(CONTENT_ENCODING);
    response.headers_mut().remove(CONTENT_LENGTH);
    *response.body_mut() = Body::empty();
}

fn strong_etag(body: &[u8]) -> HeaderValue {
    let digest = Sha256::digest(body);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}").expect("writing into a String cannot fail");
    }
    HeaderValue::from_str(&format!("\"{hex}\""))
        .expect("a hexadecimal SHA-256 entity tag is a valid header")
}

fn if_none_match(request_headers: &HeaderMap, etag: &HeaderValue) -> bool {
    let Ok(etag) = etag.to_str() else {
        return false;
    };
    request_headers
        .get_all(IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

/// Headers every response carries, applied once as a layer so no route can
/// forget them.
///
/// The layer also owns `no-store` for live state and authentication, rather
/// than leaving it to each handler. A rejected request never reaches its
/// handler, so a handler-set header is missing from exactly the responses that
/// are least worth storing.
pub(super) async fn security_headers(request: Request, next: Next) -> Response<Body> {
    let live = {
        let path = request.uri().path();
        path.starts_with("/api/") || path.starts_with("/auth/")
    };
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_SECURITY_POLICY_HEADER,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    if live {
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}
