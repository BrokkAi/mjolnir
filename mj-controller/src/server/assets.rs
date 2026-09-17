use super::*;

/// Every asset the browser application is built from. They are real files
/// under `src/web/` and `src/icons/` rather than string literals, so the
/// JavaScript can be read, formatted and tested as JavaScript, and so the
/// content-security policy below can forbid inline script outright.
pub(super) const VIEWER_HTML: &str = include_str!("../web/viewer.html");
pub(super) const VIEWER_CSS: &str = include_str!("../web/viewer.css");
pub(super) const VIEWER_JS: &str = include_str!("../web/viewer.js");
pub(super) const MARKDOWN_JS: &str = include_str!("../web/markdown.js");
pub(super) const TOOL_OUTPUT_JS: &str = include_str!("../web/tool-output.js");
pub(super) const VOICE_WORKLET_JS: &str = include_str!("../web/voice-worklet.js");
pub(super) const VOICE_WORKER_JS: &str = include_str!("../web/voice-worker.js");
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
    static_response("text/html; charset=utf-8", VIEWER_HTML, true)
}

pub(super) async fn viewer_css() -> Response<Body> {
    static_response("text/css; charset=utf-8", VIEWER_CSS, false)
}

pub(super) async fn viewer_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", VIEWER_JS, false)
}

pub(super) async fn markdown_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", MARKDOWN_JS, false)
}

pub(super) async fn voice_worklet_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", VOICE_WORKLET_JS, false)
}

pub(super) async fn voice_worker_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", VOICE_WORKER_JS, false)
}

pub(super) async fn tool_output_js() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", TOOL_OUTPUT_JS, false)
}

pub(super) async fn manifest() -> Response<Body> {
    static_response("application/manifest+json", MANIFEST, false)
}

/// The worker itself is never cached: a stale worker is what keeps a phone on
/// a superseded application, and it is the one asset that can never be fixed
/// by a later upgrade.
pub(super) async fn service_worker() -> Response<Body> {
    static_response("text/javascript; charset=utf-8", SERVICE_WORKER, true)
}

pub(super) async fn icon() -> Response<Body> {
    static_response("image/svg+xml", ICON_SVG, false)
}

pub(super) async fn icon_192() -> Response<Body> {
    binary_response("image/png", ICON_192)
}

pub(super) async fn icon_512() -> Response<Body> {
    binary_response("image/png", ICON_512)
}

pub(super) async fn maskable_512() -> Response<Body> {
    binary_response("image/png", MASKABLE_512)
}

pub(super) async fn apple_touch_icon() -> Response<Body> {
    binary_response("image/png", APPLE_TOUCH_ICON)
}

pub(super) async fn mono_font() -> Response<Body> {
    binary_response("font/woff2", MONO_FONT)
}

pub(super) fn static_response(
    content_type: &'static str,
    body: &'static str,
    no_store: bool,
) -> Response<Body> {
    finish_static(Response::new(Body::from(body)), content_type, no_store)
}

pub(super) fn binary_response(content_type: &'static str, body: &'static [u8]) -> Response<Body> {
    finish_static(Response::new(Body::from(body)), content_type, false)
}

/// Cacheable assets still revalidate. `no-cache` means "ask first", not "do
/// not store", so an upgraded viewer is picked up on the next load while an
/// unchanged one costs one conditional request.
pub(super) fn finish_static(
    mut response: Response<Body>,
    content_type: &'static str,
    no_store: bool,
) -> Response<Body> {
    let headers = response.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        CACHE_CONTROL,
        HeaderValue::from_static(if no_store { "no-store" } else { "no-cache" }),
    );
    response
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
