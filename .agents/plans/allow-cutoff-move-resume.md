# Move and Resume without later history

## Purpose

Allow sessions launched with --no-later-history to change profiles or resume while retaining the launch ancestry and their own work. No origin, remote refs or tags may remain when their worker starts.

## Progress

- [x] Map launch, retained Move, reconstructed Resume, and cross-target Move.
- [x] Obtain parent approval for controller preparation without a serialized contract change.
- [x] Share cutoff bundle preparation and workspace verification.
- [x] Validate real Git integration and retained profile Move; run targeted tests and clippy for both touched crates.
- [x] Commit the completed change.
- [ ] Complete the required worktree review and hand back the final SHA.

## Context and path map

mj-controller/src/controller/backend.rs builds launch repository specifications. targets/bootstrap.rs creates cutoff workspaces with git init and fetches only the recorded head/base, without tags or origin. provisioning.rs chooses launch, checkpoint, or transferred repository specifications. Previously the latter two omitted the cutoff. All now receive apply_session_history_cutoff before target provisioning. Target command execution supports local, SSH and container placements.

LocalBare compatibility in mj-client/src/target.rs keeps cutoff sessions in a managed bundle workspace. provisioning.rs uses the recorded worker root and targets::provision_local_bundle_plan, which calls the same clone_commands as other placements. It does not create a host-linked checkout.

network_git.rs positions the launch checkout and configures its workspace. resume.rs restore_into_target handles reconstructed and retained workspaces, then starts their worker. Its shared verification call belongs immediately before worker startup, after archive import and profile installation. Move uses this same restoration function.

## Surprises & Discoveries

Checkpoint capture uses CloneFrom(base), preserving a delta rather than all launch ancestry. Restore imports that delta from a local temporary bundle and writes origin configuration. It performs no origin fetch. Consequently restore must receive cutoff ancestry during provisioning and remove the restored origin before worker startup.

## Decision Log

The parent approved option A: apply the recorded session cutoff to all provisioning specifications, then use shared controller preparation before starting any restored worker. No schema, protocol or archive format change. The parent subsequently approved retaining the managed workspace on LocalBare, selected in one compatibility decision instead of WorkspaceToRaw. Launch positions HEAD and verifies; retained/restored workspaces verify without resetting HEAD or requiring cleanliness. Removing restore-created origin is authorized because no network fetch occurs before removal.

## Milestones

First centralize cutoff application for provisioning, removing the Move/Resume refusal. Then use the launch no-origin configuration function to check restored workspaces. Finally exercise real Git capture/restore and retained profile Move using isolated existing fixtures. Run cargo test -p brokk-mj-controller with affected filters and cargo clippy -p brokk-mj-controller --all-targets -- -D warnings outside the sandbox. Do not push. The parent approved the worker contract requirement to request review on this detached worktree after committing.

## Validation and acceptance

The real Git fixture contains later commits, a later tag, and unrelated pull request history. Launch, retain a session commit and dirty work through profile switching, capture its delta, reconstruct only cutoff ancestry, and restore. Assert session work survives, later object IDs are absent, and remotes/remote refs/tags are empty. Adding a forbidden tag must fail visibly. Existing isolated Move fixtures exercise the actual in-place state transition with real Git and a recorded worker.

## Outcomes & Retrospective

Implementation complete. Initial six cutoff tests, a 20-test focused run, and subsequent three-test regression runs passed. An accidental extra argument in an existing test call caused E0061 and was removed before rerunning. The first controller clippy pass passed. After the approved LocalBare extension, the final 20 focused controller tests passed (the selected client filters matched no standalone client tests; its public compatibility function is covered by the controller Git regression). Clippy for brokk-mj-controller and brokk-mj-client with --all-targets and -D warnings passed. Formatting and diff checks passed. Review follows the commit. No live instance touched. Cross-target bundle preparation shares provisioning policy; no live SSH or container Move is planned.

Evidence logs are stored under /mnt/optane/tmp/fix-1509-*.log. The final intended records are fix-1509-complete-tests.log and fix-1509-complete-clippy.log.

## Review correction

Both reviewers of ea0608d4d found that the post-launch all-object scan rejected legitimate amended commits and dropped stashes. The parent approved strict inventory only at launch. Later Move/Resume keep the shared no-origin/no-remote-ref/no-tag checks and rely on cutoff-only provisioning plus archive provenance to prevent mj importing later upstream history. Agent-created discarded objects are preserved; no provenance tracking is added. The retained Move regression now amends a commit and drops a stash before moving, then checks both objects survive. The existing Git regression still checks that contaminated launch ancestry fails visibly. Both affected real-Git regression tests passed, including the recorded-worker profile Move. Clippy for both touched crates with --all-targets and -D warnings passed; diff and formatting checks passed. Evidence: /mnt/optane/tmp/fix-1509-review-tests.log and /mnt/optane/tmp/fix-1509-review-clippy.log. A second review follows this correction commit.
