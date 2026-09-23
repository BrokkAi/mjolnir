# Expose Mjolnir's launch options on the documented HTTP API

This ExecPlan is a living document. The sections `Progress`, `Surprises &
Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to
date as work proceeds.

This document must be maintained in accordance with `.agents/PLANS.md` at the
repository root.

## Purpose / Big Picture

Starting a Mjolnir session requires naming two identifiers before anything else
can happen: a profile id (which account and harness) and a target id (which
machine and runtime the session runs on). Today a program that wants to start a
session can only learn those identifiers by reading `config.toml` itself, or by
being the terminal dashboard or the web viewer. There is no documented way to
ask the daemon "what accounts do I have, where can sessions run, and which pair
should I use if I do not care?"

After this change, one authenticated HTTP request answers that question.
`GET /api/v1/options` returns the profiles, the targets, the bundles, what is
known about each host's availability, and the remembered default profile and
target pair. `mj api-info` already tells a caller where the API is and which
bearer token to present, so the whole exchange is: read `mj api-info`, call
`/api/v1/options`, choose, start a session.

The immediate consumer is Brokk Town, a separate local service that coordinates
repository bots. Town wants to offer an execution target without becoming a
second place where infrastructure is configured. It will read this list, show a
choice only when more than one real option exists, and store a reference to a
target Mjolnir owns rather than a copy it maintains. Nothing in this plan is
Town-specific: the endpoint is useful to any script, to the CLI, and to future
clients that need to answer "where can this run" without parsing a TOML file.

This plan covers the whole Mjolnir-side surface that a consumer needs, in four
milestones. Milestone 1 is the read-only options endpoint. Milestone 2 makes the
default usable at session creation. Milestone 3 records the artifact routes that
a consumer needs as part of the stable contract instead of incidental behavior.
Milestone 4 describes a deferred, larger piece: an ACP adapter owned by this
repository.

## Progress

- [x] (2026-09-22T14:00Z) Reconnaissance: located the existing public projection,
      the `/api/v1` route surface, the session-creation request shape, and the
      remembered fast-start preferences.
- [x] (2026-09-22T14:06Z) Wrote this plan.
- [x] (2026-09-22T14:21Z) Milestone 1: added `GET /api/v1/options` with
      profiles, targets, bundles, host availability, and the default pair, with
      three contract tests. Verified by a live request against an isolated
      instance, by the full controller suite (1560 passed), by
      `cargo test --workspace`, and by `cargo clippy --workspace --all-targets
      -- -D warnings`.
- [x] (2026-09-22T15:06Z) Milestone 2: `profile_id` and `target_id` are now
      optional on session create and resolve independently from the remembered
      default. `mj new` no longer requires `--profile`/`--target`. Verified by
      four new contract tests, by the full workspace suite, by
      `cargo clippy --workspace --all-targets -- -D warnings`, and by a live
      `mj new` on an isolated instance.
- [x] (2026-09-22T15:19Z) Milestone 3: the API module documentation now names
      the four artifact routes as contract, with their media types, response
      bodies, and the 409-versus-5xx distinction. The only genuine gap in their
      coverage was the transcript's paging cursor, which the route test now
      pins. Verified in a worktree by `cargo fmt --all -- --check`, by
      `cargo clippy --workspace --all-targets -- -D warnings`, and by the
      controller suite (1565 passed).
- [ ] Milestone 4 (deferred): ship an mj-owned ACP adapter so new capabilities
      reach a consumer without a consumer release.

## Surprises & Discoveries

- Observation: the read model this endpoint needs already exists and is already
  published to the web viewer as `ViewerSnapshot`. It carries `profiles`,
  `targets`, `bundles`, and `capacity` (per-host availability), and its
  constructor `ViewerSnapshot::from_config_state` documents an explicit
  redaction guarantee: it never copies profile homes or environment, SSH hosts
  or keys, container environment, AWS details, concrete resource locators,
  native session IDs, or raw error strings.
  Evidence: `mj-controller/src/server/viewer_types.rs`, the struct fields at
  lines 5-38 and the doc comment on `from_config_state`.

- Observation: the viewer projects a target without its machine. The person-
  facing host name lives on `ViewerTargetCapacity.label`, which is documented as
  "the host or fleet as a person names it. Never a locator, an address or a full
  path," and carries `target_ids`, `stale`, `refreshing`, and `has_error`. That
  is why availability is joined to a target through the host reading rather than
  read off the target itself.
  Evidence: `mj-controller/src/server/viewer_types.rs`, `ViewerTarget` at line
  764 and `ViewerTargetCapacity` at line 734.

- Observation: a capacity reading deliberately does not carry the probe's error
  message, because that message names hosts and commands. An unavailable reason
  in the API therefore has to be a sentence this repository composes, not text
  copied from the probe.
  Evidence: the comment on `has_error` in `ViewerTargetCapacity`.

- Observation: the remembered default already exists, for a different purpose.
  `GoPreferences` stores `default: Option<GoRecipe>` beside `config.toml` in
  `go.json`, the first setup becomes the default, and `GoRecipe::defaults()`
  strips bundle, project directory, and mounts because "project paths and
  repository choices never leak into another folder." The portable part of a
  saved setup is exactly a profile id and a target id.
  Evidence: `mj-core/src/go.rs`, `GoRecipe::defaults` and
  `GoPreferences::save_recipe`.

- Observation: `/api/v1` currently exposes no configuration at all. Its routes
  cover sessions, workspaces, prompts, transcripts, usage, files, elicitations,
  exports, and the session index. The only profile-shaped route is
  `/profiles/{profile_id}/config`, which discovers the models and efforts a
  profile's harness offers; it does not list profiles or targets.
  Evidence: `mj-controller/src/server/api/routes.rs`.

- Observation: a projection revision is a large monotonic counter, not a small
  sequence number. The live instance answered `"revision": 1790086448725747`,
  which arrived as a revision of 1 in the test fixture. Treat it as an opaque
  token for comparing two reads; do not parse it or expect it to increment by
  one.
  Evidence: the live response recorded below.

- Observation: a fresh instance with no `config.toml` targets at all already
  answers with four usable ones, and the local host reading marks them `ready`
  rather than `unknown`. `Config::with_local_targets` materializes localhost,
  Podman, Docker, and Apple Container when the file is loaded, and the capacity
  poller covers the local host from the start. This is the zero-configuration
  case the endpoint exists to serve.
  Evidence: the live response recorded below.

- Observation: an isolated instance inherits the default viewer port
  `127.0.0.1:3765`, so it fails to start when a daemon is already running for
  the same user, and a daemon that starts before its `config.toml` exists keeps
  the port it first resolved until it is stopped. Give a throwaway instance its
  own `[phone] bind` and stop it before expecting a config change to take
  effect.
  Evidence: `Error: the web viewer failed to start: Port 3765 is already in use`

- Observation: the `mj-cli` integration test `daemon_startup` stalled for 28
  minutes during one full workspace run, with a spawned `mj daemon-run` child
  alive, and then passed on its own in 3.6 seconds serially and 2.2 seconds in
  parallel, twice, and the following full workspace run passed. The stall
  coincides with a workspace-wide rebuild running at the same time and with a
  real daemon of the same user already holding the default viewer port, so it
  reads as environment sensitivity in a test that starts real daemons rather
  than a behavior change: nothing in that test binary reaches `new`, the option
  route, or the controller's configuration. When a workspace run appears to
  hang, run the suspect binary directly to tell a stall from a failure.
  Evidence: `ps` showed `daemon_startup-ec5ad5a53fb214c6` at 27:42 elapsed with
  its child `target/debug/mj daemon-run`; running either the binary or the same
  tests through cargo afterwards finished in seconds.

- Observation: `mj new` reports resolution failures well past the point where
  the identifier question is settled, so a deliberately invalid bundle is a
  clean way to prove that resolution succeeded without provisioning anything.
  Evidence: `mj new --bundle does-not-exist` with a saved default answers
  `unknown bundle`, which can only be reached after both identifiers resolved.

## Decision Log

- Decision: expose one endpoint, `GET /api/v1/options`, rather than separate
  `/profiles`, `/targets`, and `/bundles` routes.
  Rationale: a caller cannot make a launch decision from one list alone. It
  needs the profile, the target, and the default together, and it needs them
  from one consistent revision so a form cannot render half of one
  configuration and half of another. One request also means one cache entry and
  one refresh for a consumer. Adding narrower routes later is additive and
  costs nothing.
  Date/Author: 2026-09-22, root agent.

- Decision: report availability as a three-state string (`ready`, `stale`,
  `unknown`, `unavailable`) rather than a boolean.
  Rationale: capacity is empty until the host poller publishes a reading, and an
  unprobed target is not an unavailable target. A boolean would force a caller
  to guess, and the honest answer before a probe is "we do not know yet."
  Date/Author: 2026-09-22, root agent.

- Decision: read the remembered default from an injected path held in
  `ServerState`, not from the process environment inside the handler.
  Rationale: `GoPreferences::path()` resolves through `MJ_CONFIG_DIR`, and the
  route tests run in one process. Reading the real path from a handler would
  make tests depend on the developer's own configuration and would race with any
  test that sets the environment variable. An injected path keeps the handler a
  pure function of its inputs and lets a test point at a temporary file.
  Date/Author: 2026-09-22, root agent.

- Decision: reuse the viewer projection instead of building a second read model.
  Rationale: the projection is already the controller's answer to "what is
  configured," already redacted, already built on every snapshot refresh, and
  already covered by tests. A second model would drift.
  Date/Author: 2026-09-22, root agent.

- Decision: the API defines its own response types rather than serializing the
  `Viewer*` types directly.
  Rationale: the module documentation states that the `/api/v1` routes are the
  stable surface while the viewer's routes are undocumented and shaped around
  what a phone renders. Publishing `Viewer*` types as the contract would freeze
  viewer internals, so a presenter field could never change for phone reasons
  alone. The API types are small and derive from the projection.
  Date/Author: 2026-09-22, root agent.

- Decision: resolve an omitted profile and target in the HTTP handler, not in
  the controller.
  Rationale: the controller's job is to run what it is told, and its action
  already carries two explicit identifiers that its own validation and its
  retry records depend on. Resolving in the handler keeps one shared decision
  point for every caller of the route, leaves the controller's contract
  unchanged, and keeps the failure a plain bad request instead of a dispatch
  outcome.
  Date/Author: 2026-09-22, root agent.

- Decision: relax `mj new`'s `--profile` and `--target` in the same change.
  Rationale: the command-line interface is a client of this route, and leaving
  the flags mandatory would keep the zero-configuration path unreachable from a
  terminal while the API allowed it. It also makes the behavior observable
  without a hand-written request.
  Date/Author: 2026-09-22, root agent.

- Decision: leave `POST /wiki/sessions/{wiki_id}/restore` requiring both
  identifiers.
  Rationale: a restore reproduces a specific archived session somewhere the
  caller chooses, so naming the destination is part of the request rather than
  an inconvenience. Creation is where "I do not care, use my default" is a real
  intent.
  Date/Author: 2026-09-22, root agent.

## Context and Orientation

Mjolnir runs a per-user background process called the daemon. The daemon owns
`config.toml` (profiles, machines, targets, bundles), the SQLite store of
sessions, and two HTTP surfaces.

The first surface is the web viewer under `/api/...`. It is cookie-only, aimed
at the phone and desktop clients, and explicitly undocumented. The second is
the documented API under `/api/v1/...`, described in
`mj-controller/src/server/api.rs` as "the documented HTTP API an orchestrating
agent drives sessions with." It authenticates with a bearer token from the file
`api-token` in the data directory, and every response carries an `mj-api-version`
header so a client can refuse a contract it does not understand.

The pieces this plan touches:

`mj-controller/src/server/api.rs` is the module root. It declares the
submodules, the version constants, and re-exports their types.

`mj-controller/src/server/api/routes.rs` is the route table. Every route is
registered here, behind one authentication layer and one response-header layer.

`mj-controller/src/server/api/types.rs` holds the wire types for the documented
API, including `StartSessionRequest`, whose `profile_id` and `target_id` are
required strings today.

`mj-controller/src/server/api/tests.rs` is the contract test suite. It builds the
router with a hand-written fake backend and a `ViewerSnapshot`, so the HTTP
contract is exercised without a running daemon.

`mj-controller/src/server/viewer_types.rs` defines `ViewerSnapshot` and its
`from_config_state` constructor, which projects the private configuration into
the public, redacted shape.

`mj-controller/src/server/routes.rs` defines `ServerState`, the value every
handler receives, and the viewer route table.

`mj-controller/src/server/validation.rs` holds `require_profile`,
`require_target`, and `require_bundle`, which turn an unknown identifier into a
bad-request failure.

`mj-core/src/go.rs` holds `GoPreferences` and `GoRecipe`, the remembered
fast-start setup stored in `go.json` beside `config.toml`.

Terms used in this plan. A profile is one account or one harness installation:
it has a kind such as `codex` or `claude` and a harness home directory on the
controller. A target is a runtime template that says how a session runs and
which machine it runs on, for example bare on this computer, or Podman on a
named SSH host. A machine is a host: this computer, an SSH host, or an EC2
launch template. A bundle is a set of repositories that a managed target
provisions into a workspace. Projection means the controller's transformation of
private configuration and session state into the public shape a client renders.
The daemon is the background process described above.

## Milestone 1: the options endpoint

At the end of this milestone the daemon answers one new request, and a caller
that has never read `config.toml` can enumerate what it may launch.

    GET /api/v1/options
    Authorization: Bearer <contents of api-token>

The response is a single JSON object:

    {
      "revision": 42,
      "profiles": [{"id": "codex-work", "harness": "codex"}],
      "targets": [
        {"id": "raw", "kind": "local-bare", "availability": "unknown",
         "requires_project_directory": true, "is_default": true},
        {"id": "podman", "kind": "local-podman", "availability": "ready",
         "requires_project_directory": false, "is_default": false,
         "host": "this-mac"}
      ],
      "bundles": [
        {"id": "product", "primary_repository": "product",
         "repositories": [
           {"id": "product", "github": "acme/product", "destination": "product"}
         ]}
      ],
      "hosts": [
        {"id": "host-1", "label": "this-mac", "targets": ["raw", "podman"],
         "stale": false, "refreshing": false, "has_error": false}
      ],
      "default": {"profile_id": "codex-work", "target_id": "raw"}
    }

`revision` is the revision of the snapshot the lists came from, so a caller can
tell whether two reads are the same view of the world. `default` is null when
nothing has ever been saved.

Availability is one of four strings:

`ready` means a host reading covers the target and its last probe succeeded.
`stale` means a reading exists but is marked stale. `unavailable` means a
reading exists and its last probe failed; the target's `unavailable` sentence
explains that the host did not answer, without repeating the probe's own message
because that message names hosts and commands. `unknown` means no reading covers
the target yet, which is the ordinary state immediately after the daemon starts.

To implement it:

1. Add the wire types to `mj-controller/src/server/api/types.rs`. They are
   `LaunchOptions`, `LaunchProfile`, `LaunchTarget`, `LaunchBundle`,
   `LaunchRepository`, `LaunchHost`, `LaunchDefault`, and an availability enum
   serialized as a lowercase string. Follow the existing style in that file:
   `Debug, Clone, PartialEq, Eq, Serialize, Deserialize`, and
   `#[serde(deny_unknown_fields)]` on responses a client parses.

