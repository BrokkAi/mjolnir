# Configure global review and prevent avoidable failures


This ExecPlan follows `.agents/PLANS.md` and implements the remaining accepted designs in `.agents/docs/adversarial-review-ux-follow-up.md`. The user also authorized pushing the completed commits to the current branch's upstream.

## Purpose / Big Picture


Users should configure one global adversarial reviewer from F2, using actual model/effort choices and target readiness results, without per-session preference copies. Review priorities should reflect demonstrated impact and consolidate duplicate root causes without changing the output protocol. Forwarding findings must only report success after the primary accepts the correction, and failures must preserve findings for retry. Configuration and probing must not block the terminal loop or disrupt reviews already running.

## Progress


- [x] Read existing configuration, palette, review host, worker startup, and prior design notes; confirmed a clean starting tree.
- [x] Delegated global settings UI/save integration, acknowledged forwarding, and shared severity guidance to separate Luna agents. Parent owns readiness backend, cross-component design, live verification, integration, and final review.
- [x] Implement target-specific reviewer choice discovery and readiness checks shared with actual launch semantics; six remote-manager behavior tests pass.
- [x] Integrate settings, acknowledged forwarding, and prompt calibration; validate representative behaviors including interrupted handoff, stable retry identity, independent reviewer connections, and worker disconnect cleanup.
- [x] Run focused tests, full Cargo tests and Clippy; all pass. Live F2 verification/save passed on the actual disposable target.
- [x] Merge updated upstream on the current branch, preserving both review overlays and upstream session colors. Final integrated validation: 2,317 Cargo tests passed, 12 ignored; Clippy, formatting/diff checks, and 11 web unit checks passed.
- [x] Complete the live calibration/forwarding cycle: revised Opus verdict retained two root causes at P1/P2; Sol fixed both, the independent six-test rerun passed, and automatic Opus follow-up was clean. Repeated Enter during active review did not cancel it; repeated Enter at forwarding produced one corrective turn. Cancelling a visibly checking readiness probe preserved the open review.
- [x] Record evidence, commit coherent validated changes on the current branch, and stop/remove the disposable test container and isolated UI/daemon.
- [x] Push the completed work to the branch's upstream: `9e71fec9` was accepted on `origin/master`, including all implementation and live-test evidence.

## Surprises & Discoveries


The existing F2 `Open setup` command is a first-run stdio workflow; review configuration needs a normal global dialog. Review capability selection is advertised after a harness starts, and model selection may change effort choices. A global configuration therefore has target-specific readiness, especially when adapters differ. The current forward path starts a detached submit and closes before acknowledgement, so failed delivery can be reported as sent.

Integration review found that initial dialog code could allow Save during probing, reopen after Cancel, accept a stale response after close/reopen, and mark unverified selectors unavailable. Direct transition tests and a dashboard-wide probe generation repair those boundaries. Choice lists must remain usable while changing a model; only model-dependent efforts need invalidation. Disabling review remains available even with invalid settings.

The upstream moved during implementation. Fetch showed new commits on origin/master; integrate them by merge on the current branch before final validation and push, without rewriting history.

## Decision Log


Keep the existing `[review]` TOML shape and atomic-save machinery. Save against the latest configuration to preserve unrelated changes. Probe real managed workers through a private reviewer role so probing cannot replace an active reviewer's native conversation. Use supervised background tasks, distinct probe identities, and cleanup; no hidden provisioning when no target exists. Unknown readiness must be explicit rather than silently claiming success. Model and effort selectors use the same adapter interpretation as actual review. Reject incompatible values rather than changing the requested reviewer model.

Keep `[P0]`–`[P3]` markers and clean sentinels unchanged. Centralize semantic calibration and final-reviewer consolidation in shared prompts. Evaluate behavior with representative cases rather than prompt-paragraph snapshots or deterministic text deduplication.

Readiness probes stage a private role per attached actual placement and apply the selected model before discovering efforts. They issue no model prompt and exercise Bifrost analysis with identical current/current trees, leaving review coverage unchanged. No target means unverified. A successful check is an observation of currently attached targets, not a persistent guarantee about absent targets or future adapter changes; every dialog open and selector edit performs a fresh check.

The cancellation audit exposed an existing transport bottleneck: reviewer RPCs occupied the primary actor connection, and remote bridges serialized them with primary commands. Move reviewer requests onto supervised connections cached per role, preserve order within each role, and keep primary controls independent. Propagate cancelled callers through the daemon bridge as disconnects; the worker aborts and pauses only an operation still in flight when its client disconnects. A completed Start remains alive normally. This avoids both hundreds of seconds of quit delay and a process-per-poll regression. A real stdio relay test proves primary sync and another role remain responsive while one role is blocked, and aborting that caller closes its proxy.

Forwarding must be an acknowledged transition with a stable command identity. Keep the review hold until the internal correction is accepted; admit only that scoped handoff through the hold. Duplicate actions must not create duplicate corrections. Rejection preserves findings and baseline; only accepted delivery records sent and advances coverage. Restart and ambiguous transport outcomes need explicit handling rather than optimistic success.

## Outcomes & Retrospective


The live F2 probe on the disposable Podman target advertised Opus (1M context) and Medium, verified the adapter and Bifrost, and saved the global review configuration successfully. Sol medium created the make-work helpers and six passing tests. Controlled post-turn edits exposed two independent defects and three failing assertions. After the first calibration result prompted a shared-guidance revision, the same Opus/medium review returned P1 for the authorization bypass and P2 for the boundary defect, with no third finding for the stale test report. It still appended an unnecessary explanatory note; the small evaluation does not establish deterministic prompt compliance or a low failure rate.

