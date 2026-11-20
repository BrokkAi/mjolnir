# Run several before-push reviews at once, one per set of worktrees

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root and must be maintained in accordance with that file.


## Purpose / Big Picture

A Mjolnir session asks for review of its committed work with the `request_review` tool before it pushes. Today a session can have only one review open at a time. An agent that works in two worktrees, or in two repositories, must wait for the first verdict before it can ask for the second, even though the two reviews have nothing in common.

After this change a session can start a review of worktree `a` and, while that review runs, start a review of worktree `c`. Both run at the same time and each delivers its own verdict. A caller can also ask for one review that covers several worktrees on purpose, by passing a list of paths. Two open reviews never cover the same repository root. A request that would overlap an open review is refused, and the refusal says exactly what to do next.

To see it working: in a test instance, start a session in a workspace with two worktrees of the same repository, commit in both, and call `request_review` with `paths: ["<worktree a>"]` and then with `paths: ["<worktree c>"]` without waiting. Both calls answer `accepted`, the dashboard shows two reviews for the session, and two verdict messages arrive, each naming its worktree. A third call with `paths: ["<worktree a>", "<worktree c>"]` while both run is refused with a message that names the open reviews.


## Progress

- [x] (2026-11-20) Precursor, separate commit: sub-agents no longer get the review tools, and `set_review_baseline` is renamed `set_baseline`. (Not part of this plan's milestones, but it changes the same tool surface.)
- [x] (2026-11-20) Review of the plan: added root reservation for override and set_baseline, and corrected the older-worker behavior.
- [x] (2026-11-20) Second review of the plan: restart stale preparations, resolve roots without a capture, keep save-before-ref for overrides.
- [x] (2026-11-20) Fifth and sixth reviews: interrupted markers keep their own recovery path in Milestone 2 (see Milestone 2); one relay constant in Milestone 3 gates both ResolveRepositories and concurrency, and Milestone 1 no longer adds a relay version.
- [x] (2026-11-20) Fourth review of the plan: the base generation counts every review close, because a failed review can have moved some bases.
- [x] (2026-11-20) Third review of the plan: review-off joins root ownership, clean verdicts retire findings, pathless override selects by count, old override schemas stay at one outstanding verdict, legacy overrides bump the base generation; new action fields are deliberately not version-gated.
- [ ] Milestone 1: tool API and default scope, still one review per session (completed: `paths` and `override_review(path)` through tool, durable action with legacy `path` decoding, daemon dispatch and host; `capture_scope` in `reviewer.rs` decides the scope once for preparation and the review-off path; worker refusal lists workspace repositories; instructions and docs. Remaining: test run, review, commit).
- [ ] Milestone 2: store one row per open review (breaking migration).
- [ ] Milestone 3: review host keyed by review, with the overlap decision at the open step.
- [ ] Milestone 4: surfaces show several reviews per session.
- [ ] End-to-end check in an isolated instance.


## Surprises & Discoveries

- Observation: the "no path" review did not cover every repository under the working directory. It covered the repository that contains the working directory and the repository that contains each additional directory, because discovery runs `git rev-parse --show-toplevel` once per root.
  Evidence: `discover_repositories` in `mj-worker/src/review/capture.rs` (around line 812) and `roots()` in `mj-worker/src/worker_runtime/review_capture.rs` (around line 85), which returns the working directory followed by `additional_directories`.

- Observation: the relay request the review host sends to the worker already takes a list of repositories. Only the tool schema limited callers to one path.
  Evidence: `ReviewerAction::CaptureDelta { repositories: Option<Vec<PathBuf>> }`; `mj-review-host/src/review_host/begin.rs` wraps the single `path` in a one-element vector.

- Observation: the worker already removes duplicate paths that name the same worktree, because it compares resolved roots.
  Evidence: `scoped_repositories` in `mj-worker/src/worker_runtime/review_capture.rs` pushes a root only when `selected` does not already contain it.

- Observation: declined findings are already scoped per repository root, so they do not need to move into per-review state.
  Evidence: `DeclinedFinding { entry, repositories }` and `TurnReviewState::advance_review_bases` in `mj-core/src/storage.rs`.


## Decision Log

- Decision: the unit of concurrency is a set of resolved repository roots. Two reviews in one session may be open together only when their root sets are disjoint.
  Rationale: the review base is stored per worktree as a Git ref (`worktree_review_base_ref` in `mj-worker/src/review/capture.rs`), and the pre-push gate checks reviewed trees per repository. A review is a statement about the HEAD of specific worktrees, so the worktree is the natural owner. Disjoint scopes mean every root has at most one open review, so the base, the declined findings and the push gate each have one owner per root.
  Date/Author: 2026-11-20, agreed with the user.

- Decision: the tools select a review by path, never by a review ID. `request_review(paths?)`, `override_review(path?, overrides)` and `set_baseline(commit, path?)`.
  Rationale: the agent already knows its paths and does not otherwise need IDs. Because open reviews never share a root, one root identifies at most one review. IDs would allow two reviews of the same worktree at different HEADs, after which it is unclear which one moves the base.
  Date/Author: 2026-11-20, agreed with the user.

- Decision: `request_review` takes `paths`, a list, and the single `path` argument is removed from the tool schema. No alias for `path` is kept in the tool.
  Rationale: a list lets a caller deliberately review a cross-repository change in one pass, so one reviewer sees both sides. Keeping both `path` and `paths` would give two ways to say one thing. Only models call these tools and they read the current schema. The durable action queue is different: it must still read actions an older worker queued with `path` (see Milestone 1).
  Date/Author: 2026-11-20, agreed with the user.

- Decision: when `paths` is omitted, the review covers the repository that contains the session's working directory. The old "every workspace repository" scope is removed. When the working directory is not inside a repository, the request is refused and the refusal lists the discovered repositories so the caller can pass them as `paths`.
  Rationale: "all" was inherited from per-turn review and had no recorded reason. It disagreed with `set_baseline`, whose default was already the working directory. Under concurrency it would overlap every other review and need a special case.
  Date/Author: 2026-11-20, agreed with the user.

- Decision: a request whose roots overlap an open review is refused, not narrowed. The refusal names the overlapping root, the roots of the open review, and both ways forward: request the non-overlapping roots now, or wait for the verdict and request the whole set.
  Rationale: a caller who names `b` and `c` together usually wants one reviewer to see both. Narrowing to `c` would silently drop that. Two sensible actions exist, so the refusal offers both.
  Date/Author: 2026-11-20, agreed with the user.

- Decision: the overlap decision is made once, inside the review host's actor loop, when a prepared review is about to open (the `HostEvent::Prepared` handling that leads to `pending_open`). It is not made when the request arrives.
  Rationale: only the worker can resolve a path to a repository root, and that happens during preparation, which runs asynchronously. A check on arrival would compare path text and would miss two paths that name the same worktree. The actor loop is the one serialized decision point; reviews that are still preparing have no resolved roots and cannot conflict yet. If two overlapping requests race, whichever reaches the open step second is refused, whatever order they finished preparing in.
  Date/Author: 2026-11-20.

- Decision: `override_review(path?)` selects the outstanding findings verdict whose roots contain the resolved root of `path`. Without `path`, it works only when exactly one findings verdict is outstanding; otherwise it is refused with a list of the outstanding verdicts' roots. `set_baseline` without `path` uses the repository of the working directory, as today.
  Rationale: same as the path-selection decision. A refusal that lists the choices lets the agent retry in one step.
  Date/Author: 2026-11-20.

- Decision: a new findings verdict for roots R replaces every stored findings record that shares a root with R.
  Rationale: the latest verdict for a root is the one the agent must answer; an older record for that root describes commits that have since been reviewed again.
  Date/Author: 2026-11-20.

- Decision: `override_review` and `set_baseline` reserve their roots in the actor before the worker changes any ref, and release them when the operation ends.
  Rationale: found by review of the plan. The worker's `SetReviewBase` and `RecordOverridden` actions move refs before they answer, so a check after the answer would come too late. An override selected through `a` also writes refs for every root of the selected verdict, for example `b` of a verdict for `a, b`, so it must reserve all of them, not just the root it was named by.
  Date/Author: 2026-11-20.

- Decision: the new `paths` and `override_review.path` fields of the durable actions are not gated on the daemon's relay version.
  Rationale: raised by review, which noted that an older daemon's action decoder denies unknown fields. A worker that writes the new fields only runs under the daemon that started it or a newer one: daemon handoff replaces the daemon first and workers afterwards, and a daemon is never downgraded automatically. The project's relay versions describe what a worker can do for the daemon; the earlier worker-to-daemon action `SetReviewBaseline` was added the same way, without a gate. What must keep working is the other direction, a new daemon reading actions an older worker queued, and Milestone 1 keeps that by decoding the old `path` field.
  Date/Author: 2026-11-20.

- Decision: the review-off path takes part in root ownership. When review is off, `request_review` still records the captured trees as allowed to push (`run_review_off` in `begin.rs`). It never emits `HostEvent::Prepared`, so in Milestone 3 it must reserve its captured roots in the actor, under the same rule as override and `set_baseline`, and refuse when a review still owns one.
  Rationale: found by review. Today `begin_push` refuses before `run_review_off` when a review is open; removing that refusal without a replacement would let it approve a push for a root another review owns.
  Date/Author: 2026-11-20.

- Decision: a clean verdict retires every findings record that shares a root with it, the way a new findings verdict replaces them.
  Rationale: found by review. The current clean-verdict path clears pending findings. Without the same rule per root, an old findings record for `a` would stay outstanding after `a` passed, and a pathless override for the only real verdict on `b` would be refused as ambiguous.
  Date/Author: 2026-11-20.

- Decision: a pathless `override_review` selects by count before it resolves anything: exactly one outstanding findings record is selected and its roots are reserved; otherwise the call is refused with the list of records. Only an explicit `path` goes through `ResolveRepositories`.
  Rationale: found by review. Resolving the omitted path to the working directory would refuse a call whose only verdict belongs to a linked worktree outside the working directory.
  Date/Author: 2026-11-20.

- Decision: a session can have more than one outstanding findings record only when its worker's relay protocol is at least `RELAY_CONCURRENT_REVIEW_PROTOCOL`, the one constant Milestone 3 adds with `ResolveRepositories`. Below it, the overlap check treats an outstanding findings record as owning every root of the session, so such a session keeps at most one review and one verdict at a time.
  Rationale: found by review, twice. A worker without `override_review(path)` cannot choose between two verdicts, and a worker that has the argument but no `ResolveRepositories` cannot have its path resolved to a root without a review capture. Gating on the single protocol that brings both removes both dead ends, and no separate Milestone 1 protocol is needed: Milestone 1 does not change what the daemon sends to the worker, and a worker that writes the new fields always runs under a daemon that reads them.
  Date/Author: 2026-11-20.

- Decision: a preparation records the session's base generation, and the open step restarts it when a base moved in the meantime.
  Rationale: found by review of the plan. The overlap check alone misses a review that opens and settles while another request of the same root is still preparing; the stale capture could then move the base backwards. Restarting rather than refusing keeps the caller's request working without action from them, and every restart follows a real base move, so it cannot loop on its own.
  Date/Author: 2026-11-20.

- Decision: `override_review` and `set_baseline` resolve their roots with a new read-only `ResolveRepositories` relay request, not a review capture, and an override saves its declined findings before the worker records its ref.
  Rationale: found by review of the plan. A capture refuses uncommitted tracked edits that `set_baseline` accepts today and can create a start ref before the actor decides. `run_override` saves the declined findings before recording the ref that permits the push; if the order were reversed, a daemon stop between the two could leave a pushable tree without its declined findings.
  Date/Author: 2026-11-20.

- Decision: a worker older than relay protocol 49 keeps its old behavior for a request with no `paths` (`repositories: None`), and explicit `paths` stay refused with "restart the session".
  Rationale: found by review of the plan, which had assumed an old worker ignores a scoped capture. `prepare` in `mj-review-host/src/review_host/reviewer.rs` refuses it instead, so always sending a scope would break no-argument reviews that work today.
  Date/Author: 2026-11-20.

- Decision: the session part of review state (declined findings and findings records) has one owner, the review host's actor, which keeps it in memory per session. Operations that run in tasks (override, set_baseline, a verdict) return the change they want to make; the actor applies it to its current copy and saves it through the ordered persistence lane. No task loads, changes and saves that state itself.
  Rationale: today `run_override` in `override_push.rs` loads the session's state from the store, works on it across awaits, and saves it. That was safe only because one review per session meant nothing else wrote the state meanwhile. With several reviews, a verdict for `c` could save between an override's load and save for `a`, and one update would be lost. Moving the change into the actor makes that lost update impossible to express, rather than narrowing the window.
  Date/Author: 2026-11-20.

- Decision: cancelling a review from a surface (the legacy `Resolution` request) cancels every open review of that session.
  Rationale: surfaces offer one cancel action per session today. A per-review cancel can be added later if anyone asks; it is not needed to deliver concurrent reviews.
  Date/Author: 2026-11-20.


## Outcomes & Retrospective

Nothing yet.


## Context and Orientation

Mjolnir has a daemon (the control plane, `mj daemon-run`) and one worker process per session (the data plane, `mj-worker`). The worker runs the coding agent, called the harness, and exposes Mjolnir's tools to it over MCP, a protocol in which a tool server lists tools and the agent calls them. The review tools live on a separate MCP server named `mj-review`. Its tool definitions and argument checks are in `mj-worker/src/subagent_mcp.rs` (`review_tool_definitions` and `review_action`). A tool call becomes a durable action, `mj_core::subagent::SubagentToolAction` in `mj-core/src/subagent.rs`, which the worker queues and the daemon executes. The daemon dispatches review actions in `mj-daemon/src/server_runtime/api.rs` (search for `SubagentToolAction::RequestReview`).

The review host is an actor inside the daemon, in the `mj-review-host` crate, under `mj-review-host/src/review_host/`. An actor here means one task that owns all review state and handles events from a channel one at a time, so no two decisions about reviews can interleave. Its public handle is `TurnReviewHost` in `host.rs`. Its state is `HostState` in `events.rs`. Today `HostState::reviews` is a `BTreeMap<String, ReviewSlot>` keyed by session ID, and `preparing`, `pending_open` and `closing` are keyed by session ID too. `begin.rs` holds `begin_push`, which refuses with "a review is already open" when the session already has a slot, then prepares the review asynchronously: it asks the worker to capture the committed changes (`CaptureDelta`), picks reviewers, and sends `HostEvent::Prepared` back to the actor. `persist.rs` writes the durable "active" marker before the review becomes visible (`pending_open`). `dispatch.rs` routes events. `override_push.rs` handles `override_review`, `set_base.rs` handles `set_baseline`, `recovery.rs` restores reviews after a daemon restart, and `reviewer_child.rs` manages the reviewer sub-sessions a review starts.

A capture is the worker's answer to `CaptureDelta`: one `delta` per repository root with the base commit, the head commit, the tree and the changed files. The worker side is `capture_delta` in `mj-worker/src/worker_runtime/review_capture.rs`. A repository root is the top-level directory of a Git worktree, as `git rev-parse --show-toplevel` prints it. A review base is the commit after which a review starts looking; the worker stores it per worktree as a Git ref.

Durable review state is the `turn_review_state` table, one row per session (`session_id` is the primary key), read and written by `mj-store/src/database/turn_reviews.rs`. Its Rust shape is `mj_core::storage::TurnReviewState` in `mj-core/src/storage.rs`, with four parts: `declined_findings` (already per root), `active` (the open review's marker), `push_findings` (the trees and synthesis of the last findings verdict, which `override_review` answers) and `active_group` (the reviewer children of the open review, for recovery). Migrations live in `mj-store/src/database/schema.rs`; each one is marked compatible or breaking with a reason, and a breaking one raises `minimum_compatible_version` in the same transaction.

Surfaces receive `mj_client::review::RuntimeReviewView` (in `mj-client/src/review.rs`), one per open review, through the runtime feed. The type has `#[serde(deny_unknown_fields)]`, so a new field breaks older readers. The TUI stores them in `session_reviews: BTreeMap<String, RuntimeReviewView>` keyed by session (`mj-tui/src/lib.rs`, `mj-tui/src/ingest.rs`), the chat view in `mj-chat/src/chat/turn_review.rs`, and attention rules in `mj-daemon/src/daemon/attention.rs`.

The standing instructions that teach the agent to use the review tools are `REVIEW_SERVER_INSTRUCTIONS` in `mj-core/src/session_instructions.rs`. Their rendered text is checked by the golden files in `mj-worker/tests/golden/session-instructions-*.txt`; set `MJ_UPDATE_GOLDEN=1` when running the test to rewrite them.


## Plan of Work

### Milestone 1: tool API and default scope

At the end of this milestone the tools have their final shape, while the host still allows one review per session. It can ship alone.

In `mj-worker/src/subagent_mcp.rs`, change the `request_review` schema to `{"paths": {"type": "array", "items": {"type": "string"}, "minItems": 1}}` with no required fields, and describe the default ("the repository that contains your working directory"). Give `override_review` an optional `path` with the same description as `set_baseline`'s. In `review_action`, refuse an empty list and empty strings with messages in the style of the existing ones.

In `mj-core/src/subagent.rs`, change `SubagentToolAction::RequestReview` to carry `paths: Vec<String>` (empty means the default) and `OverrideReview` to carry `path: Option<String>`. In `SubagentToolActionWire`, keep reading the old `path: Option<String>` field of `RequestReview` and fold it into `paths`, the way `OverrideReview` already folds the legacy `reason`. Extend the existing test `request_review_path_preserves_legacy_encoding` rather than adding a new one.

Through `mj-daemon/src/server_runtime/api.rs` and `TurnReviewHost::begin_push` in `mj-review-host/src/review_host/host.rs`, pass `paths: Vec<PathBuf>` instead of `path: Option<String>`. In `begin.rs` and `reviewer.rs` (`prepare`), send `CaptureDelta { repositories: Some(paths) }`, using `vec![PathBuf::from(".")]` for the default. The worker resolves relative paths against the working directory (`scoped_repositories`), so `"."` means the working directory's repository.

A worker older than `RELAY_WORKTREE_REVIEW_PROTOCOL` (relay protocol 49) cannot take a scoped capture: `prepare` in `reviewer.rs` refuses one with "the session's worker predates worktree review; restart the session", and the relay client in `mj-session-runtime/src/worker_client/reviewer.rs` checks the same version. Keep that refusal for explicit `paths`. For a request with no `paths`, send `repositories: None` to such a worker, which reviews what it always reviewed. The review's roots are then the roots the capture returned, so the overlap rule in Milestone 3 still compares real roots; an old worker's review may simply cover more and so conflict more often.

In `scoped_repositories`, when a path is not inside a Git repository and it is the default `"."`, return an error that lists the discovered repositories so the agent can pass them. Keep the existing message for an explicit path.

In `override_push.rs`, accept the optional path. With one review per session there is only one findings record, so in this milestone the path only has to resolve to a root that the record covers; otherwise refuse with the record's roots.

Update `REVIEW_SERVER_INSTRUCTIONS` and regenerate the goldens. Update `docs/src/content/docs/turn-review.md`.

### Milestone 2: one durable row per open review

Add a migration in `mj-store/src/database/schema.rs` that creates `push_reviews(session_id, review_id, roots, active, active_group)` with primary key `(session_id, review_id)`, and `push_findings(session_id, review_id, trees, synthesis)` with the same key. It moves each existing `turn_review_state` row's `active`, `active_group` and `push_findings` into the new tables (the existing review gets roots from its stored group or findings trees, or an empty list meaning "unknown, conflicts with everything" when neither is stored), then drops those columns from `turn_review_state`, which keeps only `declined_findings`. Mark it breaking: an older daemon would read no active review and would start a second review over a restored one. Raise `minimum_compatible_version` in the same transaction.

Split `TurnReviewState` in `mj-core/src/storage.rs` into the session part (`declined_findings`, `push_findings: Vec<PushFindingsRecord>`) and a new `OpenReviewState { review_id, roots, active, active_group }`. Give `PushFindingsRecord` a method that answers whether it covers a root, and give the session part a method that inserts a new record and drops the overlapping ones, following the replace rule in the Decision Log. Update `turn_reviews.rs` so every save and close names the review it writes. `active_review_groups` returns one entry per open review that has a group. A review interrupted during preparation leaves an `active` marker with no group; the startup sweep must keep clearing those through its separate interrupted-review path (`clear_interrupted`, in the sweep in `events.rs`), so that one such row cannot make group recovery fail and block valid groups from restoring.

### Milestone 3: the review host keyed by review

Change `HostState::reviews`, `pending_open` and `closing` to be keyed by review ID, and keep `ReviewSlot` holding its `OpenReviewState` and its resolved roots. Key `preparing` by a request number taken from `next_epoch`, so several preparations of one session can run at once. Remove the "a review is already open" refusal from `begin_push`.

Put the overlap decision in the handling of `HostEvent::Prepared` in `dispatch.rs`, before `persist.rs` inserts into `pending_open`. The prepared capture gives the roots. Compare them with the roots of every slot and every `pending_open` entry of the same session. On overlap, drop the preparation, reply with the refusal described in the Decision Log, and leave state unchanged. Do this in the same event handler that inserts into `pending_open`, so no other event can run between the check and the insert.

The open step must also reject a capture that went stale while it was prepared. A preparation can capture commit A and wait on reviewer selection while another review of the same root opens, settles clean and moves the base; if the first then opened, its verdict would review A against an old base and could move the base back. Keep a base generation per session in `HostState`, a counter the actor increments whenever a review of the session closes, whatever its outcome, and whenever `set_baseline` or an override finishes. Counting every close, not only clean ones, matters because `RecordReviewed` writes one worktree at a time: a review of `a, b` can move `a`'s base and then fail on `b`, and its failed verdict would otherwise leave the generation unchanged. It also covers an override on a worker older than `RELAY_REVIEW_BASE_PROTOCOL` (52), which overrides with `RecordReviewed` and so moves the base (`base_moved` in `run_override`). A spurious restart costs one capture; a missed one can move a base backwards. `begin_push` records the generation when it starts a preparation. At the open step, if the generation has changed, the actor discards the capture and starts the preparation again for the same request, keeping the caller's reply. Each restart is caused by a base move, which only happens when another review settles, so the restarts end.

Make every per-session question ask across that session's reviews: `has_open_review` answers whether any is open; the publish step emits one view per review; `retain_sessions` and session removal close every review of the session; the legacy `Resolution` cancel cancels all of them. Reviewer children already find their review by searching all slots' registrations (see `begin.rs` near the top); keep that search, now over review-keyed slots.

`HostState::push_operation_refusal` in `override_push.rs` refuses `override_review` and `set_baseline` while the session has any open or preparing review. Replace it with a per-root reservation, so overriding the verdict for `a` works while `c` is being reviewed. Both operations change Git refs in the worker (`RecordOverridden`, `SetReviewBase`), and the worker moves the ref before it answers, so the decision must come before the worker acts, not after. Each operation therefore runs in three steps. First, resolve the caller's `path` (or the working directory when it is omitted) to a root with a new read-only relay request, `ReviewerAction::ResolveRepositories { paths }`, which runs only the resolution half of `scoped_repositories` in `mj-worker/src/worker_runtime/review_capture.rs` and returns roots. Do not use `CaptureDelta` for this: a capture refuses uncommitted tracked edits, which `set_baseline` allows today, and it can create the session's start ref before the actor has decided anything. Add `RELAY_CONCURRENT_REVIEW_PROTOCOL` in `mj-core/src/relay.rs` for the new request and make it `RELAY_PROTOCOL_VERSION`; the same constant gates concurrent reviews (see the Decision Log). A worker older than that constant cannot resolve without side effects, so for such a worker the operation reserves every root of the session, which is today's behavior of refusing while any review is open. Second, back in the actor, decide: for an override, select the findings record that covers the root and take all of that record's roots; for `set_baseline`, take the one root. Refuse if any of those roots is in an open review, a `pending_open` entry or another reservation; otherwise record the reservation in `HostState` (a map from session to reserved roots and the operation holding them). For an override, the actor also applies the declined findings to its copy of the session state now and sends the save to the ordered persistence lane, so the declined findings are durable before the worker records the ref that permits the push, as `run_override` does today. Third, run the mutating worker action, and release the reservation when the operation finishes on every exit path, including failure and panic (the existing `OverrideFinished` event is where `spawn_operation` already ends). The open step for a new review also refuses when its roots overlap a reservation. `override_push.rs` selects the findings record by root and `set_base.rs` applies the base move to the record and the declined findings for that root only. Recovery in `recovery.rs` restores each open review separately. The verdict message enqueued through `ReviewEnvironment::enqueue_verdict` names the review's roots.

### Milestone 4: surfaces

Add `review_id: String` and `roots: Vec<String>` to `RuntimeReviewView`. Because of `deny_unknown_fields`, raise `mj_client::daemon::PROTOCOL_VERSION` (follow the pattern of the existing named constants in `mj-client/src/daemon.rs`) so an older client is told to upgrade instead of failing to parse. In the TUI, change `session_reviews` to `BTreeMap<String, Vec<RuntimeReviewView>>`. The session row shows one label: working if any review is working, otherwise the most severe settled state. The chat review panel lists each review with its roots on its first line. `attention.rs` already iterates `metadata.reviews`; check each rule still holds with several reviews per session.


## Concrete Steps

Run every command from the repository root. Cargo here is wrapped by mbx, so plain `cargo` is correct; do not set `CARGO_TARGET_DIR`. Run tests outside any restricted sandbox, because the suite uses loopback sockets.

    cargo test -p brokk-mj-worker-runtime subagent_mcp
    MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-worker-runtime golden_session_instructions
    cargo test -p brokk-mj-core subagent
    cargo test -p brokk-mj-store turn_review
    cargo test -p brokk-mj-review-host
    cargo test -p brokk-mj-daemon review
    cargo clippy -p <each edited crate> --all-targets -- -D warnings

Package names carry a `brokk-` prefix; `mj-worker` is `brokk-mj-worker-runtime`. A filter that matches no tests reports `ok` with `0 passed`; check the count.


## Validation and Acceptance

After Milestone 1, calling `request_review` with no arguments in a session whose working directory is a repository reviews only that repository, and the reply's `repositories` list has one entry. Calling it in a session whose working directory is not a repository is refused with a message that lists the repositories to pass.

After Milestone 3, the following must hold, and the tests below protect it. Two requests with disjoint `paths` in one session are both accepted and both verdicts arrive. A request whose roots overlap an open review is refused with the message from the Decision Log. Two overlapping requests that race are decided at the open step: exactly one opens, whichever order their preparations finish in. A daemon restart with two open reviews restores both.

Tests to add, following the "Which tests to add" rules in `AGENTS.md`:

- A migration integration test in `mj-store/src/database/tests.rs` that writes a `turn_review_state` row in the previous schema with an active group and a findings record, runs the migration, and reads back one open review and one findings record with the right roots.
- A tricky-condition test in `mj-review-host/src/review_host/tests.rs` for the race: prepare `(a, b)` and `(b, c)` concurrently, let `(b, c)` finish preparing first, and check that `(b, c)` opens and `(a, b)` is refused; then the reverse order.
- A restart test in the same file: two open reviews with disjoint roots survive a host restart and each settles on its own.
- A TUI golden render of a session row and the chat panel with two open reviews, extending an existing golden case.
- An end-to-end check in an isolated instance (`mj --instance concurrent-review ...`), as described in Purpose. Stop that daemon afterwards with `mj --instance concurrent-review daemon stop`.


## Idempotence and Recovery

Code milestones are ordinary edits and can be repeated. The Milestone 2 migration is breaking. Test it only with isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`, never against the live store. A binary run from a Cargo target directory refuses to migrate the default instance, so if it refuses, add `--instance`. Never rewrite the migration after it has been applied anywhere; give a later change a new revision.


## Artifacts and Notes

The refusal text for an overlapping request, to be used verbatim in the host and checked by the race test:

    <root> is already in the open review of <root list>. Request a review of <non-overlapping roots> now, or wait for that verdict and then request <requested roots> together.

When every requested root overlaps, the first option is omitted.


## Interfaces and Dependencies

In `mj-core/src/subagent.rs`:

    SubagentToolAction::RequestReview { paths: Vec<String> }
    SubagentToolAction::OverrideReview { overrides: Vec<ReviewOverride>, path: Option<String> }

In `mj-core/src/storage.rs`:

    pub struct OpenReviewState {
        pub review_id: String,
        pub roots: Vec<std::path::PathBuf>,
        pub active: Option<String>,
        pub active_group: Option<ActiveReviewGroup>,
    }

In `mj-review-host/src/review_host/host.rs`:

    pub async fn begin_push(&self, session_id: &str, paths: Vec<std::path::PathBuf>)
        -> Result<serde_json::Value, StartRefusal>
    pub async fn override_push(&self, session_id: &str, path: Option<std::path::PathBuf>,
        overrides: Vec<ReviewOverride>) -> Result<serde_json::Value, StartRefusal>

In `mj-client/src/review.rs`, `RuntimeReviewView` gains `review_id: String` and `roots: Vec<String>`.

Revision note (2026-11-20): review of the first version found that override and set_baseline change refs before the actor could refuse them, and that older workers refuse a scoped capture rather than ignore it. Milestone 1, Milestone 3 and the Decision Log now cover both.

Revision note (2026-11-20, second): review found that a preparation could open with a capture made before another review moved the base, that resolving roots with a capture refuses edits `set_baseline` accepts and has side effects, and that an override must keep saving its declined findings before the ref. Milestone 3 and the Decision Log now cover all three.

Revision note (2026-11-20, third): review found five more Milestone 3 gaps (review-off approvals, clean verdicts and findings records, pathless override selection, older override schemas, legacy overrides that move the base) and asked for relay versioning of the new action fields. The Decision Log records the five fixes and why the new fields are not version-gated.

Revision note (2026-11-20, fourth): review found that `RecordReviewed` can move some bases of a review that then fails, so the base generation now advances on every review close and on every override or `set_baseline`, not only on clean verdicts.

Revision note (2026-11-20, fifth and sixth): Milestone 2 keeps interrupted markers out of group recovery; the relay gate for concurrency moves to the single Milestone 3 constant, because a worker with the new tool schema but no `ResolveRepositories` would still leave a verdict that cannot be selected.
