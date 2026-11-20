# HTTP client: `go/crates/reqwest` (reqwest 0.13.4) and `go/crates/rustls`

No oracle module is registered. If you want one, write
`go/port/oracle/src/reqwest_oracle.rs` and say which dependencies and features
it needs. The main check is a local server that records what each client
sends.

Mjolnir is an HTTP client in about 30 files: its own daemon API client
(`mj-client`, `mj-cli`), GitHub and provider APIs (`mj-accounts`,
`mj-session-runtime`, `mj-daemon`, `mj-github-relevance`), downloads
(`mj-core/src/harness_install.rs`, `mj-build-cache`, `mj-worker-staging`),
uploads and streaming. It uses `reqwest::Client`, `reqwest::blocking::Client`,
`ClientBuilder`, `RequestBuilder`, `Response`, `StatusCode`, `header::*`,
`redirect::Policy`, `Proxy`, `Url`, `Error`. `rustls` is touched directly in a
few files (installing the `ring` provider, a `ClientConfig` with custom roots
or a verifier pinning the daemon's self-signed certificate).

What must match is what the other end can observe and what the caller can
observe.

1. Inventory every reqwest and rustls item and builder option used, with the
   values passed, into `doc.go`. Check which decompression features the build
   enables and do not decompress unless Rust does.
2. `Client`/`ClientBuilder` (the operations that are `async` in Rust take
   `ctx` and must be cancellable when the calling task is cancelled; read
   `.agents/docs/go-port/tokio-contract.md`) and `blocking::Client`, over
   `net/http`:
   - the request as reqwest and hyper send it: method, target, `Host`, default
     headers with their casing and order as far as a server can tell,
     `content-length` or `transfer-encoding` for each body kind,
     `content-type` for `.json()`, `.form()` and multipart with reqwest's
     encodings, auth helpers, HTTP/1.1 or HTTP/2 by ALPN as the Rust build
     selects. State every difference that remains.
   - redirects: default limit 10, headers stripped across origins, method
     changes for 301, 302, 303, `Policy::none`, custom policies as used.
   - proxies: reqwest's system proxy detection (the environment variables and
     their lower-case forms, the CGI guard, its `NO_PROXY` matcher, which
     differs from Go's), proxy auth, `Proxy::all/http/https`, `no_proxy()`.
   - timeouts: total, connect and read as reqwest defines them, and the error
     each produces.
   - `Response`: `status`, `headers`, `url`, `content_length`, `version`,
     `remote_addr`, `error_for_status`, `text`, `bytes`, `json`, `chunk`,
     `bytes_stream`.
   - `Error`: every predicate, `status`, `url`, `without_url`, and reqwest's
     `Display` text for each kind, with the source chain a caller sees through
     anyhow's `{:#}`. Map Go's network errors onto the kinds reqwest reports.
   - `StatusCode`, `header::{HeaderMap, HeaderName, HeaderValue}`, `Method`,
     `Version`.
3. TLS: determine which trust roots the Rust build uses (read the features
   resolved in `Cargo.lock`) and trust the same set by default; support
   `add_root_certificate`, `danger_accept_invalid_certs`,
   `use_preconfigured_tls(ClientConfig)` with Mjolnir's verifier reproduced
   through `crypto/tls`. In `go/crates/rustls` provide only what product code
   names.

Tests: a local server (HTTP and HTTPS, HTTP/1.1 and HTTP/2; a raw TCP
listener for HTTP/1.1 so exact bytes and header order are captured) that can
redirect, delay, truncate, reset and answer each status class; a table of
client configurations and requests taken from Mjolnir's real call sites;
assertions on the recorded bytes, on `Response` values and on each error's
predicates and text. Capture what the real reqwest sends for the same table
and record it as the expected values, so the comparison is against Rust.
