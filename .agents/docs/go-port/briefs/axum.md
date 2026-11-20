# HTTP server: `go/crates/axum` and `go/crates/tower`

axum 0.8.9, axum-server 0.7.3, tower 0.5.3, tower-http 0.6.11 (features
`compression-br`, `compression-gzip`), http-body-util 0.1.3, and the `http`
crate types they share with `go/crates/reqwest`. No oracle module is
registered; write `go/port/oracle/src/axum_oracle.rs` and say which
dependencies it needs. Start after the tokio core and `go/crates/reqwest`
exist (share `StatusCode`, `HeaderMap`, `Method` with it: one definition).

The daemon's HTTP API and web viewer are an axum application
(`mj-server/src/`, with the daemon implementing its backend traits in
`mj-daemon/src/server_runtime/`): about 23 files, with routing, extractors,
middleware, JSON bodies, server-sent events or streaming responses, TLS with a
self-signed certificate, and graceful shutdown. The Rust `mj` client, the web
viewer's JavaScript and other tools speak to it, so every status code, header
and body the Go server produces must equal the Rust server's, including for
malformed requests.

Inventory what is used into `doc.go`, then build over `net/http`, running
each handler as a task of the port's async runtime:

- `Router`: `route`, `nest`, `merge`, `fallback`, `layer`, `route_layer`,
  `with_state`, method routers (`get`, `post`, ..., `on`, `any`, chained),
  axum 0.8's path syntax (`{id}`, `{*rest}`), matching and precedence as
  `matchit` decides, trailing-slash behaviour, the 404 and 405 answers (with
  `Allow`), `HEAD` handling, percent-decoding of path parameters.
- Extractors and their rejections (status, headers and body text of each):
  `Path`, `Query`, `Json` (content-type check, syntax vs data error
  distinction and their messages), `Form`, `State`, `Extension`, `HeaderMap`,
  `TypedHeader` if used, `Bytes`, `String`, `Request`, `ConnectInfo`,
  `MatchedPath`, `OriginalUri`, `DefaultBodyLimit` and the default 2 MB limit
  with its 413 answer, `Multipart` and `ws` only if used.
- Responses: `IntoResponse` for the types handlers return (tuples of status,
  headers and body; `Json` with its content type; `Html`; `Redirect`;
  `Response`; `Result`; `Sse` with `Event` and keep-alive formatting; `Body`
  from a stream), the headers hyper adds (`content-length` or chunked,
  `date`), and header casing on the wire.
- Middleware: `middleware::from_fn`, `from_fn_with_state`, `Next`,
  `tower::ServiceBuilder` and layer ORDER (which layer sees the request
  first), `tower_http::compression::CompressionLayer` (negotiation by
  `Accept-Encoding`, the size and content-type predicates of its default
  policy, gzip and brotli through `compress/gzip` and
  `github.com/andybalholm/brotli`), `tower::ServiceExt::oneshot` (tests call
  the router directly without a socket: support that), other tower-http
  layers only if used.
- Serving: `axum::serve(listener, app).with_graceful_shutdown(fut)`,
  `axum_server::from_tcp`, `from_tcp_rustls`, `tls_rustls::RustlsConfig`,
  `Handle` (`graceful_shutdown(Some(duration))`, `shutdown`, `listening`),
  HTTP/1.1 and HTTP/2 as the Rust build serves them, connection keep-alive and
  what happens to in-flight requests at shutdown (AGENTS.md requires in-flight
  request and response delivery to complete across a daemon handoff).
- `http_body_util::BodyExt::collect` and friends as used in tests.

Tests: the routing, extractor-rejection and response tests of axum that apply;
then record what the real axum application answers for a table of requests
(every extractor's failure modes, unknown routes, wrong methods, oversized and
truncated bodies, compression negotiation, HEAD, SSE framing, shutdown while
a request is in flight) and require the Go server to answer the same bytes.
