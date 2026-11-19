# Show quota pooled by provider, with refill timing per profile

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

A user of Mjolnir (`mj`) often holds many subscription logins ("profiles") for the same company: seven Claude Code logins, four Codex logins, two Antigravity logins, and so on. Today the TUI dashboard's quota pane lists every profile as one row of a table (weekly bar, reset time, five-hour bar, reset time). The user cannot see at a glance how much of a provider's total capacity is left, when it comes back, or which profile is about to reset with unspent capacity and should be used first.

After this change the quota pane groups profiles by provider (the company that sells and meters the quota: Anthropic, OpenAI, Google, xAI, Moonshot, DeepSeek). Each provider quota pool is one segmented bar: one segment per profile, segment width proportional to plan size, filled to that profile's weekly remaining percentage, segments ordered by weekly reset time and labelled with the time until that reset, followed by the pool's total remaining percentage. Selecting a pool and expanding it shows one lane per profile over the next 24 hours, which shows the five-hour window draining toward its reset and when the weekly allowance comes back. The user can see "claude5 has 60% left and resets in 19h, spend it first" and "claude2 is back at 17:00".

Provider is not the harness. A Codex profile can be configured to talk to DeepSeek, so grouping by harness would be wrong; grouping is by the provider the profile's configuration actually reaches.


## Progress

- [x] (2026-11-18 TUI-only port) Compared saved patches against c6564d58e and ported scrolling, recentering, timeline blocking/reset markers, segment fallbacks, period/average labels, readable-window selection and partial-total semantics, retaining TUI provider discovery.
- [x] (2026-11-18 TUI-only port) Quota tests pass (11). Full TUI suite run once (604 pass, 2 ignored, 3 stale goldens); the three goldens were reviewed, refreshed and individually verified without blessing. Doc tests, dev-profile clippy with warnings denied and formatting pass. Changes remain uncommitted.

- [x] (2026-11-18) Mockups agreed with the user (see Artifacts): bar design "D" for the pool, design "4" (next 24 hours lanes) for the expanded per-profile view; both ordered by weekly reset time.
- [x] Milestone 1: provider attribution helper in mj-core.
- [x] Milestone 2: pool model (pure computation) in mj-tui.
- [x] Milestone 3: render collapsed pool bars in the quota pane, with golden coverage.
- [x] Milestone 4: selection, expand/collapse, and the 24-hour lanes, with golden coverage.
- [x] Validation and isolated manual check; leave changes uncommitted for parent review.


## Surprises & Discoveries

- Observation (2026-11-18 TUI-only port): the only full-suite differences outside the quota golden were quota-pane `avg` labels and corresponding bar spacing in `dashboard-resource-panels`, `dashboard-session-render` and `profile-picker-table`. Reviewed those complete diffs before updating them; all three pass on subsequent comparison without blessing. Evidence: `.mj/agents/a9afd282f90f4e204b7a1c4b018430b0/tui-full.log` and `golden-*-verify.log`.

- Observation: a fresh isolated TUI needs terminal capability replies in a PTY driver, and startup profile discovery can import conventional host homes. The successful smoke driver used an explicit test home, empty enabled profile homes, disabled placeholders for other harnesses, and a completed setup marker. Every temporary daemon was stopped. Evidence: the report directory's `manual-quota-pools.log` and `manual-first-stripped.txt`.

- Observation: the current live snapshot differs slightly from the prototype capture (claude5 94% five-hour, claude7 41% weekly); the weighted Anthropic pool still rounds to 11%. The fixture uses these real readings without adjusting them to the earlier mockup.
- Observation: Codex custom-provider files must explicitly declare `wire_api = "responses"`. The first integration fixture omitted it and was corrected after the parser refused it.

- Observation: package names are `brokk-mj-core`, `brokk-mj-tui`, and `brokk-mjolnir`; the plan's abbreviated package names fail Cargo selection. Builds use the installed mbx shim/cache.
- Observation: provider attribution reads harness files, so calling the helper from pooling would block rendering. The TUI resolves provider identity on a supervised background thread and pools only in-memory snapshots.

- Observation: Anthropic reports no five-hour window at all until a session starts one; an absent five-hour window on a profile whose week has room means "full, not started", not "unknown".
  Evidence: `mj-accounts/src/claude_usage.rs` `parse_api_usage` filters a null `five_hour`; live `mj quota --json` shows `claude7` with only `Week` and `Week (Fable)` windows at 42% weekly.
