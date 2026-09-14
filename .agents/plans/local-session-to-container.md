# Move or resume a local session into a container


This ExecPlan is a living document maintained in accordance with `.agents/PLANS.md`. Keep `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` current throughout implementation. All paths below are relative to the repository root (`/home/jonathan/Projects/hel2` at authoring time). Commit each validated checkpoint on the current branch; do not push unless asked.


## Purpose / Big Picture


A "session" is one conversation with a coding agent (Claude Code, Codex, and so on) that Mjolnir supervises. A "local session" (also called a "raw" or "bare" session in the code) runs the agent directly in a directory on this machine. An "isolated session" runs the agent in a fresh clone inside a container (Podman, Docker, Apple container), an SSH host, or an EC2 instance; the code calls these "workspace" or "bundle" sessions.

Today a user can move or resume an isolated session onto a local directory, but not the other way. Choosing a container target for a local session shows "raw sessions do not have isolated network repository provenance". After this change, a user can pick **Move…** on a running local session, or **Resume** on a stopped one, choose a container target, read a short preview of what will happen (which remote is cloned, which branch, how many unpushed commits and dirty files will be copied), confirm, and then continue the same conversation inside the container with the same commits and dirty files present at `/workspace/<directory name>`.

The checkout must have a network Git remote (GitHub or any `https`/`ssh` URL). A local repository with no remote stays bare-only and the picker says so.


## Progress


- [x] Checkpoint A (2026-09-14): compatibility gate, network-remote resolution in the conversion plan, host checkout snapshot builder, and `RawConversionPreview` computation, with real-Git tests.
- [x] Checkpoint B (2026-09-14): resume wiring (conversion archive, checkpoint swap, restore of repository content, notice, rollback test) and move preparation carrying the preview.
- [x] Checkpoint C (2026-09-14): surfaces (TUI review lines and resume confirm dialog, web resume preflight and move dialog, `mj move` output), user docs, and this plan's closing sections.


## Surprises & Discoveries


The controller still contains a complete `ResumePlan::RawToWorkspace` conversion, but it was never reachable: `resume_compatibility` in `mj-client/src/target.rs` never returns it. Its downstream expected repository content to be "seeded from its own checkout" (comment in `mj-controller/src/controller/resume.rs` near `restore_repositories: (resumed_project_directory.is_none() && conversion.is_none())`). That seeding mechanism was removed by `.agents/plans/remote-only-isolated-sessions.md`, so today a converted session would land in an empty default-branch clone.

`resume_session_controlled_with_repository_preflight` calls `network_git::bundle_from_manifest` on the stored archive before it plans any conversion, and that call refuses an archive without network repository provenance. Checkpoint A therefore changes no session at runtime even though the gate now returns `RawToWorkspace`: a converting resume still stops at that archive check, with the record and configuration untouched. Checkpoint B has to write the conversion archive and point the record at it before that check, or move the check after the conversion.

A fixture cannot put `url.<path>.insteadOf` in the checkout's own Git configuration, because `git remote get-url` applies `insteadOf` rewrites: `resolve_local_repository` would then read the local path and refuse it as a non-network URL. The tests record the network URL on the remote and rewrite only the commands that really contact it (`ls-remote`, `fetch`), which is what `FixtureRemoteExecutor` does.

`MovePreparation.conversion` is `Option<Box<RawConversionPreview>>`, not the unboxed `Option` this plan's Interfaces section first described. `MoveSessionRequest` is a variant payload of `DaemonAction` and of `ControllerAction`, and the unboxed preview pushed both enums past clippy's `large_enum_variant` threshold. Boxing one field was smaller than boxing the payload in every enum that carries it, and the serialized form is identical either way.

The conversion snapshot uses the repository id `project`, which a raw checkpoint has always used, while `converted_raw_bundle` synthesizes a bundle whose repository id is derived from the checkout's name. Checkpoint B must write the conversion archive's `BundleManifest.primary_repository` as `project` so `bundle_from_manifest` can still find the primary repository.

Pointing the session record at the conversion archive is not enough. The resume
uploads and restores from its own local `archive_path` variable, not from the
record, so a conversion also has to route the restore specification and the
archive upload to the new file; otherwise the container would clone the right
remote and then restore the raw archive's content over it.

`move_confirmation` runs twice for one move: once in `prepare_move_session_controlled`
and again in `move_session_managed_controlled`, which overwrites the prepared
fingerprint and compares it with the one the person confirmed. The conversion
fields therefore enter the digest through a parameter both call sites pass;
computing them in only the preparation would make every raw move fail its own
fingerprint check.

