# Brief: porting mj-core to Go package `core`

Read `.agents/plans/go-port.md` first (the ExecPlan; its library table says
which Go package replaces each Rust dependency). This brief adds the rules
for `mj-core`, which every agent porting a part of it follows.

## What we are doing

We translate Mjolnir's own Rust code into Go, module by module, onto Go's
standard library and established Go packages. We do not port Rust libraries
(tokio, serde, anyhow, clap, ...) into Go: we use the Go equivalent. The Go
build must behave as the Rust one does: same files written, same JSON and
TOML accepted and produced, same error and output text, same decisions.

## One package

`mj-core/src` has a dependency cycle through 42 of its modules, so all of
`mj-core` becomes the single Go package `core` in `go/internal/core/`
(import path `github.com/BrokkAi/mjolnir/go/internal/core`).

- File names: module `foo` becomes `foo.go`; submodule `foo/bar.rs` becomes
  `foo_bar.go`; deeper nesting joins with `_`. Tests go in the matching
  `_test.go`. Fixture files go under `go/internal/core/testdata/<module>/`.
- Embedded assets: `go/internal/core/assets/` is a copy of
  `mj-core/assets/` made by `go/scripts/sync-assets.sh`; embed with
  `//go:embed`. Other files a module `include_str!`s from its own source
  directory: copy them under `go/internal/core/data/<module>/` (runtime
  data) or `testdata/<module>/` (test-only data).
- Comments: start each file with `// Port of mj-core/src/<path>.rs.` Keep
  the Rust doc comments, translated to Go doc style. Keep `Hard-won:`
  markers on tests.

## Naming

- Types keep their Rust names (`HarnessKind`, `Config`).
- `pub` functions, constants and statics become exported PascalCase
  (`validate_id` -> `ValidateID`, Go initialism style: ID, URL, HTTP, JSON,
  ACP, API, SSH, TLS). Crate-private and private ones become unexported
  camelCase.
- Methods keep the same rule on their receiver type.
- A unit-only enum becomes a named string or int type with constants
  `TypeVariant` (`HarnessKindClaude`). An enum with data becomes a sealed
  interface (an unexported marker method) with one struct per variant named
  `TypeVariant`, unless a single struct with a kind field is clearly simpler
  and keeps the same serialization.
- `Option<T>` becomes `*T` or a `(T, bool)` pair, whichever keeps "absent"
  distinct from "empty" where Rust keeps them distinct.
- Name collisions: package `core` is flat, so these top-level names exist in
  more than one Rust module. Prefix the Go name with the module name in
  PascalCase in every module except the first listed (for example `Action`
  stays `Action` in `activity/eot_glide`, and becomes `AssessmentAction` in
  `assessment`). If you hit another collision, apply the same rule and
  report it.

      action: activity/eot_glide, assessment
      background: activity/eot_glide/types, assessment
      containertemplate: config/targets, targets
      decision: activity/verdict, jev
      environment: config/secrets, login_environment
      failure: activity/eot_glide/types, assessment
      handbackremindertext: relay/snapshot, subagent
      input: activity/eot_glide/types, assessment
      iodraintimeout: subprocess, targets
      isfalse: config, config/aws, state, transcript
      isplanreviewid: acp, acp/dialect/grok
      maxbodybytes: continuation, github_item
      modeloffer: config/harness, model_catalog, review/autoconfigure
      normalizemodel: model_catalog, usage/pricing
      parse: activity/question_locator, codex_catalog, config/dsh_credentials, login_environment
      planreviewidprefix: acp, acp/dialect/grok
      podmanworkspacelocator: state, targets
      podmanworkspacestorage: config/targets, targets
      questions: activity/eot_glide, activity/question_locator, github_item
      reply: activity/question_locator, assessment

  (The full list is produced by the collision script in the ExecPlan's
  Artifacts; trivial helpers like `default_true`/`is_false` may simply be
  shared once.)

## Translating Rust idioms

- serde: JSON is a wire and storage format shared with other daemon and
  worker versions, so the Go encoding must be byte-compatible. Use
  `encoding/json` with struct tags; write `MarshalJSON`/`UnmarshalJSON`
  where serde attributes need it (`rename_all`, `tag`, `content`,
  `untagged`, `flatten`, `default`, `skip_serializing_if`, `alias`,
  `deny_unknown_fields`, `from`/`into`). `BTreeMap` becomes a map (Go's
  JSON encoder sorts keys) or a sorted slice where iteration order matters.
- TOML: `github.com/BurntSushi/toml`.
- anyhow: `fmt.Errorf("context: %w", err)`, keeping the Rust message text.
  `bail!` becomes `return ..., fmt.Errorf(...)`/`errors.New`.
