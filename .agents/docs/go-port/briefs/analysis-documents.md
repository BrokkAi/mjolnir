# Four analysis documents for the Go port

This assignment is analysis, not code: read Rust (and some Go) and write four
reference documents that translators will trust without re-checking. Do not
write Go code and do not edit any file other than these four. Be exhaustive and
verify every claim against the source; say where you infer rather than read.
Read `.agents/docs/go-port/translation-rules.md` and `runtime-packages.md`
first. Rust product code is in `mj-*/` and `voice-worker/`; crate sources under
`~/.cargo/registry/src/*/`.

1. `.agents/docs/go-port/acp-mapping.md`. The Rust code uses
   `agent-client-protocol` 5.0.0 (features `unstable_session_fork`,
   `unstable_session_system_prompt`, `unstable_end_turn_token_usage`, and
   `process`/`stdio` in some crates; schema crate
   `agent-client-protocol-schema` 1.15.1). The Go port uses
   `github.com/BrokkAi/acp-go` v0.14.0 (`~/go/pkg/mod/github.com/!brokk!ai/acp-go@v0.14.0/`;
   read its README, `docs/`, especially `rust-ecosystem-parity.md` and
   `unstable-facades.md`, and the `schema`, `unstable`, `agent`, `jsonvalue`
   and root packages). Write: (a) every distinct `agent_client_protocol::...`
   path the product uses, with a count and the crates using it, mapped to the
   exact acp-go identifier or marked MISSING; (b) how construction and
   matching translate for the ten most used types (acp-go represents tagged
   unions as a struct with a Kind and one pointer field per variant); (c) how
   the Rust code drives connections (`on_receive_request`, `UntypedMessage`,
   `JsonRpcRequest`, `ConnectTo`, `Lines`, `Agent`, ...) and the acp-go
   equivalent of each; (d) every difference that could change behaviour,
   including that acp-go tracks Rust SDK 5.4.0 while Mjolnir pins 5.0.0.
2. `.agents/docs/go-port/counted-arcs.md` (exists; complete and verify it):
   every use of `Weak`, `Arc::downgrade`, `upgrade`, `Arc::strong_count`,
   `weak_count`, `try_unwrap`, `into_inner`, `get_mut`, `make_mut`, `ptr_eq`,
   `Weak::ptr_eq`, `Rc`, with the concrete type and what the code relies on;
   then the definitive list of types whose `Arc` must be `rs.Arc`, the
   `make_mut` sites (copy-then-mutate is equivalent?), the `get_mut` sites
   (provably unique, or count-dependent), the `ptr_eq` sites.
3. `.agents/docs/go-port/compressed-bytes.md` (exists; complete and verify it):
   every place the product computes a digest, length, signature, cache key or
   equality over bytes produced by gzip, deflate, zstd, zip or tar writers, or
   stores such bytes for later comparison; for each, the writer and settings,
   what is compared and by whom, and the verdict (decodable by the other side
   is enough / deterministic within one implementation is enough / must equal
   Rust's bytes), plus every test asserting a fixed digest or exact archive
   bytes, and a conclusion naming which compressors and archive writers must
   be ported bit-exactly and at which levels.
4. `.agents/docs/go-port/library-usage.md`: for `reedline`, `tokenizers`,
   `ort` (+ `half`, `libloading`), `tree-sitter` + `tree-sitter-bash`,
   `cap-std` + `cap-fs-ext`, `imbl`, `indexmap`, `rat-focus`, `tui-term`,
   `arboard`, `webbrowser`, `image` + `png`, `sysinfo`, `portable-pty`,
   `signal-hook`, `rayon`, `yaml-rust2`, `jsonc-parser`, `brokk-anvil-client`
   (`anvil_client`), `brokk-sessionwiki` (`sessionwiki`), `ring`, `rustls`,
   `rcgen`, `tiny_http`, `cpal`, `mach2`, `libc` (every function, constant and
   struct used, grouped, with counts) and `windows-sys` (every API used): the
   files, the items called, and what callers depend on. For `anvil_client` and
   `sessionwiki` (about 30,000 lines each), which of their modules are
   reachable from Mjolnir's calls and an estimate of the lines to port. For
   `tokenizers` and `ort`, which model and tokenizer files are loaded, from
   where, and what is computed.

Finish with a report under 300 words: what you produced, what you could not
determine, and the most important surprises.