- Observation: Antigravity (Google) profiles report two independent quota pools, one for Gemini models (`model_scope = "gemini"`) and one for the Claude and GPT models Google hosts (`model_scope = "claude,gpt"`), each with its own weekly and five-hour window. `mj-client/src/quota.rs` already models this as `AntigravityQuotaPool`.
- Observation: Claude reports a model-scoped weekly window `Week (Fable)` (`model_scope = "fable"`). An unscoped window applies to every model (`QuotaWindow::applies_to_model`), so when the all-models week is 0% the Fable allowance is unusable.


## Decision Log

- Decision (2026-11-18 TUI-only port): daemon-owned provider attribution was abandoned at the user's direction. Attribution stays derived in mj-core from the quota-source decision through `HarnessProfile::quota_provider`, with the existing supervised TUI provider discovery retained. No provider/account fields, collector identities, caches or wire-format changes are part of this port. Configuration and login-switch races during display grouping are accepted and tracked as follow-ups; they do not justify daemon attribution machinery.
- Decision (2026-11-18 TUI-only port): preserve line scrolling across stationary quota refreshes; recenter only on explicit selection movement or changed selected identity/rendered line range. Pool the longest readable window; name non-weekly periods and equal-weight averages. Failed or empty reports count as missing in every eligible pool, unreadable windows only in their own period, and absent periods do not make an otherwise usable report missing.

- Decision (2026-11-18 review fixes): lane notes always name the pooled allowance and independently append short-window readings and provider extras. Wrap notes below lanes when they cannot fit, retaining row identity on every continuation for selection, scrolling and clicks. Bound the name column so a long profile ID cannot consume other readings.
- Decision (2026-11-18 review fixes): count subscription profiles without usable readings within the provider/model scope as missing; annotate totals with `*` and report the count. Usage-priced reports are excluded. Use positive absolute limits when every member has one, then complete monthly prices, then equal weights. Provider/scope/period define the metered unit (the report format has no separate unit field).

- Decision: resolve file-based provider identity asynchronously in the TUI; unresolved identities say resolving and completed failures are reported. Pooling remains pure.
  Rationale: preserve the repository's event/render loop I/O rule without changing persisted or wire formats.
  Date/Author: 2026-11-18 / implementation agent.
- Decision: Enter and clicks toggle pools; `e` retains rename for single-profile pools and opens profile settings for multi-profile pools.
  Rationale: a pooled selection must never arbitrarily rename one of several profiles. Expanded lanes retain per-profile right-click actions.
  Date/Author: 2026-11-18 / implementation agent.

- Decision: Provider means the company that meters the quota, derived from the profile's harness kind plus its configured endpoint, not the harness alone.
  Rationale: user requirement; Codex or OpenCode can be pointed at DeepSeek and other providers. Antigravity's Claude/GPT pool stays under Google because Google meters it.
  Date/Author: 2026-11-18 / user + parent agent.
- Decision: Segment weight is `ProfileQuota::monthly_price_usd`; if any member of a pool lacks a price, every member weighs 1.
  Rationale: the field is already documented as the subscription capacity proxy; mixing known and unknown weights would make the pool incomparable (same rule t3code uses).
  Date/Author: 2026-11-18 / parent agent.
- Decision: One pool bar per (provider, weekly window model scope). Claude's `Week (Fable)` does not get its own bar; Antigravity's two scopes each get a bar.
  Rationale: Fable is capped by the all-models week, so a separate bar would mislead; Antigravity's scopes are independent allowances.
  Date/Author: 2026-11-18 / parent agent, shown to user in mockups.
- Decision: Both the pool bar segments and the expanded lanes are ordered by weekly reset time, soonest first.
  Rationale: user instruction; reading left to right (or top to bottom) is the refill order, and a soon-resetting segment with fill left is visibly capacity to spend first.
  Date/Author: 2026-11-18 / user.
- Decision: The expanded view replaces the per-profile table for that pool; the old table is removed rather than kept as a parallel path.
  Rationale: the user chose design 4 over the incumbent table; repository guidance prefers one mechanism.
  Date/Author: 2026-11-18 / parent agent.
- Decision: Usage-priced (pay-per-use) profiles and profiles whose quota is unavailable appear as one text line under their provider ("claude-aws: pay-per-use", "muse: unavailable"), not as segments.
  Rationale: they have no percentage to pool.
  Date/Author: 2026-11-18 / parent agent.


## Outcomes & Retrospective