- tokio: goroutines, channels, `sync`, `context.Context` for cancellation,
  `time` for timers. `tokio::process` and `std::process` become `os/exec`
  through the ported subprocess helpers in `subprocess.go` (the AGENTS.md
  subprocess rules apply).
- `tracing` becomes `log/slog`.
- HTTP (reqwest): `net/http`. URLs: `net/url`.
- The ACP crate (`agent-client-protocol`): `github.com/BrokkAi/acp-go`.
- Paths: `string` with `path/filepath`, compared by component.
- Unsafe and platform code: `runtime.GOOS` checks or `_unix.go` /
  `_windows.go` build-tagged files.

## Tests

Port every test in your modules (`#[cfg(test)] mod tests` blocks and
`tests.rs` files) with the same inputs and assertions. They are the
specification. Go test names are the Rust names in PascalCase with a
`Test` prefix; prefix the module name if the name collides.

## Working rules

- Edit only the files of the modules you were given, plus new fixture
  files for them. If you need something from another module that is
  missing or has the wrong shape, do not edit that module: say so in your
  result.
- `go build ./internal/core/` builds the whole package. Errors in your own
  files must be fixed; errors only caused by other modules still being
  written are expected in the declarations phase.
- Run `gofmt -w` on your files.
- Commit your files in your worktree when done (one commit; message
  "go port: <modules>") and report the commit.

## Decisions for the bodies phase (2026-11-20)

The declarations are merged and compile (commit 823be3abb). About 970
function bodies are `panic("unported: ...")`. These rules settle what the
declaration agents left open.

- Libraries: zstd `github.com/klauspost/compress/zstd`; Windows APIs
  `golang.org/x/sys/windows`; YAML `go.yaml.in/yaml/v3`; JSONC
  `github.com/tailscale/hujson`; HuggingFace tokenizer JSON
  `github.com/sugarme/tokenizer` (only the `github-history` code uses it).
  Unified diffs (Rust `similar`): use an established Go diff library only if
  its output matches the Rust tests byte for byte; otherwise write the Myers
  diff in the module, which is small.
- toml_edit (format-preserving edits in config/document.rs): parse with
  `github.com/pelletier/go-toml/v2/unstable` and splice the edits into the
  original text by byte range, so comments and layout survive as they do
  in Rust. Plain TOML decoding and encoding: `github.com/BurntSushi/toml`
  or go-toml/v2, whichever reproduces the Rust output; the config group
  decides and uses one.
- Thread-local state in Rust (the config secret resolver and the
  sources-only flag) becomes an explicit value passed through decoding, not
  a process-wide variable.
- Keep each group's declared conventions (sealed interface or kind struct,
  decoder names). Do not rename another group's API. Shared helpers that
  exist in several copies stay as they are until the final cleanup.
- JSON: nil Go slices and maps must serialize as Rust does (`[]`/`{}` for a
  present empty Vec/map, absent or `null` exactly as serde writes it), and
  required fields must be required, exactly as serde decodes.
- Fields holding a sealed interface must decode through the owner's
  decoder; plain encoding/json cannot fill an interface.

## Crates above mj-core

Every other crate follows the same rules with these changes.

- Package: one Go package per crate, `go/internal/<crate without mj->`
  with dashes removed (`mj-transcript` -> `go/internal/transcript`, package
  `transcript`; `mj-build-id` -> `go/internal/buildid`; `mj-worker` ->
  `go/internal/worker`). A crate's internal modules all go in its package,
  files named as in mj-core (`archive_git_operation.go`). Binaries
  (`mj-worker-bin`, `mj-cli`'s `main`) go under `go/cmd/`.
- Imports: a crate's dependencies are the Go packages of its Cargo
  dependencies (`github.com/BrokkAi/mjolnir/go/internal/core`, ...). Use the
  existing exported API of those packages; if it lacks something the Rust
  crate uses from the dependency, it is a porting gap there: add it to the
  dependency's file that ports the Rust item, following the Rust source.
- Name collisions only matter within a package; prefix with the module
  name as for mj-core.
- Files a crate `include_str!`s, and its `tests/golden` and `tests/fixtures`
  files, are copied under the package's `testdata/` (or `data/` when not
  test-only) and embedded or read from there.
- Golden tests compare with the copied expected files, which must stay
  identical to the Rust ones; `MJ_UPDATE_GOLDEN` never rewrites them from Go
  output.
- Tests that need a crate not yet ported (a Rust dev-dependency such as
  mj-controller) are ported with `t.Skip("needs <crate> (not yet ported)")`
  and listed in the result, so they are enabled when that crate lands.
- mj-build-id: Go records the commit and its time itself
  (`runtime/debug.ReadBuildInfo`, `vcs.revision`/`vcs.time`); release-only
  values come from `-ldflags -X`. The worker build identity is computed by
  `go/scripts/build.sh` from the Git tree of the Go worker's inputs and
  passed with `-ldflags -X`.
