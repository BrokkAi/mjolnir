# Select several transcript roles

This ExecPlan follows `.agents/PLANS.md` and remains the record of the implementation and validation.

## Purpose / Big Picture

After this change, a user can select any combination of transcript roles with repeated `mj transcript --role` flags. With no role flag, the CLI continues to hide `tool` and `terminal`; listing all eight roles shows the entire transcript. The CLI sends the selection to the HTTP API, which filters in SQLite before limiting each page. An API caller that omits `role` still receives every role, and existing callers that send one `role` continue to work.

The behavior is visible through `mj transcript --session SESSION --role agent --role user` and through repeated `role` query parameters on `GET /api/v1/sessions/{id}/transcript`. Both return only matching roles and preserve pagination cursors. `--finished-only` continues to mean closed agent messages and permits no role other than `agent`.

## Progress

- [x] (2026-10-05 12:45Z) Read repository instructions, confirmed a clean working tree on `master` at the requested baseline, and traced CLI, HTTP API, database paging, docs, and tests.
- [x] (2026-10-05 13:32Z) Added repeatable CLI and HTTP role selection, kept the CLI six-role default and API all-role default, and preserved the single-role query format.
- [x] (2026-10-05 13:32Z) Filtered selected roles in SQLite before paging; tests cover multiple roles, sequence ties, an empty filtered read followed by a later matching row, and finished-only cursor behavior.
- [x] (2026-10-05 13:32Z) Updated both reference pages; CLI tests passed (281 unit tests plus integration suites), controller tests passed (2,220 passed, 10 ignored), and clippy and format checks passed. The reviewed implementation is committed directly to `master`; it is not pushed.

## Surprises & Discoveries

- The CLI currently fetches every role and removes tool and terminal items after pagination. The API's database query already owns cursor computation, so moving the CLI's default role selection into that query lets each page fill with visible roles and keeps cursors server-owned.
- The database already advances `next_after_seq` over gaps and keeps tied events together. Its current filter accepts one role; applying a role list in the `matches` CTE and the `more` query preserves those existing cursor rules.
- `serde_urlencoded` presents one query value as a scalar and repeated keys as separate map entries. A plain `Vec` field rejected the single-role form, while an untagged scalar-or-list field rejected repeated keys as duplicates; `TranscriptQuery` therefore has a map visitor that collects repeated `role` entries and keeps duplicate checks for its other fields. The API test exercises both forms.
- Under default full-suite parallelism an unrelated controller readiness timing assertion once exceeded its 250 ms bound; it passed alone, and the complete controller suite passed with eight test threads.

## Decision Log

- Decision: represent repeated HTTP `role` parameters as a vector; an empty vector means no API role filter. Rationale: `role=agent` stays valid for existing clients, while `role=agent&role=user` carries a selection without a new query name. Date/Author: 2026-10-05, Codex.
- Decision: have the CLI send the six-role default (`user`, `agent`, `thought`, `plan`, `plan_proposal`, `system`) to the server, and require users to list all eight roles when they want tool and terminal entries too. Rationale: the API's unfiltered default remains all roles for existing callers and the web viewer; selection and pagination happen together on the server. Date/Author: 2026-10-05, Codex.
- Decision: normalize finished-only CLI reads to the `agent` role and reject any explicitly selected non-agent role before connecting. The API also validates this rule and implies `agent` when a caller omits roles. Rationale: callers get the same behavior at either interface and cannot accidentally page another role as finished agent output. Date/Author: 2026-10-05, Codex.
- Decision: deserialize `TranscriptQuery` with a map visitor rather than a plain vector field. Rationale: the HTTP query decoder exposes scalar and repeated `role` keys in different map-access forms, and the visitor preserves both without changing existing field validation. Date/Author: 2026-10-05, Codex.

## Outcomes & Retrospective

The CLI accepts repeated roles and sends the effective selection to the API. The database applies the selection before its page limit, and its existing cursor behavior still covers excluded rows, sequence ties, and the finished-agent barrier. The API accepts both legacy single-role and repeated-role query strings while its no-role default remains every role. The reference docs and behavior tests describe these contracts.

`mbx test -p brokk-mjolnir` passed its 281 unit tests and all applicable integration suites. `mbx test -p brokk-mj-controller -- --test-threads=8` passed 2,220 tests with 10 ignored. `mbx clippy --all-targets -- -D warnings`, `mbx fmt --all -- --check`, and `git diff --check` passed. The reviewed implementation and this plan are committed directly to `master` without a push.

## Context and Orientation