A failed converting resume leaves behind the bundle it installed in the
configuration. The rollback restores the record, including its bundle id, but
nothing removes a configured bundle, and `converted_raw_bundle` reuses a
matching one, so retries do not pile up. The rollback test asserts exactly that
instead of "the configuration is unchanged".

`reconcile_managed_checkpoint_archives` only recognizes archives named
`<session>-<frontier>-archive-<32 hex>.hel.zip`, and they live in
`sessions_dir()` (the data directory's `sessions`), not `archives`. The
conversion archive keeps that shape and distinguishes itself inside the session
segment, as `<session>-converted-<frontier>-archive-<nonce>.hel.zip`, so an
interrupted conversion's file is still reconciled away.

A restore reads nothing from `TargetManifest` beyond validating that its
`details` carry no secrets, so the conversion archive keeps the previous
archive's target and bundle identity and changes only
`BundleManifest.primary_repository`, which is how `restore_checkpoint` and
`bundle_from_manifest` find the primary repository, its destination, and the
working directory the native transcript is rewritten to.

`raw_checkout_snapshot` refuses a checkout with no origin refs (through
`repair_origin_refs`), so a conversion of a checkout that has never pushed fails
before provisioning. Fixtures push their base commit to the fixture remote.

A managed worktree's archive bundles commits since its creation commit (`DeltaFrom{base_commit}`). If that creation commit was never pushed, a fresh network clone cannot fetch the bundle because Git bundles require their prerequisite commits to exist. Bundling "commits not on any origin ref" (`GitHistoryMode::SessionDelta`) avoids this because every prerequisite is then on the remote.


`ResumeRepositorySourcePreflight` lives in `mj-controller/src/controller/resume.rs`,
not in `mj-core/src/state.rs` as the Interfaces section said, and it carries no
serde attributes; only its `Ready` payload,
`ResumeRepositorySourceReceipt`, is a `mj-core` type. The new
`ConvertingRawCheckout` variant therefore went beside the others in the
controller and carries the preview boxed, and the browser gets its own
serializable answer (`server::PreflightResume`) instead.

Computing the preview inside `preflight_resume_repository_sources` broke
Checkpoint B's rollback test. The resume itself re-runs that preflight whenever
it has no current receipt, and it only reads the `RepositoryMoved` case, so the
conversion's remote probe ran a second time against the real network. The
function now splits: the public entry describes the conversion, and the resume
path asks a private one with `describe_conversion: false`.

The browser's resume preflight shares the new-session preflight's channel, so
`server::PreflightRequest` became an enum of `NewPreflightRequest` and
`ResumePreflightRequest`. Sharing the channel also shares
`MAX_CONCURRENT_PREFLIGHTS` and the cancellation-on-disconnect behavior, which
a second channel would have had to duplicate.

The resume route answers `Ready` without reading anything when
`resume_compatibility` is not `RawToWorkspace`, and a conversion that cannot be
planned comes back as `Unavailable { detail }` with the plan's own message
rather than an `ApiError`: `ApiError::message` is a `&'static str`, and the
message is the part that says what to do (add a remote, commit a submodule).

The resume card had to gain a render trigger. `resumeCardSignature` ignored the
draft, so selecting a target re-rendered nothing — the `<select>` held its own
state. The signature now includes the selected target and the preflight's
answer, while the acknowledgement checkbox deliberately stays out of it and
updates the Resume button through the existing `card._invalid` path instead of
rebuilding the card under the person's cursor.


## Decision Log


2026-09-14, user: require a network remote on the local checkout. No fallback that ships full history for remote-less repositories. This keeps the rule from `.agents/plans/remote-only-isolated-sessions.md` that isolated workspaces always clone from a network remote, and narrows that plan's "raw archives cannot convert" to "raw checkouts without a network remote cannot convert". The reason the narrower rule is safe: the conversion re-snapshots the host checkout at resume time into an archive whose origin and push URLs come from the checkout's own configuration, so the container has real provenance and later checkpoints inside it work unchanged.

2026-09-14, user: both managed-worktree sessions and sessions opened directly on the user's own checkout may convert. A managed session arrives on its `mj/<session>` branch; an unmanaged one arrives on whatever branch the checkout was on, and `git push` in the container pushes that branch.

2026-09-14, user: show the same kind of precautions as similar flows. The new-session wizard previews the network clone plan and the move confirmation lists interruption facts; this feature adds a shared `RawConversionPreview` shown before anything is stopped or provisioned, with a dirty-tree warning, an unpushed-commit count, and (for unmanaged sessions) a warning that the host checkout stays behind.

2026-09-14, Fable: dirty counts are excluded from the move fingerprint. A live local session has an agent editing files, so hashing them would invalidate every confirmation. Only fetch URL, push URLs, branch, and destination are hashed.

2026-09-14, Fable: the conversion writes a new checkpoint archive and points the session record at it before provisioning, rather than teaching provisioning a second source of clone information. `rollback_failed_resume` restores the entire previous record on failure, so the old archive file must be kept until the resume succeeds.


## Outcomes & Retrospective


A local session on a whole checkout with a network remote can now be moved or
resumed into a container, and every surface says what that does before it
happens. The wording is computed once per language:
`RawConversionPreview::summary_line` and `warning_lines` in
`mj-core/src/state/session_move.rs` serve the TUI review, the TUI confirmation,
and `mj move`; `conversionSummary` and `conversionWarnings` in
`mj-controller/src/web/viewer.js` serve the move dialog and the resume card.

What each surface does. The TUI move review appends the summary and the
warnings after the existing move warnings. A TUI resume onto a container opens
a `Confirmation::ConvertRawCheckout` dialog focused on Cancel; Cancel restores
the wizard exactly, Confirm returns `DashboardAction::ConfirmRawConversion`
carrying the preflight receipt, which the CLI dispatches straight into
`start_preflighted_session_launch`. `mj move` prints the same lines to stderr
before its prompt and with `--yes`. The web move dialog renders them as
`move-warning` paragraphs; the web resume card fetches
`POST /api/preflight/resume` when the destination is not bare, shows
"Checking checkout…" while it waits, and then either an error, nothing, or the
warnings plus a checkbox that gates the Resume button.

Validation, on the dev profile from the repository root:

- `cargo test -- --quiet`: pass. 1220 controller, 436 TUI, 513 core, 415 CLI,
  and every other crate's suite green; no failures and no reruns needed (the
  known `npm_upgrade_restarts_after_the_running_package_is_removed` flake did
  not appear).
