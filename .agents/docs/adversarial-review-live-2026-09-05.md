# Adversarial review live test, 2026-09-05

## Setup and reproducible fixture

The test started from commit `76c094b0` in `/home/jonathan/Projects/hel2`. Both the host CLI and portable worker were built from the current source. The terminal ran at 160 columns by 48 rows under a separate tmux socket, `mj-adv-live`, with workspace `adv-live`. Configuration and durable state were isolated under `target/adversarial-live/`; no existing user sessions or profile configuration were changed.

The target was local Podman using `ghcr.io/brokkai/mjolnir/agent-dev:latest`, image ID `db6a2d589b9f`. Its adapters reported Codex ACP 1.8.0 and Claude Agent ACP 0.73.0; Bifrost reported 0.10.7. The primary profile was `codex3`, configured in the live session with `/model gpt-5.6-sol` and `/effort medium`. Reviewer profile `claude2` used Opus at medium effort. The Claude bridge's advertised Opus ID was `opus[1m]`; `opus` was rejected. Journal observations confirmed both quick-review roles selected `opus[1m]` and `medium` before their prompts.

The disposable local fixture repository initially held only a README. The task asked for `ranges.py` implementing inclusive `clamp(value, lower, upper)`, raising ValueError on reversed bounds while accepting equal bounds. It also asked for five unittest cases and `python3 -B -m unittest -v`, without dependencies or commits. A second turn added negative-number and floating-point cases.

For deterministic findings coverage, the operator changed `if lower > upper:` to `if lower >= upper:` after Sol's successful five-test implementation and before an on-demand review. This was an operator-seeded regression, not a defect authored by Sol. The suite then failed on equal bounds, as expected. The general reviewer and validator identified it; Forward findings started a Sol correction turn, restored the strict comparison, and returned all five tests to green.

## Observed defects

The first completed changed turn was skipped. It produced `Review coverage starts here; the next completed turn is reviewed` because the initial baseline was captured only after the primary had made its edits. The source fix captures the initial worktree before the primary starts and retains that Git tree independently of HEAD.

An invalid reviewer model failed before producing a usable transcript. The terminal displayed an empty `Turn review · failed` pane, hiding the actual `"opus" is not an available model value` diagnostic. Forward findings remained selected while disabled, so the advertised Enter action did nothing.

Correcting the model and retrying failed with a missing native Claude conversation. Profile staging deleted the default reviewer's live home, while its durable relay retained the deleted conversation's ID. Host generations also restarted per review. These are conversation-lifetime defects, not authentication failures.

At a findings verdict the role strip disappeared, and Tab could not switch from General to Validator. The driver returned an empty role list outside its Running phase, making the final validator transcript unreachable despite the Tab hint.

A clean verdict was recorded as `Review dismissed`, and dismissing a failed review was recorded as `Review cancelled`. Completion notices now retain typed verdict context, distinguishing clean results from manual dismissal and failure.

Extended review also selected the wrong governing task: the host labeled the first-ever user prompt (parity) as the current outer prompt while reviewing a later parser change. The intent analyst explicitly classified the parser request as superseded. The host now supplies the latest real user prompt and all chronological user requirements, excluding generated harness notes. Keeping earlier requirements is necessary when a recent steering prompt only says to finish or fix the work.

The forwarding wrapper claimed that a validator had verified every finding, including in an extended review where no separate validator ran. It now describes independent review findings without overstating their validation.

## UX recommendations

Offer reviewer setup through the TUI using the bridge's advertised profile, model, and effort choices. Configuration currently requires editing TOML and knowing exact model IDs; the `opus` versus `opus[1m]` mismatch caused a real failed launch. `/review status` should show the effective model and effort as well as the profile and tier.

Show reviewing, validating, findings, or failed in the session row. During the live review it said `[idle]`, even while the review pane held the composer. A user looking at another session cannot infer the actual review state from that label.

Separate a role's completion state from its judgment. The final extended review showed `Supervisor clean` beside a findings verdict. The supervisor had completed successfully and found a defect; `done` would communicate that completion without implying a clean change.

Lead with the verdict and compact progress. Keep internal review prompts, XML-like context packets, and raw patches collapsed or behind a transcript/raw view. They currently fill the reviewer pane before useful reviewer output arrives, and Markdown rendering makes raw diff text harder to inspect.

Clarify result semantics and severity. Both quick-review roles labeled the small fixture boundary defect P0 and reported its failing test as a second P0. The prompts demonstrate findings with `[P0]` but do not define priority levels. Add explicit severity definitions and ask the validator to consolidate one root cause with its test evidence. The seeded post-turn edit also shows why a stale primary test claim should be described as stale evidence, not automatically treated as an independent code defect.

Make command submission state clearer. During initial setup the first model-change attempt did not apply; the unchanged primary-model heading exposed it. Later, with Prompt focused and an exact advertised value, one Enter applied the model. Distinguish accepting a completion, submitting a command, and receiving confirmation so this interaction is easier to diagnose.

## Integrated live validation

A fresh worker captured its baseline before primary edits. Its first changed turn automatically launched review. Deliberately configuring `opus` reproduced the model error, now fully visible in the verdict pane at 160×48 and scrollable at 90×30. Enter selected the enabled Dismiss action. Correcting the model to `opus[1m]` and retrying reached a fresh Claude conversation and a clean result.

For the final extended pass, Sol added `parse_range(text)` and five tests alongside the earlier five parity tests. The operator again seeded the equal-bound comparison defect after the successful primary turn. On the repaired host the intent analyst correctly identified the parser task and classified parity as earlier completed work. Escape cancelled a running supervisor; the prompt was available within two seconds. Retrying reached visible findings on the same unreviewed defect. Tab switched from Verdict to Intent and Supervisor transcripts and back. Forward findings started a Sol correction turn; both Sol's run and an independent operator run of `python3 -B -m unittest -v` passed all 10 tests. Automatic extended review then completed with `Review complete: no material findings` and restored the prompt.

The final journals reported primary `gpt-5.6-sol` / `reasoning_effort=medium`, intent `opus[1m]` / `effort=medium`, and supervisor `opus[1m]` / `effort=medium`. Extended review elected to run no specialist lanes on this small change, so this run does not claim live specialist fan-out coverage. Quick review did exercise separate General and Validator conversations. Automated role-lifecycle tests cover independent role homes and compatible conversation reuse.

Evidence is local under `target/adversarial-live/`: `fixed-error.capture`, `fixed-error-narrow.capture`, `extended-wrong-intent.capture`, `final-running.capture`, `final-cancelled.capture`, `final-extended.capture`, `final-supervisor-tab.capture`, and `final-clean.capture`. The seeded failures were operator actions to exercise review; the primary's earlier successful test reports were accurate when originally produced.

## Repository validation and cleanup

The final full Cargo suite, including the forwarding-copy correction, passed 2,260 tests with 0 failures and 12 ignored tests. Required Clippy checks passed with warnings denied. Host and portable musl worker builds succeeded; formatting and diff-whitespace checks passed. Logs remain under `target/adversarial-live/` and are not committed.

The isolated daemon was stopped, all three recorded disposable containers were stopped before removal, and the `mj-adv-live` tmux server was closed. Profile homes and unrelated sessions remain untouched. Generation-specific profile snapshots and archived relay journals remain for the life of a worker; a future retention policy may be useful for very long-lived sessions.
