# Give the terminal and the web viewer one runtime projection

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture


Mjolnir's daemon (the background control process started as `mj daemon-run`; its code is in the `mj-daemon`, `mj-controller` and `mj-server` crates) tells two kinds of client what is happening. The terminal dashboard (the `mj` TUI, crates `mj-cli` and `mj-tui`) reads a "runtime feed" over the daemon's control socket. The web viewer (the browser page served from `mj-server/src/web/viewer.js`, used mostly on phones) reads a separate feed over HTTP server-sent events. Both feeds start from the same daemon capture, but the web feed then rebuilds many facts on its own: it consumes worker updates a second time, keeps its own caches, and computes its own answers to questions such as "can this session be interrupted" or "is this target available". The terminal computes its own answers to the same questions, and `viewer.js` re-derives several of them again in JavaScript.

The result is that the two surfaces can disagree about the same session at the same moment. Issue #1372 is one known case: with Docker stopped, an unreachable SSH host, or a profile whose agent was uninstalled, the terminal's new-session picker correctly leaves the entry out while the web viewer still offers it. Issue #1596 is another: the terminal's Refresh button re-samples disks in the terminal process, while the Disks view shows the daemon's readings, so Refresh appears to do nothing. The repository's engineering rules (`AGENTS.md`, "Concurrency") treat two predicates for the same question as a bug even while they agree, because they eventually drift.

After this plan, one daemon-owned projection decides every session, target and profile fact and every action verdict. The terminal and the web viewer read that projection through one history mechanism and shape it only for presentation. A user can see this working by putting a session, a target or a profile into an awkward state (Docker stopped, a host unreachable, a sub-agent waiting on a question, a failed Move) and checking that the terminal and the web viewer offer exactly the same actions and list exactly the same entries. The browser and API explanations omit private probe details.

This plan does not add a new public API. A later plan will publish this projection as a versioned `/api/v1` stream for non-browser clients such as a desktop app; that work depends on this one and is recorded under Outcomes.


## Progress


