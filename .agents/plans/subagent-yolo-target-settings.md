# Give YOLO targets their own sub-agent choices

This ExecPlan is a living document. It follows `.agents/PLANS.md`, and its `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` sections must stay current.

## Purpose / Big Picture

Today every session uses one global set of Mjolnir sub-agent task choices. This change lets a person keep the existing shared choices for ordinary targets and configure a second set for targets that run with `permissions = "yolo"`. The Sub-agents settings page gains a checkbox, **Use the same subagents on all targets**, checked by default. Unchecking it shows a second group titled **YOLO Target Subagents** with Straightforward, Subtle, and Oracle model/effort rows. The maximum concurrent children setting stays shared. The second group starts empty when first revealed. Checking the box again hides the group and makes its values do nothing, but the values are retained in the saved configuration; unchecking later restores exactly those values rather than an empty group.

A "YOLO target" in this plan means a target whose permission mode is `yolo`. In `config.toml` that is an SSH bare runtime with:

    [targets.builder]
    kind = "bare"
    machine = "builder"
    permissions = "yolo"

Local bare targets keep `guardian` and container targets have no permission-mode setting, so neither uses the YOLO choices. This matches the dashboard's existing `[Y]` permission badge, which only bare targets with `yolo` show. The decision is recorded in the Decision Log because it determines which sessions the new group affects.

The behavior is visible in three places. The terminal Settings page shows the checkbox and the conditional group, and saving a new configuration writes either the shared section alone or both the shared section and `[subagents.yolo]`. Create and Move use the destination target's mode when deciding whether Mjolnir is configured and whether the Oracle row is available. Spawning a child or asking the oracle resolves the parent session's recorded target and uses the matching task choices.

## Progress

- [x] (2026-11-17) Write the plan and settle the YOLO definition.
- [x] Add core configuration fields, version 23, validation, and target helpers.
- [x] Resolve target-specific choices for create, move, spawn, oracle, and worker launch.
- [x] Extend the shared settings model and terminal settings rendering.
- [x] Update the web viewer's target data and New/Move availability checks.
- [x] Update documentation, goldens, and focused tests.
- [ ] Run affected tests and full validation, commit, and push.
  (Focused suites, goldens, the browser spec, formatting, and Clippy on the touched crates
  passed. The user asked to skip the remaining full-suite runs; commit and push follow.)

## Surprises & Discoveries

- Observation: `TargetRuntimeSettings` already records the target kind and execution policy, so a session's recorded placement can answer whether it runs with `permissions = "yolo"` without a new database or state field.
  Evidence: `mj-core/src/state/target_runtime.rs` stores `kind` and `execution_policy`; `TargetTemplate::permission_mode` maps an SSH bare template with `yolo` to `PermissionMode::Yolo` and therefore `ExecutionPolicy::Unconstrained`.

- Observation: the settings page is a shared model consumed by the terminal, the web viewer, the settings API, and search. A conditional group must be expressed in `mj_client::settings::page::visible_paths` and `page_plan`, not drawn ad hoc in one surface.
  Evidence: `mj-client/src/settings/page.rs`, `mj-client/src/settings/view.rs`, and `mj-tui/src/setup/render.rs` all call the same page model.

## Decision Log

- Decision: a YOLO target is an SSH bare target whose `permissions` is `yolo`; containers and local bare targets do not use the YOLO choices.
  Rationale: the product already calls exactly this mode YOLO and marks it with the `[Y]` badge. Extending it to every unconstrained target would silently change container sessions that have no YOLO setting and no badge.
  Date/Author: 2026-11-17, implementing agent.

- Decision: keep the YOLO choices in the saved configuration while the checkbox is checked, and ignore them while inactive.
  Rationale: the request requires re-checking to hide and no-op the values, but unchecking again must restore the previous values, including across saves.
  Date/Author: 2026-11-17, implementing agent.

- Decision: an empty YOLO set means no Mjolnir sub-agents and no oracle on YOLO targets; it does not fall back to the shared choices.
  Rationale: the request explicitly says the second set starts empty, and a fallback would make the split appear to work while silently using the shared values. A YOLO session whose policy is Mjolnir is refused at create/move with a remedy naming the YOLO keys, and an existing session that loses its YOLO choices reports the spawn or oracle error instead of using a different model.
  Date/Author: 2026-11-17, implementing agent.