2. Add `mj-controller/src/server/api/options.rs` with the handler. It takes
   `State<ServerState>`, borrows `state.snapshot_rx`, maps the projection into
   the new types, joins each target to a host reading for availability, and
   loads the default from the injected preferences path. A missing, unreadable,
   or malformed `go.json` is not an error for this route: it yields
   `"default": null`, because a caller that cannot read a preference can still
   use everything else. Log the failure at debug level so the cause is
   discoverable without failing the request.

3. Register the module in `mj-controller/src/server/api.rs`, following the
   existing `mod`/`pub use` pattern, and add the route in
   `mj-controller/src/server/api/routes.rs` inside the authenticated router.

4. Add `preferences_path: PathBuf` to `ServerOptions` in
   `mj-controller/src/server.rs`, defaulting to `mj_core::go::GoPreferences::path()`
   in `ServerOptions::new`, with a builder method to override it for tests.
   Copy it into `ServerState` in `mj-controller/src/server/routes.rs`.

5. Cover it in `mj-controller/src/server/api/tests.rs`:

   - an unauthenticated request is refused and still carries `mj-api-version`;
   - the sample fixture's profile, target, and bundle ids appear;
   - no secret from the sample fixture appears anywhere in the serialized body.
     The fixture deliberately holds `/highly/secret/codex`, `secret-token`,
     `secret.registry/image`, `secret-target`, and `/private/source/hel`; assert
     the body contains none of them. This test is the point of the milestone: it
     proves the new route inherits the projection's redaction rather than
     copying fields itself.
   - a target with no host reading reports `unknown`;
   - a failed host reading reports `unavailable` with a composed sentence;
   - a `go.json` written to a temporary path reports `default`, and a missing
     file reports null.