Forwarding through the real TUI produced one corrective Sol turn, a sent notice after acceptance, and an automatic clean Opus follow-up. The independent final `python3 -B -m unittest -v` rerun passed all six cases; no dependencies or fixture commits were introduced. The persisted review state ended with no active review or pending handoff. Global readiness could verify concurrently with an active review; cancelling a fresh probe while it showed `checking actual targets` preserved the findings, and no readiness/Claude harness remained afterward. The primary model was reset to Sol/medium after each automatic binary upgrade restored its profile default; model persistence across upgrades was outside this approved change.

Final integrated validation passed 2,317 Cargo tests (12 ignored), Clippy with warnings denied, 11 web unit checks, formatting, and diff checks. Severity guidance and implementation were committed in coherent checkpoints; upstream was merged on the existing branch, resolving only a session-rendering conflict by retaining both behaviors. The isolated tmux/daemon and disposable container were stopped, then the container was removed; evidence remains under the ignored lab directory and user profile homes were preserved. The authorized push succeeded at `9e71fec9`; this final documentation update records completion.

## Context and Orientation


`src/hel_config.rs` defines `ReviewConfig` and atomic configuration saves. `mj-tui/src/actions.rs` owns the F2 command registry; dialogs live in `mj-tui`, and `mj-cli/src/dashboard` runs their background operations. `mj-controller/src/hel_review_host.rs` runs review independently of any open terminal and publishes `RuntimeReviewView`. `src/hel_review/driver.rs` defines review transitions and requests; `mj-controller/src/hel_session_manager.rs` admits and submits actor commands. `mj-worker/src/hel_worker_runtime/reviewer.rs` runs role-specific harnesses and applies advertised selectors. `src/hel_review/lanes.rs` owns shared prompts. A readiness backend will live in the existing controller crate, using existing worker/reviewer facilities rather than adding a crate.

## Plan of Work


Milestone one adds a global settings dialog with fields for automatic review, tier, profile, model, and effort. Opening, probing, and saving have visible async state and cancellation; outdated probe replies cannot overwrite newer edits. Saving preserves unrelated current config and applies to subsequent reviews. Verify through palette/key/render tests and a live isolated TUI.

Milestone two supplies actual-target capability discovery and readiness. Select representative attached workers for execution environments, stage the requested reviewer in a private role, discover selectors, apply the candidate model before reading efforts, and check required review tooling using shared paths. Clean up the probe's harness after use. No live target yields explicit unverified state. Behavior tests cover unavailable selectors, invalid choices, model-dependent efforts, cleanup, and environment distinction. Integrate readiness so known-invalid settings fail before repeated role startup.

Milestone three repairs forwarding at driver/host/actor boundaries. Use delayed and rejected submit fakes to prove no premature closure, baseline advancement, or false sent notice. Demonstrate normal external prompts remain held, an internal correction is admitted, duplicate resolution is safe, and retries preserve command identity. Update all affected views for any new phase.

Milestone four adds shared priority and root-cause guidance. Use representative evaluation fixtures with ordinary and broad-impact defects, unsupported claims, duplicates, distinct causes, stale reports, and intentional behavior. Inspect actual reviewer answers for justified findings and clean results without requiring exact prose.

## Concrete Steps


Work in `/home/jonathan/Projects/hel2`. Run focused package tests outside the sandbox, then `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and `git diff --check`. Build host and portable worker for any live container check. Use isolated config/data under `target/adversarial-live/` and a separate tmux socket; never modify user profile homes. Commit explicit task-owned paths only. After validation, inspect upstream and push the current branch with elevated permissions, as authorized.

## Validation and Acceptance


F2 exposes global Review settings without requiring a selected session. Choices come from the selected review profile's actual adapter; effort refreshes after model selection. Save changes only global review fields. Dialog and terminal remain responsive during operations. Probing cannot disturb a real review. Forwarding remains pending until acceptance, preserves findings on rejection, and produces one corrective prompt across retries. Shared calibration reduces unjustified urgency/duplicate findings without changing parser semantics. Full checks pass before push.

## Idempotence and Recovery


Use stable command identities for submission reconciliation and unique role/generation identities for probes. Cancellation stops processes before removing probe working files. Preserve unrelated user sessions, config, and working-tree changes. Do not switch branches, create a PR, or force-push. Configuration saves must respect newer-schema read-only protection.

## Artifacts and Notes


Keep live captures and validation logs under ignored `target/adversarial-live/`; store sanitized evaluation findings and design decisions under `.agents/docs/`. The established live profiles are codex3/Sol medium and claude2/Opus medium (`opus[1m]`).

## Interfaces and Dependencies


Reuse `ReviewConfig`, `SessionConfigOption` and shared selector helpers, `ReviewEnvironment`, `ReviewerAction`, `ManagedSessionHandle`, existing background-I/O supervision, and atomic configuration writes. Parent coordinates readiness interfaces with UI and review-host agents. No new workspace crate or third-party dependency is planned.

Plan created 2026-09-05 for explicit approval of global review settings, calibration, readiness, and acknowledged forwarding; includes the user's subsequent push authorization.

Updated 2026-09-05 after integration and full validation: repaired non-consuming disconnect observation so cancelling a watcher cannot discard a partial next frame; a 128 KiB socket fixture proves preservation. Worker cancellation tests check actual process death rather than a graceful-exit marker that a correct force-stop need not write.

Updated after upstream integration and the first calibration observation: the live validator retained an over-prioritized boundary bug and a redundant stale-report finding. Both journals contained the intended rubric, so the shared semantic rule was tightened and the same fixture is being repeated. Details and limits are in `.agents/docs/review-calibration-evaluation.md`. No parser or output-protocol change was introduced.