- Decision: bump the configuration schema version from 22 to 23.
  Rationale: `config.toml` is read with `deny_unknown_fields`, so an older same-version reader would reject the new tables with an unknown-field error. The version bump makes the same situation say the file was written by a newer Mjolnir. Existing version 22 files are read and upgraded in memory on the next save.
  Date/Author: 2026-11-17, implementing agent.

## Outcomes & Retrospective

Implemented on 2026-11-17. `SubagentConfig` gained `yolo: SubagentChoices` and
`use_same_on_all_targets`, and the configuration schema advanced to version 23.
A target is a YOLO target when `TargetTemplate::permission_mode` is `Yolo`,
which is an SSH bare runtime with `permissions = "yolo"`; a session answers the
same question from its recorded `TargetRuntimeSettings`, so a session keeps the
mode it launched with.

The Sub-agents settings page ends with **Use the same subagents on all targets**,
checked by default. Unchecking it reveals the **YOLO Target Subagents** group
with three empty task rows, and the discovered model/effort choices are stored
per set (`_subagent_options.yolo.<task>`). While the box is checked the group is
absent from the page, from search, and from staged edits, and the stored YOLO
values stay in the file untouched; unchecking restores them. `max_concurrent`
stays one shared value.

Create, Move, spawn, and `ask_oracle` read the destination or session target's
mode, so a YOLO target with an empty YOLO set is refused with a remedy naming
`[subagents.yolo.straightforward]` rather than silently using the shared model.
The web viewer publishes each target's `permission_mode`, `subagents_configured`,
and `oracle_configured`, and uses them for its Mjolnir and Oracle controls.

Validated: `brokk-mj-core` (661 passed), `brokk-mj-client` (57 passed),
`brokk-mj-tui` (596 passed), the focused controller settings, sub-agent, YOLO,
and daemon-exchange tests, the updated terminal, API, and browser goldens, the
Playwright `settings.spec.js` and `viewer.unit.test.mjs` suites, `cargo fmt
--all --check`, and Clippy on the touched crates. Per the user's instruction the
remaining whole-suite controller and CLI runs were skipped, so they are the one
validation gap to repeat on the next full run.

Lessons: keeping the inactive YOLO values in the configuration is what makes the
checkbox reversible, but it also means every reader must ask `choice_for(_, yolo)`
rather than the older `choice`/`effective_choice`, and every settings row must be
hidden in one place (`hidden_key`) so search and staged edits cannot reach a no-op
value.

## Context and Orientation

Mjolnir is a Rust workspace. The settings model shared by every surface lives in `mj-client/src/settings/`. `schema.rs` supplies defaults, labels, help, choices, and whole-number parsing. `page.rs` decides which rows a page lists, where a row lives, and how it is edited. `view.rs` renders a page as data for the web viewer and the settings HTTP API. `draft.rs` turns a `Config` into an editable JSON draft and back. `discovery.rs` fills the model and effort choices obtained from the daemon's profile catalog. The terminal UI in `mj-tui/src/setup/` renders and edits that same model. The web viewer in `mj-controller/src/web/viewer.js` renders the JSON page produced by `view.rs`.

The configuration type is `mj_core::config::SubagentConfig` in `mj-core/src/config.rs`. It currently holds `straightforward`, `subtle`, and `oracle` task choices plus the shared `max_concurrent` limit. The file is serialized through `StoredConfig`, which also lives in that file. Configuration schema version is `CONFIG_VERSION`; any file whose version is greater is refused, and older versions are upgraded in memory.

A session's physical placement is recorded in `mj_core::state::TargetRuntimeSettings` (`mj-core/src/state/target_runtime.rs`) and reached through `SessionRecord::target_runtime_settings` in `mj-core/src/state.rs`. `TargetTemplate::permission_mode` in `mj-core/src/config/targets.rs` maps a local bare target to `guardian` and an SSH bare target to its configured `guardian` or `yolo` value.

The sub-agent choices are resolved in `mj-controller/src/controller/profile_config.rs` (`validate_session_subagent_policy`), `mj-controller/src/server/api/subagents.rs` (`resolve_subagent_policy_selection`), `mj-controller/src/controller.rs` (new-session oracle), `mj-controller/src/controller/move_session.rs` (move validation and oracle), `mj-controller/src/controller/worker_binary/launch.rs` (oracle tool and subtle task offering), and `mj-controller/src/server_runtime/api/oracle.rs` (the `ask_oracle` tool). The terminal wizard's availability decision is in `mj-tui/src/wizards/subagents.rs`; the web viewer's is in `mj-controller/src/web/viewer.js` and is fed by `ViewerTarget` rows in `mj-controller/src/server/viewer_types.rs`.