Run the suite with an isolated instance and a temporary `go.json`:

    cd /Users/ryansvihla/code/mjolnir
    cargo test -p brokk-mj-controller --lib

`cargo test` must run outside the restricted sandbox in this repository: the
suite opens loopback TCP and Unix sockets, and a sandboxed run can fail with
`EPERM` or hang without producing a valid result.

To see it by hand, start a daemon on an isolated instance and call the route:

    cd /Users/ryansvihla/code/mjolnir
    cargo run -p brokk-mj-cli -- --instance options-demo api-info --json
    curl -s -H "Authorization: Bearer $(cat <data-dir>/api-token)" \
      <base_url>/options | head -40

The `base_url` and token path come from `api-info --json`. The response must
list every configured profile and target and must not contain a harness home, an
SSH host, a container environment value, or an AWS detail.

## Milestone 2: create a session without naming a pair

Today `StartSessionRequest.profile_id` and `target_id` are required strings, so
a consumer must know identifiers before it can create anything. After this
milestone they are optional. When absent, the daemon resolves them from the
remembered default; when the default is absent too, it fails with the same
bad-request shape an unknown identifier produces, naming what is missing.

This is the change that lets a consumer offer no configuration at all: read
`/api/v1/options`, and create a session without naming either identifier. It
also lets a consumer show a picker only when the list holds more than one real
choice, because the absent case is now expressible.

