# Resolve provider groups before the delegation review

This living plan follows `.agents/PLANS.md`.

## Purpose / Big Picture

Choosing a provider group in New session must resolve to a named profile when leaving the first wizard page. Subsequent pages then restore that profile's saved delegation mode and Orchestrators checkbox, and creation uses that same named profile.

## Progress

- [x] (2026-11-24 14:03Z) Trace the defect: provider selection returns an empty profile ID until creation, so the dialog reads the legacy preference while creation saves under the real profile.
- [x] Add a daemon-owned provider selection request using the existing scheduler.
- [x] Run that request asynchronously at the first page and apply only its current reply.
- [x] Exercise saved policies, cancellation, failures, and stale replies; run focused tests and Clippy.
- [ ] Commit, review, merge upstream if necessary, and push to origin/master.

## Surprises & Discoveries

The daemon and TUI already transport the remembered delegation policies correctly for named profiles. The provider-group path deliberately postpones naming the profile until Create. A separate Oracle-map omission is recorded in #1680; its patch is saved in /tmp/mj-1680-oracle-preferences.patch and excluded from this fix (#1681).

## Decision Log

- Decision: Resolve provider groups through the daemon's existing quota scheduler when Next leaves the profile page; keep named-profile and Auto (Primary) launches otherwise unchanged.
  Rationale: The daemon owns selection. The TUI's prediction is explicitly best effort and must not become a second selector. Once chosen, use ordinary named-profile creation. Do not retain a quota reservation while a person browses later pages; the selection request releases its temporary reservation when it returns.
  Date/Author: 2026-11-24 14:03Z, Codex.
- Decision: Reuse the existing session preflight generation for asynchronous reply identity and keep a pending request identity on the new-session draft.
  Rationale: Cancel, selection changes, and a newer request must prevent old replies from advancing a different wizard. Keep navigation and cancellation responsive.
  Date/Author: 2026-11-24 14:03Z, Codex.

## Outcomes & Retrospective

Implemented early provider resolution through the daemon scheduler. The profile transition regressions (3), delegation regressions including the Create golden (3), and HTTP client contract test (1) pass. Clippy passed on all five affected crates. The existing provider scheduler regression also passes (1). Review and push remain.

## Context and Orientation

`mj-tui/src/wizards/dashboard/new_session.rs` advances the Create wizard. `NewWizard::selected_profile_id` in `mj-tui/src/wizards.rs` currently returns an empty string for a provider group. `SubagentDraft::refresh_create` loads policies from `State::remembered_subagent_policy_for`. `mj-server/src/server/api/subagents.rs::resolve_provider_session_selection` already implements quota-aware selection and returns a temporary reservation. `mj-daemon/src/daemon/dashboard_api.rs` implements the dashboard HTTP backend. The CLI runs background requests and returns their completions through `DashboardIoUpdate` in `mj-cli/src/dashboard/io.rs`.

## Plan of Work

Add a terminal dashboard HTTP operation accepting a provider and returning its selected profile ID. Its daemon backend calls the existing provider resolver against current configuration and releases the temporary reservation on return. At the first wizard's Next, emit a background action and record its request generation. Disable duplicate Next while allowing cancel and profile changes. On a matching reply, select the returned named profile, restore delegation settings for it, and advance normally; report a failed selection while leaving the first page available to retry. Queue any follow-up UI action through the CLI's existing action queue. Keep a profile chosen at this boundary through later target/project/review pages and Create.

## Milestones

First implement the HTTP request and adapter without changing session persistence. Next wire the first-page transition and verify that both saved mode and Orchestrators come from the resolved profile. Finally add ordering regressions and complete checks, review, and publication.

## Concrete Steps

Run all commands from the repository root. Use `mbx test -p brokk-mj-tui new_session_profile_step --lib` for the profile transition tests and focused HTTP or daemon tests for the new operation. Use `mbx clippy -p brokk-mj-tui -p brokk-mj-client -p brokk-mj-server -p brokk-mj-daemon -p brokk-mjolnir --all-targets -- -D warnings` and `cargo fmt --all -- --check`. Every test command runs outside the sandbox. No test may start the default daemon; use isolated instance fixtures if a daemon is needed.

## Validation and Acceptance

A provider-group Next waits asynchronously for a concrete profile. Returning a profile whose saved policy is Mjolnir with Orchestrators enabled displays and submits both choices. A saved None mode stays None. Creation carries that concrete profile and no provider selection. A late completion after cancel, selecting another row, or opening a new wizard has no effect. Failure leaves the first page usable. Existing profile navigation and golden output must continue to pass except where the intended concrete-profile behavior changes.

## Idempotence and Recovery

Selection creates no session and retains no reservation. Retry is safe. Closing the wizard invalidates pending completions. Existing noninteractive provider creation stays supported. Only task files are staged; the unrelated Oracle patch stays outside the checkout.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1681. Validation receipts will be added after the tests run.

## Interfaces and Dependencies

Extend `DashboardBackend` and `HttpDaemonClient` with `select_provider_profile(provider: QuotaProvider) -> Result<String>` using `/api/v1/profiles/select`. Add a `SelectProviderProfile` dashboard action carrying provider and generation, and a matching I/O completion. Add `NewWizard::profile_selection_request: Option<u64>`. Apply completion through a public dashboard method returning the next `DashboardAction`.

Initial plan written after the user clarified provider-group selection as the trigger.

2026-11-24 14:09Z: Updated progress after the first-page transition, cancellation/retry tests, and client contract passed.