2026-11-18 shipping state and follow-ups. The change ships TUI-only (plus the mj-core provider helper); review findings outside the display were declined rather than fixed, after eight rounds spent hardening a daemon-side attribution design that was then abandoned. Lesson: a display feature's review findings about rare configuration races should be declined with a reason early, not answered with new control-plane machinery. Declined findings, kept here as follow-ups: profiles sharing one login count twice (no account identity); provider grouping can be briefly wrong right after a config or login switch; Kimi pointed at a non-default endpoint groups under Moonshot; OpenCode `{env:NAME}` base URLs and unreadable OpenCode config fall back silently; Kimi `Month code` allowances are not pooled or used for lane blocking; a lane can look available when one of a Kimi profile's long windows is unreadable or reset without a new reading; the Profiles pane sizes itself at 74 columns rather than its render width; five-hour-only lanes and indexed-accent lanes lack the active-window shading; count-weighted pools average already-rounded percentages; collapsed fallback readings under a narrow bar are not keyboard-selectable (the expanded lane carries the same data); at narrow widths a long note shrinks the pool's 24-hour lanes below 24 cells; long profile ids that differ only at the end can truncate to identical lane labels; a per-profile Refresh does not re-read that profile's provider config; in a one-line Profiles pane the provider heading can scroll away from the selected bar; an Antigravity reading that covers only one of its two scopes hides the other scope's unavailable row; a Muse weekly lane ignores an exhausted rolling window that is not five hours; with both Antigravity scopes expanded, a failed then successful probe can move the selection to the other scope's lane of the same profile.

2026-11-18 TUI-only port completed on c6564d58e without commits or git-history changes. Wheel scrolling reaches wrapped notes in a one-line viewport; identity and rendered-position tracking preserves manual scrolling on stationary refreshes. Timeline cells use half-open ranges, preserve colliding resets with a conditional W+5 legend, and remain blocked until both exhausted windows recover. Monochrome fill boundaries survive labels; narrow segments expose readings and banked resets below the bar. Non-weekly period names, equal-weight `avg` totals, longest-readable-window fallback and period-specific partial totals are covered by the extended whole-surface golden, including failed/empty probes across periods and scopes. Daemon attribution, stale-provider logic and account deduplication were deliberately excluded. Existing provider discovery and provider order/accents remain intact. Configuration/login-switch display races remain accepted follow-ups.

Validation: 11 quota tests pass, including the golden subsequently compared during the full run. Full TUI suite run once: 604 passed, two ignored, three expected golden mismatches; reviewed and updated only those quota-pane diffs, then each failing test passed without blessing (607 library tests validated in total). Documentation tests pass (zero cases). `mbx clippy -p brokk-mj-tui --all-targets -- -D warnings`, `cargo fmt --all -- --check` and `git diff --check` pass. Evidence logs and progress notes are in `.mj/agents/a9afd282f90f4e204b7a1c4b018430b0/`.

All four milestones and validation are complete. The nine quota tests, provider-file integration, CLI/API parity, six existing dashboard goldens, the new multi-mode quota golden, dev-profile clippy, format checks, and isolated interactive smoke check pass. The Anthropic fixture renders 11% with the agreed reset ordering and relative segment widths. There are no outstanding decisions; changes are left for parent review without commit or push.

Implemented the provider helper, pure pool model, asynchronous provider discovery, segmented bars, 24-hour lanes, keyed multi-pool expansion, pointer selection, and ASCII/monochrome rendering. The live fixture and text/background golden are in place; the targeted quota tests and existing dashboard goldens pass. Dev-profile clippy passes after applying scoped allowances for provider colours painted against the active theme. CLI parity and the final fixed-clock golden pass. The isolated PTY smoke check also passes and its daemon has been stopped.


## Context and Orientation

The repository is a Rust workspace. The daemon (`mj daemon-run`) probes each enabled profile's quota and publishes a `QuotaSnapshot` (`mj-client/src/quota.rs`, around line 101). Its `reports` map holds a `ProfileQuota` per profile (around line 114) with `monthly_price_usd`, `banked_resets`, `paid_credits`, `harness`, `error`, and `windows: Vec<QuotaWindow>`. A `QuotaWindow` (around line 56) has `label` ("Week", "5H", "Week (Fable)"), `remaining_percent`, `duration_seconds`, `model_scope`, `resets_at_epoch_seconds`. Helpers already exist there: `weekly_window`, `five_hour_window`, `fable_weekly_window`, `billing_class` (subscription versus usage), `AntigravityQuotaPool`, `for_antigravity_pool`. Reuse them; do not reparse labels.