Implementation notes. `StartSessionRequest` lives in
`mj-controller/src/server/api/types.rs`; `start_session` in
`mj-controller/src/server/api/sessions.rs` resolves and validates before sending
a `ControllerAction::New`, and already calls `require_profile` and
`require_target`. The resolution belongs in that handler so that every caller
gets identical behavior, and the resolved names must be what the controller
receives: a downstream consumer must never see a session whose profile is
implicit.

## Milestone 3: name the artifact routes as contract

A consumer that runs work on another machine cannot inspect a local working tree
to decide whether a change is safe to publish. It needs the session's diff, its
export as a branch or bundle, its transcript, and its file reads to be part of
the stable contract rather than incidental behavior of the viewer.

Those routes already exist in `mj-controller/src/server/api/routes.rs`:
`/sessions/{session_id}/diff`, `/sessions/{session_id}/export`,
`/sessions/{session_id}/transcript`, and the files route. This milestone is
documentation and a test that pins each response shape, so a future refactor
cannot quietly change a field a consumer depends on. State the guarantee in the
module documentation in `mj-controller/src/server/api.rs`, where the stable
surface is described.

## Milestone 4: an ACP adapter owned by this repository (deferred)

A consumer whose bots speak the Agent Client Protocol needs an adapter that
speaks ACP on one side and drives this API on the other. That adapter must ship
from this repository, not from the consumer, or every new Mjolnir capability
waits on a consumer release and the reuse goal fails.