`mj-cli/src/api_commands.rs` defines `TranscriptArgs` and runs `mj transcript`. `mj-cli/src/api_client.rs` builds its HTTP request. `mj-controller/src/server/api/types.rs` deserializes query parameters, and `mj-controller/src/server/api/turns.rs` validates them and calls the backend. The backend trait is in `mj-controller/src/server/api/subagent_backend.rs`; the live implementation is in `mj-controller/src/server_runtime/api.rs`. Finally, `mj-controller/src/database/materialized.rs` reads transcript entries from SQLite and determines the page cursor. Tests for the CLI, API, and database are colocated in their respective source modules.

The wire roles are `user`, `agent`, `thought`, `tool`, `terminal`, `plan`, `plan_proposal`, and `system`. Internally, `terminal` is stored as `terminal_output`. `after_seq` is an exclusive cursor: each next request starts after the largest sequence already covered. The API computes `latest_seq` over all transcript roles, regardless of a filter.

## Plan of Work

In `mj-cli/src/api_commands.rs`, replace the optional single role and `--all-roles` switch with a repeatable role vector. Keep Clap's role parser so unknown names fail with the enum's accepted values. Resolve the effective roles before opening the API client: use the six-role default when no role is selected, use the given roles otherwise, and use only `agent` for finished-only after rejecting any selected non-agent role. Remove client-side item filtering.

In `mj-cli/src/api_client.rs`, add one encoded `role` query parameter per selected role. In `mj-controller/src/server/api/types.rs`, deserialize repeated `role` keys into a vector whose empty default means all roles. Validate finished-only combinations in `turns.rs`, imply agent for finished-only, and pass the selected list through the backend trait and live implementation.

In `mj-controller/src/database/materialized.rs`, turn selected roles into their stored JSON kind names and apply the list in both the page query and its `more` check before ordering and limiting. Keep the existing sequence tie boundary, filtered-gap cursor advancement, global `latest_seq`, and finished-only open-message barrier. Update API fakes and add coverage for a single legacy role parameter, repeated role parameters, the unfiltered API default, multi-role database selection, excluded-row cursor progress, and finished-only validation.

Update `docs/src/content/docs/cli-reference.md` to describe repeatable roles, the six-role CLI default, all eight roles needed for a full transcript, finished-only combinations, and server-side pagination. Update `docs/src/content/docs/api-reference.md` to describe repeated role query parameters, the all-role API default, and unchanged cursor behavior.

## Concrete Steps

Run commands from `/home/jonathan/Projects/mjolnir`. Use `mbx` directly so Rust builds use the configured shared cache; do not set or redirect Cargo's target directory.

1. Edit the CLI, API, backend, database query, tests, and the two reference pages described above.
2. Format-check with `mbx fmt --all -- --check`.
3. Run `mbx test -p brokk-mjolnir` and `mbx test -p brokk-mj-controller -- --test-threads=8`. The package names come from the `[package]` entries in `mj-cli/Cargo.toml` and `mj-controller/Cargo.toml`.
4. Run `mbx clippy --all-targets -- -D warnings`.
5. Review `git diff` and `git status`, stage only files changed for this feature, and commit on the existing `master` branch. Do not push.

## Validation and Acceptance

The CLI accepts `--role agent --role user`, rejects an unknown role with a clear diagnostic, and defaults to the six roles that omit tool and terminal items. `--finished-only` works without a role or alongside `--role agent`, and rejects a selected role such as `user`. The API accepts both one `role=agent` and repeated `role=agent&role=user`; when no role is supplied it still requests every role. Database tests prove selected roles are filtered before page limits, ties remain together, and `next_after_seq` advances across an empty filtered result so a subsequent request after that cursor can read later matching content. Finished-only cursors remain before open agent items.

Acceptance requires the touched-crate tests, `mbx clippy --all-targets -- -D warnings`, and `mbx fmt --all -- --check` to pass. The commit is made on `master`, and no push is performed.

## Idempotence and Recovery

Code, test, and documentation edits are safe to repeat. Validation uses mbx's configured build cache without relocating build output. If a validation command fails, fix the relevant source or formatting issue and rerun the required checks. Stage only feature files; the initial working tree was clean, and no other commit or branch change is needed.

## Artifacts and Notes

The CLI role list and API filter share `mj_core::transcript::TranscriptRole`, so no new wire role names or persisted values are introduced. No database migration is needed because the transcript schema does not change.

## Interfaces and Dependencies

The public CLI accepts `--role <role>` zero or more times. The HTTP query uses the existing key repeatedly, such as `?role=agent&role=user`. The API query stores the repeated values as `Vec<TranscriptRole>`; an empty vector means all roles. The database and backend receive `Vec<TranscriptRole>`. No crate dependency, stored role value, or database migration is added. The query uses SQLite's existing JSON support to match selected stored kind names as an array; `json_extract` is already used by the materialized transcript query.