The TUI is in `mj-tui`. The dashboard state (`DashboardState`, `mj-tui/src/lib.rs`, around line 1317) owns the full `Config` and the quota map (around line 1002). The quota pane is drawn by `render_quotas` in `mj-tui/src/render/quotas.rs`, called from `mj-tui/src/combined.rs` around lines 935-955; when minimized it draws `minimized_quota_line` instead. Its width enters layout through `quota_table_width` and `support_panes_fit` (`combined.rs` around 324) and its size mode through `SupportPane::Quota` (around 625-643). Keyboard focus reaches it as `Focus::Quota` (`lib.rs` around 848-890; `dashboard_input.rs` around 397); `dashboard_sessions.rs` around 1476 counts selectable rows via `quota_profile_rows()`. Read those to learn how the pane currently handles focus and row selection, and extend that mechanism rather than adding a second one.

Profile configuration lives in `mj-core/src/config.rs` (`profiles: BTreeMap<String, HarnessProfile>`, around 1367) and `mj-core/src/config/harness.rs` (`HarnessProfile` with `kind` and `environment`, around 1326). Harness-specific provider configuration parsing already exists there: Codex and OpenCode provider helpers around lines 1819-1987 and provider structs with base URLs around 2062-2235; `mj-core/src/credentials.rs` line 63 checks `CLAUDE_CODE_USE_BEDROCK`. There is no unified notion of provider today.

The theme comes from `mj_chat::theme` (`theme::palette()`, `theme::glyphs()`), with an ASCII mode (`theme::ascii()`) that must keep working; the existing bar in `quotas.rs` around line 263 shows how ASCII falls back. Colours can be `ratatui::style::Color::Rgb`.

Golden tests: the combined dashboard golden `golden_combined_dashboard_render` (`combined.rs` around 2399) writes `mj-tui/tests/golden/combined-dashboard-render.txt`; blessing is `MJ_UPDATE_GOLDEN=1` (`mj-core/src/golden.rs` around 33-45). `mj-tui/src/docs_screenshots.rs` around 175-219 seeds quota fixtures. The `mj quota` CLI (`mj-cli/src/api_commands.rs` around 2936) is out of scope and must keep its output.


## Plan of Work

Milestone 1, provider attribution. In `mj-core/src/config/harness.rs`, next to the existing provider parsing, add a `QuotaProvider` enum (Anthropic, OpenAI, Google, XAI, Moonshot, DeepSeek, and `Other(String)` carrying a display name) with `display_name()`, and `HarnessProfile::quota_provider(&self) -> QuotaProvider`. Derivation: Claude kind is Anthropic (Bedrock and Vertex still serve Anthropic models, and such profiles are usage-priced so they only appear as a text line); Codex is OpenAI unless its configured model provider points elsewhere, in which case map the provider's base URL host or provider id (deepseek, x.ai, moonshot/kimi, googleapis, anthropic, openai) to the provider, else `Other(provider id)`; OpenCode maps its configured provider the same way; Antigravity is Google; Grok is xAI; Kimi is Moonshot; DSH is DeepSeek; anything else is `Other(harness display name)`. Put the host/id-to-provider mapping in one function and reuse it for Codex and OpenCode. Add an integration-style test only where a real config file shape (a Codex profile configured for DeepSeek in the format the existing parser reads) must agree with the mapping.

Milestone 2, pool model. In a new module `mj-tui/src/render/quota_pools.rs` (pure functions, no rendering), build from the config profiles, the quota snapshot, and `now` a list of providers in a stable order (Anthropic, OpenAI, Google, xAI, Moonshot, DeepSeek, then Other by name). Each provider has pools and text-only members. A pool is keyed by weekly-window model scope (unscoped for most; Antigravity yields two pools labelled "Gemini" and "Claude & GPT"). Each pool member has profile id, weight, weekly remaining, weekly reset epoch, banked resets, the five-hour window of the same scope (if any), and the Fable window for Claude. Members are sorted by weekly reset time; members without a reset time sort last. Pool remaining is the weighted mean of weekly remaining, rounded. Profiles that are usage-priced, errored, or report no weekly window (but do report something) become text members with a short label. Profiles reporting only a five-hour window and no weekly window (none today) become a pool on the five-hour window.