- [x] (2026-11-20 16:40Z) Reconnaissance: compared both pipelines and listed every fact computed twice. Findings are folded into Context and Orientation and the Decision Log. The full report, with file and line references at commit 818e11d0e, is in the parent session's report directory `.mj/agents/08c56cb5893e5e88dd4ff2d329501203/report.md` (not checked in; everything needed is restated here).
- [x] (2026-11-20 17:09Z) Milestone 1: the daemon publishes launch availability for the terminal, web viewer and options API (#1372). Targeted tests, all 34 terminal goldens and Clippy passed. Live validation used an isolated Docker command fixture for a stopped service, and a second daemon with Docker absent from PATH.
- [x] (2026-11-20 17:35Z) Revalidated Milestone 1 with Milestone 2 on protocol 114. The merged tree passed the targeted Rust tests, all 35 terminal goldens, 97 web unit tests, 70 Chromium picker tests, Clippy, and both named-instance cases. The browser and API reports omitted private probe diagnostics.
- [x] (2026-11-20 17:46Z) Rechecked Move selection after review. The browser requires an explicit replacement when an unlocked draft loses a choice, and public engine failures name the engine and what to check. Targeted Move tests, daemon tests and the protocol 114 live cases passed.
- [x] (2026-11-20 17:57Z) Verified that a strict v1 client can still read profile entries. Revalidated saved launch retry, sharing of unchanged availability snapshots, and the adjusted target-only curl checks on protocol 114.
- [x] (2026-11-20 18:06Z) Verified that a terminal Move starts a fresh preparation if a feed update removes its selected account, whether the old preparation is pending or complete.
- [x] (2026-11-20) Implement Milestone 2: one capacity sampler, and Refresh reaches the daemon (#1596).
- [x] (2026-11-20) Verify Milestone 2 in `feeds-m2` with an isolated mbx config.
- [x] (2026-11-20) Milestone 3: built viewer rows from the runtime capture and verified a real parent/child session pair on the browser stream in `feeds-m3`.
- [x] (2026-11-20 18:37Z) Milestone 3: revalidated after merging `origin/master`. The phone golden, owner and viewer regressions, runtime feed tests, API wait and turn tests, Clippy, and a live browser Interrupt All action passed.
- [x] (2026-11-20 18:50Z) Milestone 3: verified the review fixes for persisted titles, launch preferences, and storage invalidation. The browser published all three restored values in `feeds-m3`, and the named daemon and workers stopped.
- [x] (2026-11-20 19:06Z) Milestone 3: verified that a stopped worker keeps its last committed title, that a rejected projection page rolls its title back, and that full prompt bodies stay within the browser capture.
- [x] (2026-11-20 19:20Z) Milestone 3: a headless daemon restarted with malformed stored queue data. It logged and skipped the affected queue for startup while leaving the SQLite row available for recovery.
- [ ] Milestone 4: one verdict per action and per session state.
- [ ] Milestone 5: one history for both feeds.


## Surprises & Discoveries


- Observation: the two feeds already share their starting point. Both the terminal's control request and the web viewer's publication task call the same `capture_runtime` function, and both take revision numbers from the same counter. The duplication is in what each side does after the capture, not in the capture.
  Evidence: `mj-daemon/src/daemon/feed.rs` (`RuntimeHistory`, `capture_runtime` callers) and `mj-daemon/src/server_runtime/run.rs` (the phone publication loop) at 818e11d0e.

- Observation: the web viewer consumes worker updates before the runtime owner commits them. The daemon's process loop mirrors each raw worker view to the phone task first, then installs it in the runtime owner. A web publication can therefore combine operational facts from one instant with attention from another.
  Evidence: `mj-daemon/src/daemon/process.rs` around lines 849–865 and `mj-daemon/src/daemon/owner.rs` around lines 277–312 at 818e11d0e.

- Observation: the web viewer rebuilds only rows it marks dirty, and some facts a row shows depend on things that never mark it. A parent's Interrupt All offer depends on its children's activity, but a child-only update does not dirty the parent; the "change workspace" offer depends on how many workspaces exist, but creating a workspace dirties no row; a storage-only refresh rebuilds capacity metadata but not the per-row storage problem.
  Evidence: `mj-daemon/src/server_runtime/publication.rs` lines 141–224 and 277–287, `run.rs` lines 499–509 and 579–623, `snapshot.rs` lines 278–284 at 818e11d0e.

- Observation: Milestone 1 also touches an existing HTTP admission rule. `InstalledEngines::runtime_missing` currently supplies the engine-installation fact that lets HTTP validation refuse explicitly named targets before enqueueing create, resume, and Move requests. The web Targets view also uses it to omit default candidates for engines that have never been installed. Replacing that fact with a demand-driven verdict changes admission before the first picker check, and using a general missing verdict in its place changes which requests get refused. The Decision Log records how those consumers retain the installation fact.
  Evidence: `mj-daemon/src/server_runtime/snapshot.rs` lines 483–508 and 564, `mj-server/src/server/validation.rs` lines 183, 607, 694 and 1064–1075, `mj-server/src/server/api/sessions.rs` lines 102 and 367, and `mj-server/src/web/viewer.js` lines 5863–5868 at acd73e9d9.


- Observation: the probe must retain the same target definitions that the daemon installs, including built-in runtime candidates. Automatic machine-runtime additions return their inserted definitions from the configuration transaction, so a later edit of a new target cannot inherit its probe result.
  Evidence: `mj-setup/src/setup/availability.rs` and `mj-setup/src/setup/machines.rs`.


- Observation: before Milestone 2, both capacity samplers used `Controller::deployment_capacity_targets`. Its host list comes from configured targets, and its session-dependent paths and EC2 instance probes use active records. The daemon already supplies those records. The existing daemon refresh triggered sampling of the old target definitions.
  Evidence: `mj-controller/src/controller/backend.rs`, `mj-daemon/src/daemon/views.rs`, and the pre-change `CapacityFeed::request_refresh` at acd73e9d9.


- Observation: the phone's mirrored session manager supplied both raw views and a request bridge. Removing its view stream also removes the registrations that made bridge handles usable. The phone now sends commands to the daemon's existing manager. The web action and background-task admission scopes still hold until the operation returns.
  Evidence: `mj-session-runtime/src/session_manager/remote.rs` and `mj-daemon/src/server_runtime/run.rs`. A live browser stream in `feeds-m3` saw the parent's Interrupt All offer go from false to true and back to false after a child started and completed its assignment. The isolated daemon and workers were stopped.

- Observation: retired relay actors still need to leave queue and activity behind for the web viewer. Bootstrap loads the last durable facts, and the runtime owner keeps those facts when a relay actor retires. Family-scoped terminal feeds continue to omit the retired actor's view.
  Evidence: `mj-daemon/src/daemon/owner.rs`, `mj-client/src/runtime_feed.rs`, and the existing regression for retiring a relay actor.


- Observation: the former viewer relay consumer persisted harness and provisional titles from worker views. The relay actor now saves each title in the same transaction as its projection page, including the last page read during checkpoint or suspend. It backfills titles from older projections when it attaches to an already-current worker. The captured controller state also includes the saved Oracle choice and the recent directories used by the browser launch wizard.
  Evidence: `mj-daemon/src/daemon/snapshot.rs`, `mj-store/src/database/sessions.rs`, and `mj-daemon/src/daemon/feed.rs`.


## Decision Log


- Decision: unify facts before unifying mechanisms. Milestones 1–4 give each duplicated fact one owner; Milestone 5 then replaces the web viewer's own snapshot history with the terminal's.
  Rationale: moving the web viewer onto the terminal's history first would carry its stale and duplicated facts along unchanged. The two small fact fixes (#1372, #1596) are user-visible bugs that can ship on their own first.
  Date/Author: 2026-11-20, parent session.

- Decision: where the terminal and the web viewer disagree today, the terminal's semantics win unless noted, because they are the more complete ones. Failed Closing or Destroying with no remaining operation is a settled failure, not a transition. Interrupt All covers managed children that are waiting or interrupted and cancellable native (harness-started) children. The Move offer respects the move-queue admission hold. Changing workspace is offered when another workspace exists and no move-queue hold applies; an ordinary checkpoint does not block it. Execution admission (the daemon's check when the action actually runs) still refuses on its own terms.
  Rationale: each of these is a case where the web viewer offers something the daemon cannot do, or omits work the user asked to stop.
  Date/Author: 2026-11-20, parent session.

- Decision: presentation stays at the edge. Where a session is placed (for example, a stopped session with an unread important notification appears in the terminal's Sessions list but only in the web inbox), how a count is drawn, and which palette commands are local stay per surface. Only verdicts move into the projection.
  Rationale: those differences are deliberate design choices, not two answers to one question.
  Date/Author: 2026-11-20, parent session.

- Decision: an availability answer is one of three values per entry: available, missing with a reason, or unknown. Unknown means no check has completed against the entry as it is configured now. Unknown entries are offered, exactly as the terminal does today ("until an answer arrives the configuration stands").
  Rationale: "unknown" and "missing" must not collapse; a fresh daemon or an edited profile must not hide entries.
  Date/Author: 2026-11-20, parent session.

- Decision: the availability check keeps adding newly detected profiles and runtimes to the configuration, as it does today, and still runs only when a client asks (a dashboard starting, or a new-session or resume picker opening on either surface). The daemon does not run it on its own at startup.
  Rationale: running discovery at daemon start would edit configuration on headless daemons that no user is looking at. The check already runs inside the daemon (`DaemonAction::CheckAvailability`), so only its owner and its readers change.
  Date/Author: 2026-11-20, parent session.

- Decision: the presence of a local engine command and the result of probing a runtime are different facts. The existing `InstalledEngines` classifier moves to runtime capture and remains the owner of the first fact. HTTP create, resume and Move still refuse an absent engine before enqueueing, and the web Targets view still omits implicit runtimes whose engine is absent. The availability projection also uses that classifier to mark a target missing before any check if its command is absent. The pickers and launch options get their launch verdicts from the projection, and HTTP admission lets execution preflight judge probe-dependent failures.
  Rationale: an absent command is definitive. A probe may be stale or unknown and does not justify broadening admission refusals.
  Date/Author: 2026-11-20, parent session.

- Decision: browser rows and launch-option target entries carry public explanations for the same availability verdicts. Terminal diagnostics retain the daemon-side details. Browser explanations identify the failed check and where to investigate it, without sending profile paths, configuration excerpts, SSH destinations, or subprocess diagnostics.
  Rationale: a launch diagnostic can quote a credential from a malformed configuration file or from command output. Both surfaces must offer the same entries.
  Date/Author: 2026-11-20, Milestone 1 review.

- Decision: `/api/v1/options` publishes target availability through the existing status and reason fields. Its profile entries retain their `{id, harness}` shape. Both pickers still read profile verdicts from the runtime projection.
  Rationale: existing v1 clients strictly decode profile entries. Publish profile verdicts in the planned versioned projection stream to avoid an interim opt-in parameter.
  Date/Author: 2026-11-20, parent session.

- Decision: project-source resolution and quota surfacing are out of scope. Both surfaces call the same resolver and the same quota functions; only cache timing differs.
  Rationale: they are not two answers to one question, and including them would grow Milestone 3 without fixing a visible defect. Recorded under Outcomes as follow-ups.
  Date/Author: 2026-11-20, parent session.

- Decision: control protocol numbers are assigned when a milestone lands, not when work starts. Each milestone that changes the control wire adds a named constant one above the current `PROTOCOL_VERSION` in `mj-client/src/daemon.rs` (the existing pattern, for example `QUIESCENT_ADOPTION_PROTOCOL = SUSPEND_ONLY_CHECKPOINT_PROTOCOL + 1`) and points `PROTOCOL_VERSION` at it. Milestones developed in parallel are renumbered in landing order.
  Rationale: protocol numbers must rise in the order features land.
  Date/Author: 2026-11-20, parent session.


- Decision: publish the existing `HostCapacityRow` with daemon-owned refresh status in runtime metadata. Both surfaces read the capacity service and storage board. The service publishes each requested generation together with its newly derived probe definitions.
  Rationale: the existing row carries the sample clock, failure, and last good reading used to render staleness. Pairing probe definitions with their refresh generation ensures an in-flight probe for the old mbx paths cannot complete the new request. Each capture uses one storage snapshot for the Targets row and session warnings. Unchanged host rows share their allocation across retained revisions. The capture reuses the rows while the target, reading and storage publications are unchanged. Both surfaces read the daemon's pending refresh generation, and every wake of the sampler uses its latest inputs.
  Date/Author: 2026-11-20, Milestone 2 implementation.


- Decision: rebuild rows by dependency. Compare captured per-session entries and invalidate their parents when child entries change. Invalidate descendants for changes to an ancestor record or relationship they use for checkout and project identity. Rebuild every row when config or workspace count changes. A storage refresh invalidates only rows whose warning changes, so sampling timestamps and a full filesystem on an unused host do not rebuild any rows.
  Rationale: a synthetic store of 1,200 records (200 active views, 1,000 stopped sessions, two profiles and one configured target) took 12.78 ms median and 13.19 ms p95 to rebuild all rows, versus 0.20 ms median and 0.22 ms p95 to publish one changed active view. This was 30 warm dev-profile samples on hel timed around row publication, excluding capture and HTTP serialization. Keep dependency-based selection because worker revisions can arrive many times a second and the existing test covers 100,000 retained sessions.
  Date/Author: 2026-11-20, Milestone 3 implementation.

- Decision: workspaces, capacity and storage are capture inputs. Only the runtime revision watch wakes the viewer for them. Retain the separate quota watch as a wake trigger leading to a fresh capture, keeping quota surfacing out of scope.
  Rationale: the daemon already publishes revisions from workspace and capacity changes, including storage-only publications. Session storage problems and host capacity rows now use the same storage snapshot.
  Date/Author: 2026-11-20, Milestone 3 implementation.


## Outcomes & Retrospective


Milestone 1 is implemented and validated in its detached worktree. Both pickers showed unchecked profiles and targets, then hid the same missing entries. The options API reported the target verdicts. A configuration edit restored the unchecked choice in both pickers. The running check accepted demand from both surfaces and repeated once. Without the Docker command, both pickers hid the target immediately and the API returned HTTP 400 on explicit create. The host has no installed Docker command, so the stopped-service case used a command fixture with the actual Docker preflight and daemon. Both isolated daemons were stopped.

Milestone 2 has been validated in its worktree. The terminal renders host capacity from runtime metadata. Pressing Refresh in `feeds-m2` after changing the isolated mbx `cache_dir` added `/dev/shm` to the browser snapshot in 0.31 seconds and to the terminal Disks dropdown in 0.37 seconds. The already-open browser Targets page showed the pending refresh and then displayed the new filesystem over its feed. The browser snapshot and `mj hosts --json` agreed on the sample timestamp, CPU, used memory and available bytes. The test instance's daemon was stopped.

Milestone 3 moved the viewer's worker and session facts into the runtime capture. The test cases cover workspace and storage changes without a session record change, and a child update that publishes operational state and attention together. The full queues and forms stay in the in-process capture, and terminal clients continue reading them from the transcript tail. A browser stream in `feeds-m3` showed the parent change from no Interrupt All offer to offering it after a child started. The browser action was accepted and the existing stream withdrew the offer when the child stopped. The named daemon was stopped and its workers were reaped.

Milestone 3 review follow-ups cover the retained allocation of browser prompt bodies in terminal replay history (Milestone 5), the idle verdict for a parked child whose relay actor has retired (Milestone 4), and comparing storage warnings only for sessions on affected hosts.

Follow-ups: publish the unified projection as a versioned `/api/v1` stream including profile availability and "transcript advanced to sequence N" notices (a separate plan, the prerequisite for a desktop client); give project-source resolution one cache in the runtime owner; give quota surfacing one cache; revisit the browser Move picker’s implicit destination when all compatible destinations have a missing verdict. The existing profile-only Move can omit a destination and relies on execution preflight to check the implicit one.


## Context and Orientation


A "session" is one coding-agent conversation that Mjolnir runs. A "worker" (`mj-worker`, one per session) runs the agent's harness and sends the daemon a stream of updates about it: whether a turn is running, queued prompts, open questions, and so on. A "sub-agent" or "child" is a session that another session started; a "native" child is one the harness started itself. A "target" is a launch template that says where a session runs (this computer, a container engine such as Docker or Podman, an SSH host, EC2); a "profile" is a configured harness plus login. "Attention" is the daemon's judgment of whether a session needs a person (for example, an unanswered question).

The daemon's "runtime owner" (`mj-daemon/src/daemon/owner.rs`, `RuntimeStateOwner`) holds the current worker view for every session. A "capture" (`capture_runtime`, called from `mj-daemon/src/daemon/feed.rs`) takes an immutable snapshot of the owner's state plus durable records, configuration, workspaces, quotas, capacity, reviews and notices, and produces a `RuntimeProjection` (defined in `mj-client/src/runtime_feed.rs`). A "revision" is the number the daemon's shared counter (`RuntimeRevisions`, `mj-daemon/src/daemon.rs`) assigns to a state change.

The terminal feed works like this. `RuntimeHistory` in `mj-daemon/src/daemon/feed.rs` keeps up to 4,096 captured projections (or 16 MiB of encoded change estimates) with its own sequence number. A terminal holds a `RuntimeCursor` (daemon incarnation, sequence, which sub-agent families it has opened, and a durable alert position) and long-polls the control socket; the daemon answers with a `RuntimeDelta` (keyed replacements and deletions) from the cursor's base, or a fresh snapshot if the base is gone. The client keeps a `RuntimeReplica`. The control socket speaks length-prefixed JSON; its version is `PROTOCOL_VERSION` in `mj-client/src/daemon.rs` (111 at the start of this plan), and ordinary requests require an exact match. `mj-daemon/src/pollers/remote.rs` is the terminal's adapter that applies these deltas and feeds the dashboard and embedded chat (`mj-chat`).

Before Milestone 3, the web feed worked like this. The phone publication loop in `mj-daemon/src/server_runtime/run.rs` calls the capture, then joins it with its own inputs (the `PhoneSessionViews` struct: native agents, conversations, queued prompts, user shells, pending questions, prompt image support, operational flags, materialized activity, project sources, operations, move recoveries, capacity, launch failures, reviews), builds a `ViewerSnapshot` (`mj-daemon/src/server_runtime/snapshot.rs`, `mj-server/src/server/viewer_types.rs`) and sends it on a watch channel. `mj-server/src/server/viewer_feed.rs` keeps its own history of the last 128 snapshots it served, with its own incarnation, and serves `GET /api/snapshot` and `GET /api/events?format=changes` (handlers in `mj-server/src/server/handlers.rs`). `mj-server/src/server/viewer_wire.rs` encodes rows, an intern table for repeated values, and "detail changed" notices. Many HTTP handlers, including the stable `/api/v1` ones under `mj-server/src/server/api/`, also read the latest `ViewerSnapshot` to validate requests.

The facts computed more than once, and where, are these. Target and profile availability: the terminal asks the daemon to run `mj_setup::setup::check_availability` (`mj-setup/src/setup/availability.rs`) through `DaemonAction::CheckAvailability` (handled in `mj-daemon/src/daemon/machine.rs`), then applies its own predicates in `mj-tui/src/availability.rs` (`profile_missing`, `target_offered`, `target_missing`); the web snapshot uses `InstalledEngines::runtime_missing` (a PATH check) and host capacity in `snapshot.rs`; `/api/v1/options` (`mj-server/src/server/api/options.rs`) adds `LocalEngineChecks`, a separate cached probe of local engines, through `target_availability`. Host capacity: the terminal starts its own sampler (`spawn_dashboard_capacity_poller`, called from `mj-cli/src/dashboard.rs`) while the web viewer reads the daemon's `CapacityFeed` (`mj-daemon/src/pollers/capacity.rs`, `mj-daemon/src/daemon/capacity.rs`); the terminal's Refresh re-samples locally (`mj-cli/src/dashboard/surface.rs`, `request_capacity_refresh`). Session verdicts: the web viewer's `session_capabilities` in `snapshot.rs`, the terminal's action gates in `mj-tui/src/actions.rs`, `transition_kind` in `mj-tui/src/dashboard_standby.rs`, working and unfinished child counts in `mj-tui/src/dashboard_sessions.rs`, and in `viewer.js` the functions `isTransitioningSession`, `isDashboardSession`, `resumeMoveRecovery` and the working-child count, plus `viewer_wire::is_summary` in Rust.


## Plan of Work


### Milestone 1: one availability owner (#1372)


At the end of this milestone the daemon owns the answer to "is this profile or target available" and publishes it on the runtime feed. The terminal pickers and the web viewer's new-session and resume pickers read both verdicts. `/api/v1/options` reads the target verdicts.

Add a daemon-owned availability state, kept with the other daemon-owned facts that a capture reads (a new module `mj-daemon/src/daemon/availability.rs` is a reasonable home). It is a small state machine: not yet checked; checking, with a flag that one more check was requested while it ran; or checked, holding the last report and, for each profile and target, the configured value it was checked against. A request while a check runs sets the flag and does not start a second check; when the running check ends with the flag set, exactly one more check starts. The check itself is the existing `check_availability`, run on a blocking thread under the daemon's supervised task helpers, never on an event loop. A failure is kept and published (the terminal shows "Could not check what is available: …" today and must keep showing it once per distinct failure). Entries the check added (`added_profiles`, `added_runtimes`) are announced through the daemon's existing public notices so both surfaces show them.

From the report and the daemon's current configuration, compute one verdict per enabled profile and per configured target: available, missing with the reason text the terminal shows today, or unknown when the entry's current configuration differs from what was checked or nothing has been checked. Move the predicate logic now in `mj-tui/src/availability.rs` (`profile_missing`, `target_offered`, `target_missing` and the `Missing` reasons) into the daemon side so it runs once; the terminal keeps only rendering. Publish the verdicts in the capture (in `RuntimeMetadata`, since they are not per session) and copy them into the web snapshot's target and profile rows.

Replace the terminal's request-and-wait use of `DaemonAction::CheckAvailability` with a request that only asks for a check; the answer arrives on the feed. The terminal still holds its request while first-run setup has not decided whether the configuration is new (the `held` flag in `mj-tui/src/availability.rs`). Add a web viewer action that asks for a check when the new-session or resume dialog opens, and make `viewer.js` build its pickers from the published verdicts, leaving out missing entries and naming them with their reasons the way the terminal does. Make `/api/v1/options` report the same target verdicts through its existing fields. Delete `LocalEngineChecks` and the engine probe inside `target_availability`, and stop using `InstalledEngines::runtime_missing` to decide what is offered (keep it only if another caller still needs the "is the engine command installed" fact, and say which in this plan).

This changes the control wire, so add a protocol constant as described in the Decision Log.


### Milestone 2: one capacity sampler, and Refresh reaches the daemon (#1596)


At the end of this milestone the terminal no longer samples host CPU, memory or disks itself. It shows the daemon's samples, the same ones the web viewer shows, and its Refresh button asks the daemon to re-derive its probe targets (including re-reading the mbx build-cache configuration) and sample again.

Publish the daemon's capacity samples, with their staleness, in the capture's metadata if they are not already there in the form the terminal renders (`mj-tui/src/render/capacity.rs`). If the terminal samples any host the daemon does not, extend the daemon's probe targets so the daemon covers it. Remove `spawn_dashboard_capacity_poller` and its plumbing from `mj-cli/src/dashboard.rs`. Add a control request that maps onto the daemon's existing refresh (`ControllerAction::RefreshCapacity` today, reachable only from the web viewer) and have the terminal's Refresh send it; the new readings arrive on the feed. This changes the control wire; add a protocol constant.


### Milestone 3: web rows built only from the runtime capture


At the end of this milestone every fact in a web viewer row comes from one runtime capture, and the web viewer no longer consumes worker updates on its own.

Remove the mirror that sends raw worker views to the phone task before the runtime owner commits them (`mj-daemon/src/daemon/process.rs`, and the consuming branches in `mj-daemon/src/server_runtime/run.rs`). Every fact the web viewer took from that mirror (queued prompts, open questions and forms, user shells and tasks, prompt image support, operational flags and configuration, turn controls, materialized activity) must be present in the runtime owner's per-session view (`RuntimeSessionView`) and read from the capture; add what is missing there rather than keeping a separate map. The phone publication loop then wakes on the runtime revision watch, plus its genuinely client-local inputs (results of its own actions, launch failures for requests that never produced a session, and conversation projection jobs).

Decide which rows to rebuild from what changed between two captures, so that the dependency gaps in Surprises & Discoveries cannot happen: a row is rebuilt when its own keyed entries change, when any entry of its children changes, or when any metadata field it reads changes (workspace count, storage status). If the measured cost of rebuilding every row on every capture is small on a store with about 1,200 sessions, rebuilding every row is acceptable and simpler; record the measurement here either way.


### Milestone 4: one verdict per action and per session state


At the end of this milestone the daemon computes, once per capture, a verdict record for each session: its transition kind, whether it belongs on the live dashboard, whether a person is needed, the offers for interrupt, Interrupt All, stop, restart, Move, change workspace, container settings and resume, and its working and unfinished child counts. The terminal and the web viewer read these verdicts and no longer compute them.

Compute verdicts in the daemon from the capture, family by family so a parent's verdicts see its children. Use the semantics chosen in the Decision Log. Where an action has an execution path (for example Interrupt All in `mj-daemon/src/server_runtime/actions.rs`), make the offer and the execution use the same function, so the button and the action cannot disagree about which sessions are covered. Publish the verdicts per session in the runtime projection and copy them into web rows. Delete the re-derivations listed in Context and Orientation in `mj-tui` and `viewer.js`, and make `viewer_wire::is_summary` read the published membership. This changes the control wire; add a protocol constant.


### Milestone 5: one history for both feeds


At the end of this milestone the web viewer's cursor is a `RuntimeHistory` cursor and `viewer_feed.rs`'s separate 128-snapshot history and incarnation are gone.

Give `RuntimeHistory` a browser view: the redacted public `ViewerSnapshot` built from a retained capture, cached the way terminal scoped views are cached. The web events handler waits on the runtime revision watch the same way the terminal long-poll does and encodes deltas between two browser views with the existing `viewer_wire` encoding. The server-sent event ID keeps its `incarnation:revision` text form, now carrying the runtime incarnation and sequence; `viewer.js` needs no change beyond any it already makes for resets. HTTP handlers that read the latest `ViewerSnapshot`, and the transient activity frames of `/api/v1/events`, read the latest browser view. The durable `/api/v1/events` log and its numeric IDs do not change. After a daemon replacement both feeds see a new incarnation and send a fresh snapshot, as today.


## Concrete Steps


Work from the repository root. Build and test on the dev profile. Run `cargo test` outside the restricted sandbox, because the suite uses loopback sockets. Run only the tests for what changed: for example

    cargo test -p mj-daemon --lib -- availability
    cargo test -p brokk-mj-tui golden
    cargo clippy -p mj-daemon -p mj-server -p mj-tui -p mj-cli --all-targets -- -D warnings

When a milestone changes dashboard layout or a shared terminal renderer, run every golden test in `brokk-mj-tui` (filter `golden`), not one filter. Test the running system only in a named instance, for example `mj --instance feeds-m1 ...`, and stop its daemon afterwards with `mj --instance feeds-m1 daemon stop`. A binary run from a Cargo target directory refuses to touch the default instance; do not work around that refusal.


## Validation and Acceptance


Milestone 1 is accepted when, in a named instance with Docker installed but its service stopped, one SSH machine configured with an unreachable address, and one native-login profile whose agent home and command were removed, the terminal's new-session picker and the web viewer's new-session picker leave out the same three entries and name the failed checks, with private diagnostic details restricted to the terminal, and `curl` of `/api/v1/options` with the instance's API token reports Docker and SSH as missing. The profile verdict is checked in both pickers. Before the first check completes, both pickers list all three (unknown is offered). Editing the profile so it differs from what was checked makes it unknown until the next check. With no Docker command on PATH, both pickers hide its target before the first check and an explicit create through `/api/v1` returns the existing HTTP 400.

Milestone 2 is accepted when, after editing the mbx `cache_dir` and pressing Refresh in the terminal, the Disks view shows the new filesystem without waiting for the daemon's next cycle, and the web viewer shows the same reading; and when no capacity sampler runs in the terminal process.

Milestone 3 is accepted when the three scenarios in Surprises & Discoveries no longer produce stale rows: a child's activity changing updates the parent's Interrupt All offer in the web viewer, creating a second workspace updates every row's change-workspace offer, and a storage-only refresh updates the per-row storage problem.

Milestone 4 is accepted when, for each disagreement scenario in the Decision Log (a failed Destroy with no remaining operation, a parent whose only unfinished work is a cancellable native child, a failed Move holding its queue, an ordinary checkpoint during a workspace edit), the terminal and the web viewer offer the same actions, and Interrupt All from either surface stops the same sessions.

Milestone 5 is accepted when a web viewer reconnecting with a cursor within the retained history receives a delta, one with an evicted cursor receives a snapshot, and after a daemon replacement both the terminal and the web viewer receive fresh snapshots and continue updating.

Tests follow the "Which tests to add" rules in `AGENTS.md`: golden renders of the terminal pickers and web viewer output where those exist, end-to-end runs in a named instance, and tricky-condition tests for unknown versus missing, single-flight checks with one rerun, and stale-dependency cases. Do not add tests that only restate a computed value.


## Idempotence and Recovery


Every milestone is a code change with no database migration, so it can be reverted by reverting its commit. Protocol constants are renumbered in landing order; a milestone that lands after another must take the next number. If a milestone is abandoned partway, revert its commits rather than leaving a half-moved fact with two owners.


## Artifacts and Notes


None yet.


## Interfaces and Dependencies


No new crates or external libraries. The shared types live where the current ones do: runtime feed types in `mj-client/src/runtime_feed.rs`, availability types in `mj-client/src/availability.rs`, web snapshot types in `mj-server/src/server/viewer_types.rs`. At the end of Milestone 1 a per-entry availability verdict type with three cases (available, missing with a reason string, unknown) must exist in `mj-client` and be what both `mj-tui` and the web snapshot read. At the end of Milestone 4 a per-session verdict record must exist in `mj-client/src/runtime_feed.rs`, published in `RuntimeProjection` and copied into `ViewerSession`.


Milestone 2 update, 2026-11-20. The plan now records the shared target coverage, why the refresh request must rebuild targets, how in-flight probes answer a refresh, and the named-instance outcome.

Milestone 3 update, 2026-11-20. The viewer reads the captured runtime state. The plan records the measured row selection cost, the remaining quota wake, and the named-instance browser result.
