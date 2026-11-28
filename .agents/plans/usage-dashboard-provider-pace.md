# Usage Dashboard: Provider-Level Pace, Expandable Role Breakdown, and 24h Role-Change Timeline

## Goal

Replace the per-profile `Quota Pace & Forecast` text list and bottom role summary in the TUI Usage modal with a **Usage Dashboard** centered on **provider-level weekly pace**:

1. Rename `Usage by session` to **`Usage Dashboard`** across the modal title, command palette, profile menu, and error notices.
2. Show **provider-level weekly pace** as a percentage (`100%` = sustainable weekly burn rate, `0%–100%–200%+` centered meridian bullet gauge), omitting 5h window noise.
3. Add a **clickable `▸`/`▾` disclosure arrow per provider** to expand an indented breakdown of **what roles are consuming that provider's usage**, rendered on the same `0%–100%–200%+` meridian scale so expanded role percentages sum directly to the provider's pace percentage (alongside each role's `% share`, estimated USD, and top models). Remove the redundant role totals from the bottom of the dialog.
4. Keep the **24h hourly bar chart** and interactive hour/range selection, and **persist a log of role-assignment configuration changes** in SQLite so they render as numbered timeline markers (`▲①`, `▲②`, …) on the 24h chart with one-click post-change hour selection for before/after comparison.

## Design Decisions

### 1. Additive, Backward-Compatible Report Contract (`mj-core`)
Extend `SessionUsageReport` in `mj-core/src/usage/report.rs` with `#[serde(default)]` fields so older clients/daemons interoperate cleanly:
- `ProfileQuotaUsage::covered_seconds: Option<u64>` (`#[serde(default, skip_serializing_if = "Option::is_none")]`) — exact number of seconds covered by quota probes in the selected hour intervals, so partial probe windows compute accurate burn rates instead of dividing by the full window length.
- `ProfileQuotaProjection::is_short_window: bool` (`#[serde(default, skip_serializing_if = "std::ops::Not::not")]`) — set from `WindowKind::from_window(window) == WindowKind::Short5h` so the TUI excludes 5h windows without fragile label parsing (falling back to checking `"5h"` prefix only for legacy daemon payloads).
- `ProfileRoleUsageEstimate { profile: Option<String>, role: UsageAgentRole, models: Vec<ModelUsageEstimate>, cost_usd_ticks: CostUsdTicks }` added as `SessionUsageReport::profile_role_usage: Vec<ProfileRoleUsageEstimate>`. Keyed by each turn's own recorded `profile` and `role`, populated inside the existing single turn scan in `mj-store/src/database/usage_report.rs`.
- `RoleAssignmentChange { id: i64, changed_at_ms: i64, summary: String }` added as `SessionUsageReport::role_changes: Vec<RoleAssignmentChange>`. Always covers the full 24h graph span (`graph_start..=now_ms`) regardless of selected hours, matching `SessionUsageReport::hours`.

### 2. Single-Owner Role-Assignment Change Log (`mj-core`, `mj-store`, `mj-daemon`)
- Define `RoleAssignmentSnapshot::from_config(config: &Config)` in `mj-core` capturing:
  - `primary: Vec<SubagentChoice>` (`config.subagents.primary`)
  - `subtle: Vec<SubagentChoice>` (`config.subagents.subtle`)
  - `straightforward: Vec<SubagentChoice>` (`config.subagents.straightforward`)
  - `oracle: Vec<SubagentChoice>` (`config.subagents.oracle`)
  - `reviewers: Vec<SubagentChoice>` (`config.review.reviewers`, plus legacy profile if present)
  - `brief_writer: Option<SubagentChoice>` (`config.review.brief_writer.as_ref().and_then(BriefWriterConfig::choice)`)
- Define deterministic `diff_role_assignments(old, new) -> Option<String>` producing concise summaries (e.g., `"Reviewers: gpt-5.4 → claude-opus-4-6"`, `"Subtle: +claude-opus-4-6, -gpt-5.4"`, joining multiple roles edited in one save with `"; "`).
- **SQLite migration 166 (compatible, O(1) DDL; `MINIMUM_COMPATIBLE_VERSION` remains `165`):**
  - Older readers/writers ignore the two new tables and preserve existing tables unmodified.
  - `role_assignment_state(id INTEGER PRIMARY KEY CHECK (id = 1), snapshot_json TEXT NOT NULL, updated_at_ms INTEGER NOT NULL)` — persists the last observed snapshot across daemon restarts so first boot seeds quietly with no spurious marker, while offline `config.toml` edits are captured on startup.
  - `role_assignment_changes(id INTEGER PRIMARY KEY AUTOINCREMENT, changed_at_ms INTEGER NOT NULL, summary TEXT NOT NULL)` + `idx_role_assignment_changes_time ON role_assignment_changes(changed_at_ms)`.
- `sync_role_assignment_snapshot(snapshot, now_ms)` runs inside one SQLite write transaction (`BEGIN IMMEDIATE` / transaction) so concurrent config reloads serialize cleanly at one owner: seed if absent, no-op if unchanged, otherwise update state and insert one `role_assignment_changes` row.
- Call `sync_role_assignment_snapshot` via `spawn_blocking` whenever the daemon loads or reloads config (daemon startup in `mj-daemon/src/daemon/process.rs`, periodic config watcher reload in `process.rs`, and `RuntimeState::reload_controller` in `mj-daemon/src/daemon/snapshot.rs`). Never perform SQLite I/O while holding an in-memory daemon mutex.

### 3. Provider-Level Weekly Pace & Role Allocation Math (`mj-tui`)
- Reuse existing provider grouping (`profile_provider`) and plan-price weighting (`monthly_price_usd`, falling back to equal weight `1.0` if any member has unknown/zero price).
- **Selected-window provider weekly pace %:**
  - For each quota-bearing profile in the provider with `consumed_basis_points: Some(bp)`:
    - Effective elapsed seconds = `covered_seconds.unwrap_or(selected_duration_secs)`. Skip if `< 60` seconds (prevents noise on a brand-new partial clock hour).
    - Profile weekly pace ratio = `(bp as f64 / 10_000.0) / (elapsed_secs as f64 / 604_800.0)`.
  - Provider weekly pace % = `round(100.0 * sum(w_i * ratio_i) / sum(w_i))` over measurable profiles. Mark `partial` (`~`) if any profile had partial coverage or was unmeasurable while others were measurable.
  - Pay-as-you-go providers (no weekly quota usage/projections) display `payg` with a purple dashed track (`┄`) and `∞` tank.
- **Provider pool remaining % (`Tank`):**
  - Weighted average of `remaining_percent` across weekly (`!is_short_window`) `quota_projections` for the provider (preferring unscoped weekly windows per profile when multiple exist).
- **Provider weekly forecast (no 5h):**
  - `pace > 105%`: simulate the pooled staggered-reset tank over the next 7 days using each account's live `(remaining_percent, resets_at_epoch_seconds, weight)` draining at the selected-window pooled rate `pace_pct / 168.0` %/hr; if it reaches 0 before 7d, display `dry in Xd Yh`, else `tight · safe 7d`. (If no projection reset data, display `over pace`.)
  - `80%..=105%`: `on pace · safe`.
  - `< 80%`: `slack · ~{100 - pace}% unused` (or `idle` when `0%`).
- **Expanded role breakdown (largest-remainder rounding):**
  - Group `report.profile_role_usage` by provider → `UsageAgentRole`.
  - Compute each role's share of the provider's known role USD ticks (falling back to total tokens if all models in that provider are unpriced).
  - Allocate `share_pct` (summing to `100%`) and `role_pace_pct` (summing to provider's `pace_pct`) via largest-remainder rounding so expanded rows always sum cleanly to the parent provider row.
  - If a subscription provider shows positive quota burn (`pace_pct > 0`) with zero attributed Mjolnir turns in the window, show a single `External / untracked` row at `pace_pct` (`100% share`).

### 4. TUI Interaction & Layout (`mj-tui/src/session_usage_report.rs`)
- Centered modal target size `110×34` (clamping cleanly to smaller terminals; graceful compact layout below 14 inner rows).
- **Provider Pace list (`UsageControl::Providers`):**
  - Clickable/scrollable via `ChoiceList::render_with_rows`.
  - Clicking a provider row or pressing `Enter`/`Space`/`←`/`→` toggles `expanded_providers: BTreeSet<String>`.
  - Indented role rows render with `├─`/`└─` tree glyphs and a thinner meridian bar (`─` + `┆`) on the exact same `0%–100%–200%+` scale.
- **24h Chart & Role-Change Timeline (`UsageControl::Hours` + `UsageControl::Changes`):**
  - Keep existing 24h bar chart and multi-hour selection (`Click`/`←→`/`Ctrl` toggle/`Shift` range/`Enter` all), and make range selection work reliably inside the chart without touching text selection anywhere else:
    - Preserve `Down | Up` mouse modifiers so `Shift+Click` works reliably whenever the terminal forwards it (even if `Shift` is released before mouse-up).
    - Add **click-and-drag range selection** scoped strictly to the 24h hour bars (`UsageControl::Hours`, which is already an interactive control rather than selectable text): press on one bar, drag across to another bar, release to select the range.
    - Fix **`Shift+←/→`** keyboard range extension on `UsageControl::Hours` (previously blocked by `if ordinary` in `mj-chat/src/components/scope.rs:1254` and `Event::Mouse`-only modifier extraction).
  - Add a baseline axis row directly beneath the bars showing selected range highlights (`━━━`) and golden timeline pins (`▲①`, `▲②`, …) aligned on the exact hour column of each `role_changes` event.
  - Render a compact clickable `Role changes on timeline` list below the hour labels when changes exist in the 24h window. Clicking or activating a change selects all hours `>= hour_start_ms(change.changed_at_ms)` in the graph so the user immediately sees post-change pace and role breakdown.
- **Bottom Sessions List (`UsageControl::Rows`):**
  - Remove the old `UsageRow::Role` and `UsageRow::ModelTotal` entries completely.
  - Keep per-session rows scrollable below the timeline section.

## Implementation Slices

1. **Slice 1 — Backend & Persistence (`mj-core`, `mj-accounts`, `mj-store`, `mj-controller`, `mj-daemon`)**
   - Add serde-compatible report types & fields (`ProfileRoleUsageEstimate`, `RoleAssignmentChange`, `covered_seconds`, `is_short_window`).
   - Add `RoleAssignmentSnapshot` + `diff_role_assignments` in `mj-core`.
   - Add compatible SQLite migration 166 (`role_assignment_state`, `role_assignment_changes`), `sync_role_assignment_snapshot`, `load_role_assignment_changes`, and populate `profile_role_usage` + `role_changes` in `load_session_usage_report`.
   - Populate `covered_seconds` and `is_short_window` in `mj-accounts/src/quota_telemetry.rs`.
   - Wire daemon startup/reload config sync in `mj-daemon` (off-mutex via `spawn_blocking`).
   - Targeted tests: migration 165→166 compatibility, role-diff + restart idempotence, `profile_role_usage` + `role_changes` store query, `covered_seconds` / `is_short_window` telemetry tests.

2. **Slice 2 — TUI Usage Dashboard (`mj-tui`)**
   - Rename `Usage by session` → `Usage Dashboard` across modal, actions, pane menu, error notices.
   - Implement provider-level weekly pace % calculation, staggered-reset 7d pooled forecast, largest-remainder role pace allocation, `0%–100%–200%+` meridian bars, clickable `▸`/`▾` provider expansion, 24h timeline pins + clickable role-change rows, and removal of bottom role list.
   - Update affected golden files (`dashboard-dialogs.txt`, `support-pane-title-menu.txt`, `command-palette.txt`) and add focused tests for provider pace %, expansion toggle, role-sum invariant, timeline pin click selection, and short-terminal resilience.

3. **Verification & Review**
   - Run targeted tests across `mj-core`, `mj-accounts`, `mj-store`, `mj-daemon`, `mj-server`, `mj-tui` + `cargo clippy --all-targets -- -D warnings`.
   - Commit on `master`, run `request_review`, address findings, and push to `origin/master`.