Milestone 3, collapsed rendering. Replace the table body of `render_quotas` with one block per provider: a provider heading in its accent colour, then per pool one line: pool label (if the provider has more than one pool), the segmented bar, and the pool percentage. Segments are separated by one blank cell, widths from largest-remainder rounding of weights over the available width (every member at least 1 cell). Inside a segment, the filled part (remaining) uses the provider accent as background, the rest a dim tint of it; when the segment is at least 6 cells wide, right-align `↻<time until weekly reset>` in it (compact form such as `4h43m`, `20h`, `1d14h`), plus `⟲N` for banked resets if it fits. Text members follow as one dim line per provider. Accents: Anthropic (217,119,87), OpenAI (16,163,127), Google (66,133,244), xAI (200,200,210), Moonshot (150,110,230), DeepSeek (77,107,254), Other uses the palette's muted colour. In ASCII mode, use `#` for fill, `.` for the rest, `|` between segments, and no colour dependence for meaning. Update `quota_table_width` (rename to fit the new content) so layout reserves a sensible width, and `minimized_quota_line` to show one `Provider NN%` reading per pool. Keep the refresh status header.

Milestone 4, selection and expanded lanes. In the focused quota pane, the selectable rows are pools (replace `quota_profile_rows()` with pool rows). Enter (or the key the pane already uses to activate a row; follow existing bindings and add the binding to the existing help/footer text) toggles expansion of the selected pool; more than one may be expanded. An expanded pool draws below its bar: a time axis row with clock labels every few cells, then one lane per member in the same weekly-reset order. The lane spans the next 24 hours, divided evenly across the available width (minimum 24 cells). Per cell: if the weekly allowance is spent and has not reset yet, `·` in dim grey, and `W` in the cell holding the weekly reset; else if a five-hour window is active and has not reset, background of the accent at strength 0.25 + 0.75 × five-hour remaining, with `5` in the cell holding the five-hour reset; else a faint accent background (fresh capacity available), with `W` in the cell holding the weekly reset if within 24 hours. After each lane, a dim note: "week back <Day HH:MM> (<duration>)" when spent; "5H idle: a fresh window starts on first use" when the week has room and no five-hour window is reported; else "5H NN%, resets in <duration>". Append "Fable NN%" for Claude members whose week has room. ASCII mode uses `.` for spent, `=` for active five-hour, `-` for fresh.

The expanded state belongs to the dashboard UI state next to the existing quota focus state; it is not persisted.


## Concrete Steps

Work from the repository root. Build and test on the dev profile, outside the sandbox:

    mbx check -p brokk-mj-core -p brokk-mj-tui
    mbx test -p brokk-mj-core quota_provider
    mbx test -p brokk-mj-tui quota
    mbx test -p brokk-mj-tui the_profiles_pane_shows_the_daemons_report_and_probing_set_and_nothing_older
    mbx test -p brokk-mj-tui golden_combined_dashboard_render
    MJ_UPDATE_GOLDEN=1 mbx test -p brokk-mj-tui golden_quota_provider_pools
    mbx clippy -p brokk-mj-core -p brokk-mj-tui --all-targets -- -D warnings

Manual check in an isolated instance (never the default instance): build, then run `mj --instance quota-pools` with the user's profiles copied or with a fixture snapshot, focus the quota pane, expand a pool, then `mj --instance quota-pools daemon stop`.


## Validation and Acceptance

Add one golden test in `mj-tui` that renders the quota pane from a fixture snapshot built from the real readings recorded in Artifacts (seven Claude profiles with prices 200/100, four Codex, two Antigravity with both scopes, Grok, Kimi, a usage-priced Claude Bedrock profile, a DSH profile, an errored Muse profile) at a fixed `now`, once collapsed and once with the Anthropic and Google Gemini pools expanded, in both Unicode and ASCII modes, and compares the whole rendered buffer (text plus a colour-annotated form if the existing golden helpers support it) with checked-in files. Extend the combined dashboard golden for the new pane. Acceptance: the Anthropic bar shows seven segments ordered claude2, claude5, claude6, claude3, claude, claude4, claude7 with claude5 half the width of the others and pool 11-12%; expanding it shows claude2's lane as dots then `W` near 17:00; claude5 shows an active five-hour band ending in `5`; claude7 reads "5H idle". `mj quota` output is unchanged (`api_quota_matches_cli_golden` passes).


## Idempotence and Recovery

All changes are code and golden files; rerunning tests is safe. Golden files are regenerated only with `MJ_UPDATE_GOLDEN=1`; review their diff before committing.