- `cargo clippy --all-targets -- -D warnings`: clean.
- `cargo fmt --all -- --check`: clean.
- `MJ_BROWSER_SPEC=resume-conversion.spec.js npm --prefix tests/e2e/web test`:
  27 unit tests and 3 browser tests pass. `resume.spec.js` (17 tests) was rerun
  to prove the resume card's existing behavior is unchanged.

New tests. `move_review_reports_what_a_local_checkout_conversion_copies_and_leaves_behind`
and `a_raw_conversion_resume_launches_only_after_it_is_confirmed`
(`mj-tui/src/wizards/tests.rs`) cover the review lines and the confirmation's
state transitions. `a_local_checkout_resuming_into_a_container_preflights_its_conversion`
(`mj-controller/src/controller/resume.rs`) proves the preflight returns the
preview for a container and asks nothing for an in-place resume, using real
Git against a fixture remote. Two `server.rs` tests cover the route's wiring
and its refusal of an unknown session. `tests/e2e/web/resume-conversion.spec.js`
covers the browser's warnings, the gated button, and the unavailable case.

What is left. Nothing in this plan's scope. The acceptance walkthrough under
"Validation and Acceptance" still needs a real Podman target and a real GitHub
remote; it was not run here, and the automated coverage stands in for it. The
`Unavailable` detail sent to the browser is the plan's own error text, which
can name a host path — the same choice the preview itself already makes by
carrying the checkout path, and worth revisiting if the viewer's path policy
tightens.


## Context and Orientation


Key files. `mj-client/src/target.rs` holds `resume_compatibility`, a pure function every target picker calls to decide whether a session may resume on a target; it returns `ResumePlan::{InPlace, RawToWorkspace, WorkspaceToRaw}` or a user-facing error string. `mj-controller/src/controller/worktree.rs` holds `plan_raw_to_workspace` (reads Git, produces `RawToWorkspaceConversion`), `apply_raw_to_workspace`, and the managed worktree helpers. `mj-controller/src/controller/resume.rs` holds `resume_session_controlled_with_repository_preflight`, the single resume path; a move is "close with checkpoint, then this resume". `mj-controller/src/controller/move_session.rs` holds `prepare_move_session_controlled`, which produces the `MovePreparation` each surface shows before confirmation. `mj-controller/src/controller/provisioning.rs` provisions targets; for a resume it takes the clone source from the session's checkpoint archive via `network_git::checkpoint_bundle`, which requires the archive's repository metadata to have `remote_workspace: true` and a network origin. `mj-checkpoint/src/archive/git.rs` collects and restores Git snapshots (`collect_git_snapshot`, `restore_git_snapshot`, `GitHistoryMode`); `mj-checkpoint/src/archive.rs` reads and writes archives (`ArchiveInput`, `write_archive_atomic`, `read_archive_verified`). `mj-core/src/remote_git.rs` and `mj-core/src/local_git.rs` resolve a checkout's network remote (`resolve_local_repository`) and probe its default branch (`default_branch`).