This milestone is deliberately out of scope for the first change. It needs its
own ExecPlan covering ACP session lifecycle mapping, cancellation, elicitation,
and the mapping from a turn outcome to an ACP stop reason. Recorded here so the
sequence is visible to the next contributor.

## Validation

For Milestone 1, the observable proof is a live request. Start a daemon on an
isolated instance, call `GET /api/v1/options` with the bearer token, and confirm
that the response lists the configured profiles and targets, marks an unprobed
target `unknown`, and exposes no harness home, SSH host, container environment
value, or AWS detail. Then run the contract suite.

For every milestone, run the repository's required checks:

    cd /Users/ryansvihla/code/mjolnir
    cargo test
    cargo clippy --all-targets -- -D warnings

Run these outside the restricted sandbox, on the dev profile, where
`debug_assert!` and overflow checks are active.

## Idempotence and recovery

Every step in this plan is additive. The new route is read-only, and a caller
that never calls it sees no change. Milestone 2 changes a request shape from
required to optional, which accepts strictly more requests than before and
leaves every existing caller working. No migration is needed for either
milestone: neither touches the SQLite store, so there is no schema revision to
advance. If a step fails halfway, re-running it is safe.

## Outcomes & Retrospective

Written at completion of each milestone.

### Milestone 1

Reached 2026-09-22. A caller that has only `mj api-info` can now enumerate what
it may launch. The route is the first configuration read on the documented
surface, and it required no new read model: the endpoint narrows the existing
viewer projection, which already carries an explicit redaction guarantee. The
three tests pin the properties that matter rather than the implementation: the
secret-bearing fixture never reaches the body, an unchecked target is `unknown`
because the capacity poller writes nothing until it runs, and the saved default
publishes only two identifiers so a per-project bundle or directory cannot leak
through it.