## Artifacts and Notes

Agreed mockup (stripped of colour) from live readings at Wed Nov 18 12:16; segments in the real UI carry the colour fill described above:

    ANTHROPIC  ↻4h43m        ↻20h43m     ↻1d0h    ↻1d14h    ↻4d13h     ↻6d0h   11%
                      12:16   16:16   20:16   00:16   04:16   08:16
    claude2           ·········W                                        week back Wed 16:59 (4h 43m)
    claude5                    5                           W            5H 96%, resets in 4h 53m
    claude6           ·········································W        week back Thu 08:59 (20h 43m)
    claude3           ················································  week back Thu 12:59 (1d 0h)
    claude            ················································  week back Fri 02:59 (1d 14h)
    claude4           ················································  week back Mon 01:59 (4d 13h)
    claude7                                                             5H idle: a fresh window starts on first use

    GOOGLE Gemini                       ↻6d6h                           ↻6d6h   90%
    GOOGLE Claude & GPT                 ↻6d5h                           ↻6d6h   48%
    MOONSHOT                                                           ↻2d7h    0%

The prototype renderers are at `/tmp/quota-mock/color.py`, `variants.py` (design D) and `fivehour.py` (design 4) on the development host; they read `mj quota --json` on stdin and may help as a reference but are not part of the deliverable.


## Interfaces and Dependencies

In `mj-core/src/config/harness.rs`:

    pub enum QuotaProvider { Anthropic, OpenAI, Google, XAI, Moonshot, DeepSeek, Other(String) }
    impl QuotaProvider { pub fn display_name(&self) -> &str; }
    impl HarnessProfile { pub fn quota_provider(&self) -> QuotaProvider; }

In `mj-tui/src/render/quota_pools.rs`: a pure `fn provider_pools(config: &Config, quotas: &BTreeMap<String, ProfileQuota>, providers: &ProviderDiscovery, now: u64) -> Vec<ProviderQuota>` with small structs for provider, pool, member, and text member, used by both the collapsed bar and the lanes. No new crates.

2026-11-18 implementation update: adjusted package commands, background provider discovery, and pool selection actions to match the actual source and avoid UI I/O. Parent explicitly requested no commit or push.

2026-11-18 validation update: provider configuration integration passes after fixing the Codex file shape. The new golden captures both text and background-colour runs, Unicode and ASCII, collapsed and expanded views, and narrow clipping. The fixture clock uses the capture's fixed Chicago offset for portability.

2026-11-18 validation checkpoint: required clippy passes (`clippy-final.log` in the sub-agent report directory). Existing combined, session, layout, footer, resource-panel and away-dialog goldens have been updated and reviewed. Changed old assertions now refer to provider pool readings; provider refresh preserves cached identity while discovery runs.

2026-11-18 completion evidence: `api_quota_matches_cli_golden` passes (CLI package `brokk-mjolnir`); clippy passes with warnings denied. The isolated `quota-pools-d3fe` PTY check seeded the recorded Anthropic quota into its own database, used empty profile homes, exercised Tab/Enter expansion and clean detach, then stopped its daemon. Its driver and logs are in `.mj/agents/d3fe29a4201859ae1f51526371c93bbb/`. The initial PTY driver needed terminal capability replies; startup discovery also required an isolated test home and disabled placeholders for other harnesses to avoid importing conventional host profiles. No default-instance daemon commands were used. The exact snapshot-replacement regression is included because its existing assertions now refer to provider names and preserve the last pool reading during refresh. No commit or push was made, as requested.

2026-11-18 TUI-only port update: supersedes the abandoned daemon attribution rounds. Port only mj-tui and this plan onto c6564d58e; preserve provider ordering/accents including Z.ai and Meta. Extend the existing quota golden with line scrolling, overlapping and coincident reset windows, unreadable periods and tiny-segment fallbacks. Codex fixtures have no capacity limit or monthly price, matching the collector. Validate outside the sandbox with `mbx test -p brokk-mj-tui quota`, one full `mbx test -p brokk-mj-tui`, `mbx clippy -p brokk-mj-tui --all-targets -- -D warnings`, and `cargo fmt --all -- --check`; bless other goldens only after confirming changes are confined to the quota pane.

2026-11-18 TUI-only validation completion: all requested behaviors and checks are complete, with failures limited to expected golden updates now verified. Only mj-tui and this ExecPlan were changed; HEAD remains c6564d58e on master. No daemon or live-instance commands were invoked.