## Plan of Work

First, add `SubagentChoices` to `mj-core/src/config.rs`. It holds `straightforward`, `subtle`, and `oracle`, and is used as the nested `yolo` value of `SubagentConfig`. Add `use_same_on_all_targets: bool` with a default of true. Give `SubagentConfig` methods that take a `yolo` flag: `choice_for`, `effective_choice_for`, `offers_subtle_for`, `oracle_configured_for`, and `is_configured_for`. Keep `choice` for the shared fields where a caller intentionally asks for the shared set. Validation checks the shared choices always and the YOLO choices only when `use_same_on_all_targets` is false, so hidden values can be saved without blocking the setting that hides them. `is_default` accounts for the new fields, and `Config::selected_models` includes YOLO models so their efforts are discovered. Bump `CONFIG_VERSION` to 23 and accept version 22 in the upgrade range.

Add small target helpers rather than duplicating the mode rule: `TargetTemplate::is_yolo`, `TargetRuntimeSettings::is_yolo`, and `SessionRecord::is_yolo_target(&Config)`. `TargetRuntimeSettings::is_yolo` derives from the recorded kind (`ssh-bare`) and unconstrained execution policy, so it works for old records and does not require a new stored field.

Second, thread the target mode through resolution. `validate_session_subagent_policy` receives the target id and uses its template's mode. New-session oracle selection and Move's oracle selection use the destination mode. `SubagentBackend` gains `session_is_yolo_target(session_id)` so spawn can ask the daemon for the parent's recorded target; the daemon's `ExportRuntime` gains the same query. `resolve_subagent_policy_selection` takes the flag and selects the matching choice. `SessionRecord::effective_oracle` takes the flag. Worker launch computes it once from `session.is_yolo_target(&config)` and passes it to the oracle and subtle checks.

Third, extend the settings model. `defaults` expands a `subagents.yolo` table with three unset task entries and adds `use_same_on_all_targets = true`. `visible_paths` lists the shared limit and tasks, the checkbox, and, only when unchecked, the three YOLO task rows. `page_plan` gains a real heading group, **YOLO Target Subagents**, before the YOLO rows; the web page model already supports group headings, and the terminal renderer draws them. Labels, help, placeholder text, choices, read-only detection, and effort-reset behavior all learn the four-segment YOLO paths. `_subagent_options` stores the YOLO discovery results under `yolo.<task>` keys so a YOLO model's efforts are discovered independently. `hidden_key` treats `yolo` as hidden while the checkbox is checked, so search, staged edits, and direct page edits cannot reach a no-op value. `config_from_draft` drops empty task entries in both sets but keeps non-empty YOLO values when the shared choice is active.

Fourth, update the terminal wizard. `SubagentDraft` carries the destination target's mode, refreshes it whenever the wizard profile or target changes, and uses `oracle_configured_for` and `effective_choice_for` with that mode. Its unavailable message names the YOLO set when the target is YOLO. The New and Move review steps derive the mode from the wizard's selected target.

Fifth, update the viewer. `ViewerTarget` publishes `permission_mode`, `subagents_configured`, and `oracle_configured` for the set that target would use. `viewer.js` looks up the draft target for the New and Move screens, uses those flags for Mjolnir availability and the Oracle row, and gives the YOLO remedy when appropriate. The top-level snapshot flags remain for older clients and for drafts without a selected target.

Finally, update the terminal Settings golden and the recorded settings exchange/browser golden to cover the checkbox and conditional group. Add focused behavior tests for the retention rule (check, hide, uncheck, values return and are active again), for spawn resolution using different shared and YOLO models, and for the wizard refusing Mjolnir on a YOLO target whose YOLO set is empty even when the shared set is configured. Update `mj-core/assets/skills/mj/references/configuration.md` (both version lines and the `[subagents]` section), `docs/src/content/docs/profiles.md`, and any version-22 fixture literals.

## Concrete Steps

Work from the repository root. Every Cargo test runs in the dev profile and outside the restricted sandbox, because the suite uses loopback sockets and must not be sandboxed. Use an isolated `--instance` for any live binary or TUI check.