The live proof, on an instance whose `config.toml` names no targets:

    HTTP/1.1 200 OK
    mj-api-version: 1
    cache-control: no-store

    {"revision":1790086448725747,"profiles":[],"targets":[
      {"id":"apple-container","kind":"apple-container","requires_project_directory":false,"availability":"ready","host":"local"},
      {"id":"docker","kind":"local-docker","requires_project_directory":false,"availability":"ready","host":"local"},
      {"id":"localhost","kind":"local-bare","requires_project_directory":true,"availability":"ready","host":"local"},
      {"id":"podman","kind":"local-podman","requires_project_directory":false,"availability":"ready","host":"local"}],
     "bundles":[],"hosts":[{"id":"local","label":"local",
      "targets":["apple-container","docker","localhost","podman"],
      "stale":false,"refreshing":false,"has_error":false}]}

A request without the bearer token answers 401.

What remains is Milestone 2, which is the half that makes the default usable:
without it a caller can read the pair but must still name both identifiers to
start anything.

### Milestone 2

Reached 2026-09-22. A caller can now read the launch options and start work
without naming anything: the profile and target each fall back to the pair the
`mj go` workflow saves, the two resolve independently so naming one still uses
the default for the other, and the omitted case is expressible at all, which is
what lets a consumer show a choice only when there is a real one.

The live proof, on an instance whose `config.toml` declares one profile and no
targets, with `go.json` saving `codex-demo` and `localhost`:

    $ mj new --bundle does-not-exist
    Error: the Mjolnir API answered 400 Bad Request: unknown bundle

Reaching the bundle check proves both identifiers resolved, because the bundle
is examined after resolution and nothing is provisioned on this path. Removing
the saved default changes the answer to what a caller needs to hear:

    $ mj new --bundle does-not-exist
    Error: the Mjolnir API answered 400 Bad Request: name a profile_id;
    this instance has no saved default to fall back on

Restoring `go.json` returns the first answer, so resolution is live on every
request rather than cached at startup. The endpoint reports the same pair:

    default: {'profile_id': 'codex-demo', 'target_id': 'localhost'}

Four contract tests cover what the live run cannot reach cheaply: the resolved
pair reaching the controller as two explicit identifiers, independent
resolution, the absent-default message, and the stale-default message for a
saved profile the user has since deleted.

The remaining gap before a consumer can adopt this is Milestone 3, which names
the artifact routes as contract, and then Milestone 4, the ACP adapter.

### Milestone 3

Reached 2026-09-22, on the `docs/artifact-route-contract` branch. The four
artifact routes turned out to be almost fully pinned already: the diff patch and
its JSON metadata, all three export kinds including the branch's remote and the
bundle's attachment filename, file read and injection with their unsafe-path
refusals, and the transcript's text flattening and limit clamping each had a
test asserting the response. Manufacturing more tests would have duplicated
those, so the work is the guarantee itself, stated where a consumer will look
for it: the module documentation in `mj-controller/src/server/api.rs` now names
the routes, their media types, their bodies, and the rule that an impossible
artifact is a 409 a person can act on while a failed one is a 5xx.

One real gap surfaced while checking: the transcript response's paging cursor
was unpinned. `session_id` and `next_after_seq` are the two fields a caller
needs to keep reading, and nothing asserted them, so the existing route test now
does. The change was verified in a worktree with a fresh target directory:
format clean, clippy clean workspace-wide, and 1565 controller tests passing.

What remains is Milestone 4, the ACP adapter, which is the piece that lets a
consumer whose bots speak ACP reach any of this without new code in the
consumer.