Surfaces. TUI: `mj-tui/src/wizards.rs` (`render_review_wizard`), `mj-tui/src/wizards/dashboard.rs`, and `mj-cli/src/dashboard/io.rs` (handles `ResumeRepositorySourcePreflight` results). Web: `mj-controller/src/web/viewer.js` (move dialog near `preparation.cross_harness`, resume card near `function resumeCard`). CLI: `mj-cli/src/main.rs` (`mj move`). There is no `mj resume` subcommand.


## Plan of Work


Checkpoint A makes the decision and the data available without changing any session. The gate returns `RawToWorkspace` for a whole-checkout local session going to a non-bare target. `plan_raw_to_workspace` resolves the checkout's network remote and stores it in the conversion. A new function builds a `RepositorySnapshot` from the host checkout with `SessionDelta` history and network metadata (`remote_workspace: true`, `base_commit` = first boundary commit of `git rev-list --boundary HEAD --not --remotes=origin`, or HEAD when there is none). Another function computes `RawConversionPreview` (defined in `mj-core/src/state/session_move.rs`).

Checkpoint B makes the resume work. After the host checkout is present (unmanaged: untouched; managed: recreated from its retained branch and the archive), build the snapshot, write a new archive that copies the previous archive's session, target, bundle, canonical transcript, and native artifacts but replaces the repository with the snapshot, verify it, set `record.checkpoint` to it, persist the row, then continue into provisioning with `restore_repositories: true`. On success prune the replaced archive; on failure the existing rollback restores the previous record. Move preparation computes the preview and carries it in `MovePreparation.conversion`.

Checkpoint C shows the preview and warnings on every surface, adds a resume confirmation in the TUI (new `ResumeRepositorySourcePreflight::ConvertingRawCheckout` variant) and in the web resume card (a resume preflight route plus a confirm checkbox), prints the preview in `mj move`, and updates `docs/src/content/docs/sessions.md`, `containers.md`, and `targets.md`.


## Concrete Steps


Run all Cargo commands from the repository root, outside the restricted sandbox, on the dev profile:

    cargo test -p mj-client -p mj-controller -p mj-checkpoint -- --quiet
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

For web changes:

    MJ_BROWSER_SPEC=<spec>.spec.js npm --prefix tests/e2e/web test


## Validation and Acceptance


With a real Podman target configured: create a local session on a checkout that has a GitHub remote, make one commit, leave one dirty file, then run `mj move --session <id> --target podman`. The confirmation prints the fetch URL, the branch, one unpushed commit, one dirty file, and the retained-checkout warning. After confirming, `podman exec` into the container shows `/workspace/<dirname>` on the same branch with the commit and dirty file, and `git config mj.remoteWorkspace` prints `true`. Stopping and resuming back onto `local-bare`, then again onto `podman`, round-trips. On a checkout with no remote, the picker refuses with a message naming the missing remote and the local session keeps running.


## Idempotence and Recovery


Preview and plan functions read Git only. The conversion archive is a new file; the previous archive is removed only after the resume succeeds. A failed resume rolls the record back to the previous checkpoint, and a retry recomputes everything. Retrying a move re-prepares and shows a fresh preview.


## Interfaces and Dependencies


`mj_client::target::resume_compatibility` returns `Ok(ResumePlan::RawToWorkspace)` for the cases above. `mj_controller::controller::worktree::RawToWorkspaceConversion` gains `source: mj_core::remote_git::NetworkGitSource`. `mj_core::state::session_move::RawConversionPreview` is a serializable struct with `checkout: PathBuf`, `destination: PathBuf`, `branch: Option<String>`, `fetch_url: String`, `push_urls: Vec<String>`, `default_branch: String`, `unpushed_commits: u64`, `staged_files: u64`, `unstaged_files: u64`, `untracked_files: u64`, `untracked_bytes: u64`, `host_checkout_retained: bool`. `MovePreparation` gains `#[serde(default)] conversion: Option<Box<RawConversionPreview>>` (boxed for `large_enum_variant`, as recorded under Surprises). `ResumeRepositorySourcePreflight` gains `ConvertingRawCheckout(RawConversionPreview)`. No new crates.