Focused checks during implementation:

    cargo test -p brokk-mj-core config::tests
    cargo test -p brokk-mj-client settings
    cargo test -p brokk-mj-controller subagent
    cargo test -p brokk-mj-tui global_subagent
    cargo test -p brokk-mj-tui yolo
    cargo test -p brokk-mj-controller settings_routes_hide_literal_secrets_and_save_through_the_shared_model

Update goldens only after the behavior is final:

    MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-tui golden_global_subagent_discovery_and_save
    MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-controller settings_routes_hide_literal_secrets_and_save_through_the_shared_model
    MJ_UPDATE_GOLDEN=1 cargo test -p brokk-mj-controller daemon_api_exchange
    cd tests/e2e/web && MJ_UPDATE_GOLDEN=1 ./node_modules/.bin/playwright test settings.spec.js --project deterministic

Final validation runs the full suites of every touched crate (`brokk-mj-core`, `brokk-mj-client`, `brokk-mj-controller`, `brokk-mj-tui`, and `brokk-mj-cli` if its remedies or fixtures change), then:

    cargo fmt --all --check
    cargo clippy --all-targets -- -D warnings
    git diff --check

Commit only the files this change owns on the current branch and push to `origin/master`. If the push is rejected, fetch and merge `origin/master`, resolve conflicts, and push the merge commit.

## Validation and Acceptance

Opening Settings > Sub-agents initially shows the shared limit, the three shared task rows, and the checked checkbox **Use the same subagents on all targets** at the bottom; no YOLO rows are visible. Unchecking the box immediately shows the heading **YOLO Target Subagents** with three empty task rows. Setting a YOLO Straightforward model makes YOLO Mjolnir available on an SSH bare target with `permissions = "yolo"`, while an ordinary target still uses the shared choices. Checking the box again removes the YOLO rows from the page and from search; saving then writes `use_same_on_all_targets = false` or omits it when true, and retains any `[subagents.yolo.*]` values. Unchecking restores the same values. The maximum concurrent children value is edited once and applies to both sets.

On a YOLO target with an empty YOLO set and a configured shared set, Create and Move show Mjolnir unavailable with a remedy naming the YOLO settings, and an explicit `mj new --subagents mjolnir` is refused with the same remedy. With a configured YOLO set, a spawned child uses the YOLO model and effort, and the oracle uses the YOLO oracle model. An ordinary target still uses the shared choices. A saved version 22 file loads, and the next save writes version 23.

## Idempotence and Recovery

The configuration change is additive and idempotent: loading and saving an unchanged file preserves every stored value, and every step can be rerun. There is no database migration. Tests keep their existing isolated config/data directories. If a golden update is interrupted, rerun the same update command; the golden file is replaced atomically by the test helper. Do not point a test build at the default instance.

## Artifacts and Notes

Not yet recorded.

## Interfaces and Dependencies

In `mj-core/src/config.rs`, add:

    pub struct SubagentChoices {
        pub straightforward: Option<SubagentChoice>,
        pub subtle: Option<SubagentChoice>,
        pub oracle: Option<SubagentChoice>,
    }

`SubagentConfig` gains:

    pub yolo: SubagentChoices,
    pub use_same_on_all_targets: bool,

and these methods:

    pub fn choice_for(&self, task_type: SubagentTaskType, yolo: bool) -> Option<&SubagentChoice>;
    pub fn effective_choice_for<'a>(&'a self, policy: &'a SubagentPolicy, task_type: SubagentTaskType, yolo: bool) -> Option<&'a SubagentChoice>;
    pub fn offers_subtle_for(&self, policy: &SubagentPolicy, yolo: bool) -> bool;
    pub fn oracle_configured_for(&self, yolo: bool) -> bool;
    pub fn is_configured_for(&self, yolo: bool) -> bool;

In `mj-core/src/state.rs`, add:

    pub fn is_yolo_target(&self, config: &Config) -> bool;

`SubagentBackend` gains:

    fn session_is_yolo_target(&self, session_id: String) -> BoxFuture<'_, AnyResult<bool>>;

`ViewerTarget` gains `permission_mode: Option<mj_core::config::PermissionMode>`, `subagents_configured: bool`, and `oracle_configured: bool`. The shared settings model keeps its existing `visible_paths(path, value) -> Vec<Vec<String>>` signature; `page_plan` changes to accept those full paths so it can place the YOLO heading.

Revision note (2026-11-17): Initial plan. Resolved the target-mode ambiguity in favor of the existing `permissions = "yolo"` meaning, made the inactive YOLO values retained but ignored, and included the configuration version bump required by strict configuration readers.
