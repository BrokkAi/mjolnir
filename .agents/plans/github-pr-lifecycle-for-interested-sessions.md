# Tell interested sessions when a pull request merges, closes or reopens

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` from the repository root and must be maintained in accordance with that file.


## Purpose / Big Picture

Mjolnir (`mj`) runs many coding-agent sessions on one machine. A background GitHub watcher in the daemon (the long-running control process, `mj daemon-run`, code in `mj-controller/`) reads repository activity. When a new pull request (PR) appears, it decides for each live session whether the PR is relevant to that session's work. If it is, it puts a short notice in the session's mailbox. The notice is "queued": it does not start a turn (`wake=false`), and the agent sees it at its next turn. A session that receives a PR's open notice this way is an **interested session** for that PR.

Today an interested session never hears what happens to the PR afterwards. Only the session that opened the PR, its **creator**, is told when it merges, closes without merging or reopens. Those three changes are the PR's **lifecycle events**, and the creator's notice wakes it. An offline study of the live store (Oct 8 to Nov 13, 2026, artifacts in `/mnt/optane/jev-panel-20261111/merge-*`) found that interested sessions often keep waiting on a PR that has already merged. In 34 hand-confirmed cases, 26 sessions kept waiting after taking a turn, for gaps from under a minute to over 7 hours, and about half never learned within 72 hours. 19 of those cases involved a "Merge batch" PR: the batch's open notice reached them, but the batch's merge reached no one.

After this change:

- every interested session receives the PR's lifecycle events as queued mailbox notices, worded so they do not claim the PR is theirs;
- the creator's notices are unchanged;
- PRs opened by another account, including `mergemarshall[bot]` merge batches, are covered.

It is enabled by default, with no setting. You can see it working by opening a PR in a watched repository, letting a second session be told about it, merging the PR, and watching the second session's next turn begin with "PR owner/repo#N "title" was merged by …" (the creator's notice says "Your PR …"). The effect is then measured from the store after a week (Milestone 4).


## Progress

- [x] (2026-11-13 16:40Z) Offline study, code map and design. The map is in this plan's Context section.
- [x] (2026-11-13 17:24Z) Milestone 1: added interested-recipient lifecycle wire/rendering, protocol 65 gating, generic older-worker downgrade, and golden/regression coverage; targeted tests, `cargo check --all-targets`, and Clippy passed.
- [x] (2026-11-13) Milestone 2: schema revision 130 (breaking): watches without a creator, and the recorded latest lifecycle event. `migrate_github_watch_followers` in `schema.rs`; tests `watch_followers_migration_keeps_rows_and_refuses_older_builds` and `a_watch_outlives_its_creator_while_a_live_session_is_interested`.
- [x] (2026-11-13) Milestone 3: fan-out inside the commit transactions (`commit_github_timeline`, `commit_github_item_classification`), interested-only watches, credential selection and the creator-less comments skip. Tests: the extended `watcher_filters_lifecycle_activity_by_actor_and_creator_turn` and `watcher_reports_a_close_at_the_merge_time_as_a_merge`, and the commit-order test `an_interested_session_is_told_of_a_merge_in_any_commit_order`.
- [ ] Milestone 4: measurement after one week enabled.


## Surprises & Discoveries

- Observation: merge batches reach no one today, not even a creator.
  Evidence: all 967 `Merge batch` PRs seen since Oct 19 are authored by `mergemarshall[bot]`. A watch row is created only when a local session opened the item with the same GitHub account (`mj-controller/src/database/github_watch.rs`, `commit_github_item_classification`), so none has a watch. Only two older batches (#4961, #5211), which a session opened itself, have one.
- Observation: only 46% of the PRs delivered as relevant opens have a watch today (1,583 of 3,404 distinct PRs). The rest are batches (843), other bot PRs (266), other authors (365), and 347 not in `github_seen_items`.
- Observation: the outbox key is the primary key, and GitHub inserts ignore conflicts. A recipient that shared the creator's key would silently receive nothing.
  Evidence: `enqueue_github_event` uses `INSERT ... ON CONFLICT(event_key) DO NOTHING`.
- Observation: classification and timeline polling run concurrently (`tokio::join!` in `mj-controller/src/github_watch/service.rs`). A session classified after the merge was committed would never be told. Batch PRs merge within minutes of opening, so this is common, not rare.
- Observation: SQLite refuses `ALTER TABLE ... RENAME` while a trigger on another table names a table that no longer exists. The migration therefore drops the two `sessions` triggers before rebuilding `github_watch_items` and recreates them afterwards.
- Observation: a creator-less watch has no creator whose destruction removes it, so without a rule it would be polled for ever once its last interested session was destroyed.- Observation: a creator can attach to a creator-less watch (its classification arrives after an interested session's) while a poll that skipped the review-comment read for that watch is in flight. Recording that poll's progress would skip the comments the creator is owed.
  Evidence: `commit_github_timeline` reads the creator from the row; the test `an_interested_session_is_told_of_a_merge_in_any_commit_order` covers it. The commit now leaves progress unrecorded in that case, so the next poll redoes the read; outbox keys make the repeat harmless.
- Observation: the creator's own-activity filter has the same imprecision: a creator mid-turn is not told when another session sharing the account merges its PR. This is pre-existing and remains out of scope; the creator's filter is unchanged.

## Decision Log

- Decision: enabled by default, with no configuration flag; measure after a week.
  Rationale: Jonathan's instruction ("enable and measure, not flag and measure"). A notice is about 35 tokens and queued; the risk check found no unrequested work from queued notices (0 of 25 hand-read outward writes after queued opens were new work).
  Date/Author: 2026-11-13, Jonathan.
- Decision: lifecycle events only (merged, closed without merging, reopened). Interested sessions do not get comments or reviews.
  Rationale: the evidence concerns merges. Comments are much more frequent and would be noise for a session that did not open the PR.
  Date/Author: 2026-11-13, parent agent.
- Decision: interested recipients' notices are queued (`wake=false`); creators keep `wake=true`.
  Rationale: Jonathan approved queued delivery. A wake costs a whole turn (median 1.55M tokens read), and the earlier wake study found that wakes buy little.
  Date/Author: 2026-11-13, Jonathan / parent agent.
- Decision: cover PRs that have no creator watch, including merge batches, by keeping a watch row for every PR with at least one interested session.
  Rationale: the batch merge is the missing signal in 19 of the 34 confirmed waiting cases. This adds no new kind of GitHub request: the existing timeline read runs for more items, and only when the repository feed shows the item was updated.
  Date/Author: 2026-11-13, parent agent.
- Decision: the recipient list and the own-activity filter are decided inside the database write transaction that commits the timeline progress. A session classified later receives the PR's recorded latest lifecycle event inside its own classification transaction.
  Rationale: SQLite serializes write transactions, so for each interested session exactly one of the two transactions runs second and sees the other's row. A lost notice becomes impossible, not just unlikely; per-recipient keys make any overlap collapse to one row. This replaces a design that read recipients during the network fetch, which left a window between the fetch and the commit.
  Date/Author: 2026-11-13, parent agent.
- Decision: a structured recipient field on the lifecycle body, a protocol gate, daemon rendering for older workers, and a breaking migration (the next revision; 130 at the time of writing). Jonathan approved the breaking migration on 2026-11-13.
  Rationale: the alternative, daemon-rendered plain text for interested sessions, would make one kind of event travel by two paths and lose its structure in the transcript and web viewer. The repository's rules classify both changes as breaking: a nullable `creator_session_id` cannot be read by an older daemon, and a new field in stored `event_json` is refused by `deny_unknown_fields`. That precedent is migration 118. AGENTS.md says a feature request does not authorize an incompatible upgrade of the live store, so this needs Jonathan's explicit approval.
  Date/Author: 2026-11-13, parent agent.
- Decision: issues are out of scope; issue lifecycle stays undelivered.
  Rationale: a hand read of 34 issues with stale-looking follow-ups found 2 real cases, each one sentence with no consequence (`/mnt/optane/jev-panel-20261111/issue-handread/summary.md`).
  Date/Author: 2026-11-13, parent agent.
- Decision: when a creator session is destroyed, its watch row stays if interested sessions remain, with `creator_session_id` set to NULL. Otherwise the row is deleted, as today.
  Rationale: today destroying the creator deletes the watch, which would silently stop lifecycle notices to everyone else.
  Date/Author: 2026-11-13, parent agent.
- Decision: the migration-130 triggers also delete a creator-less watch when its last live interested session is deleted or destroyed.
  Rationale: the plan's rule keeps a watch while live interested sessions remain; the same rule must end a creator-less watch with its last follower. Otherwise it is polled with no recipient.
  Date/Author: 2026-11-13, implementer of Milestones 2 and 3 (consistent with the plan's rule; not a design change).- Decision: migration 130 also creates a creator-less watch for every PR that already has a live interested session and no watch, with cursors at the migration time.
  Rationale: the classifier never revisits a classified pair, so without it a PR a session was told about before the upgrade is never polled. Cursors at the migration, as migration 85 did, report what happens next without replaying history or reading every old PR's timeline at once. (Review finding.)
  Date/Author: 2026-11-13, implementer of Milestones 2 and 3; flagged to the parent in the report.
- Decision: when a creator attaches to a creator-less watch, the watch's cursors go back to the item's creation.
  Rationale: what the watch read without a creator skipped review comments and discarded creator-only timeline events. The creator is owed the whole timeline, as a creator watch starting at the item's creation reads it. Interested sessions' rows have their own keys, so nothing is delivered twice. (Review finding.)
  Date/Author: 2026-11-13, implementer of Milestones 2 and 3.


- Decision: the interested wording is plain: "PR {reference} \"{title}\" was merged by …", and the transcript line is "PR {reference} was merged by …". It does not explain why the session is being told.
  Rationale: Jonathan asked to drop "which you were told about as possibly related to your work" as noise. Leaving out "Your" is enough to show the PR is not the session's own.
  Date/Author: 2026-11-13, Jonathan.
- Decision: `commit_github_timeline` records progress only if the row's creator equals the creator its poll was planned for, and only if the row's cursors still equal the poll's snapshot (compare-and-set). Backfilled cursors are truncated to the second.
  Rationale: a creator that attaches or is replaced during a poll, or after a poll, rewinds the cursors for a full read, and a stale poll must not undo that. GitHub's timestamps have one-second precision, so a cursor with milliseconds could compare later than an event in the same second. (Review findings.)
  Date/Author: 2026-11-13, implementer of Milestones 2 and 3.


## Outcomes & Retrospective

None yet.


## Context and Orientation

Terms used below:

- **Mailbox**: the per-session queue of notices from outside the conversation. The daemon writes durable rows to the SQLite table `mailbox_outbox`: `event_key` (the primary key), `target_session_id`, `event_json`, `wake`, `unpark`, `created_at`, `accepted_at`, `failure`. The delivery owner, `mj-controller/src/mailbox_outbox.rs`, sends them to the session's worker.
- **Worker**: the per-session process, `mj-worker`, that runs the agent. Workers keep running across daemon upgrades. Each publishes a **relay protocol** number, so the daemon knows which event shapes it understands. The current number is `RELAY_PROTOCOL_VERSION` in `mj-core/src/relay.rs`, which equals `RELAY_OPEN_REQUEST_PROTOCOL = 64` at the time of writing.
- **Event body**: `mj_core::mailbox::MailboxEventBody` (`mj-core/src/mailbox.rs`), a serde enum tagged by `type` with `deny_unknown_fields`, so an older reader refuses any field it does not know. `MailboxEventBody::minimum_relay_protocol` names the earliest worker protocol that understands each body. `describe_mailbox_event` and `render_mailbox_event` in the same file produce the agent text and the one-line transcript text. The worker, the daemon and the transcript projection all share them.
- **Classification**: the per-session relevance decision for a new GitHub item. It is stored in `github_watch_classifications`, with primary key (owner, repo, number, session_id) and columns `interested` and `created`. `created=1` marks the creator. `interested=1, created=0` marks an interested session, and that row is the authoritative record that the session was told about the item. Some legacy rows have `interested=1, created=1`; always filter on `created=0`. `commit_github_item_classification` in `mj-controller/src/database/github_watch.rs` writes the classification and the open notice (key `github:{owner}/{repo}#{n}:interest:{session}`) in one transaction.
- **Watch**: a row in `github_watch_items`, with primary key (owner, repo, number). It holds `creator_session_id` (today `NOT NULL`) and per-item cursors (`timeline_at`, `timeline_id`, `review_comments_at`, `review_comments_id`, `processed_at`). Only watched items get timeline polling. A watch row is created in `commit_github_item_classification`, only when `created` is true. Migration 121 (in `mj-controller/src/database/schema.rs`) added triggers that delete a watch when its creator session is deleted or destroyed, and that refuse to insert a watch for a gone session.
- **Timeline poll**: `poll_repository_timelines` and `poll_item_timeline` in `mj-controller/src/github_watch/service.rs`. For each watch whose recorded `updated_at` is not earlier than `processed_at`, they read `repos/{o}/{r}/issues/{n}/timeline`. They also read `pulls/{n}` when a close might be a merge (Merge Marshall lands commits, leaving only a `closed` entry), and `pulls/{n}/comments`. `map_item_activity` and `map_timeline` turn entries into events. The arm `"merged" | "closed" | "reopened" if watch.kind == "pull_request"` builds `MailboxEventBody::GithubPullRequestLifecycle { change, number, title, actor, repo }`, with event key `github:{owner}/{repo}#{n}:{merged|closed|reopened}:{timeline entry id}`. A `closed` that follows a merge is suppressed, and a `closed` within 5 s of `merged_at` counts as a merge. Today the loop then calls `authored_during_session_turn` (skipping events the creator itself made during one of its turns, matched by the shared credential login and the creator's turn windows) and pushes rows for `watch.creator_session_id` with `wake=true`. Finally `commit_github_timeline` inserts the rows and advances the cursors in one transaction.
- **Turn windows**: the intervals when a session was running a turn. `load_github_session_turn_windows_from` and `harness_turn_windows` in `github_watch.rs` rebuild them from the store (`api_events` turn facts and `materialized_transcript_items` markers). `harness_turn_windows` already takes a `&rusqlite::Transaction`.
- **Credential choice**: `poll_repository` in `service.rs` resolves a GitHub token from the first live session for the repository, or else from the first watch's `creator_session_id`.
- **Downgrade for older workers**: `prepare_review_or_report` in `mailbox_outbox.rs` replaces a body with `MailboxEventBody::PlainText { text: render_mailbox_event(event) }` when the target worker is too old. The durable row stays unchanged. Today it does this for `ReviewVerdict` always, and for `SubagentFinished` below its minimum protocol.

Tests and goldens that cover this area, all to be extended rather than duplicated:

- `watcher_filters_lifecycle_activity_by_actor_and_creator_turn` and `watcher_reports_a_close_at_the_merge_time_as_a_merge` in `service.rs`, using the fixtures `watch_classification`, `fake_timeline`, `fake_pull`, `FakeGithub` and `start_fake_github`;
- `classification_uses_opening_tool_calls_and_the_credential_author` in `service.rs`, which today asserts that another account's batch has no watch;
- `rows_of_a_gone_session_cannot_outlive_it` in `mj-controller/src/database/mailbox.rs`;
- `github_timeline_migration_resets_progress_and_keeps_watches` in `schema.rs`;
- `review_verdict_reaches_existing_workers_with_the_daemon_closeout` in `mailbox_outbox.rs`;
- the goldens `mj-core/tests/golden/mailbox-events.txt` (fixtures in `mj-core/src/mailbox.rs` tests) and `mj-transcript/tests/golden/mailbox-delivery-transcript.txt` (`golden_mailbox_delivery_transcript`).


## Plan of Work

**Milestone 1: wire shape, rendering, gate and downgrade.**

In `mj-core/src/mailbox.rs`, add a field to `MailboxEventBody::GithubPullRequestLifecycle`:

    #[serde(default, skip_serializing_if = "LifecycleRecipient::is_creator")]
    recipient: LifecycleRecipient,

Define `pub enum LifecycleRecipient { #[default] Creator, Interested }` with `#[serde(rename_all = "snake_case")]`. Creator bodies serialize exactly as before, byte for byte.

In `describe_mailbox_event`, `Interested` renders as:

- agent text: `PR {reference} "{title}" was merged{by_clause}.`, with "was closed without merging" and "was reopened" for the other changes;
- transcript line: `PR {reference} was merged{by_clause}.`, and so on.

`Creator` text is unchanged.

Add `pub const RELAY_FOLLOWED_LIFECYCLE_PROTOCOL: u32 = 65;` in `mj-core/src/relay.rs` and set `RELAY_PROTOCOL_VERSION` to it. In `minimum_relay_protocol`, an `Interested` lifecycle body returns 65; everything else is unchanged.

In `mj-controller/src/mailbox_outbox.rs`, turn the downgrade into one rule: render to `PlainText` whenever `protocol < event.body.minimum_relay_protocol()`. Keep the existing unconditional `ReviewVerdict` case and its comment. Remove the special `SubagentFinished` arm, which becomes a case of the general rule. A protocol-64 worker therefore receives an interested notice as daemon-rendered text with the new wording, and never as a body it would refuse.

Update the two goldens with an `Interested` merged case, and add an old-worker case beside `review_verdict_reaches_existing_workers_with_the_daemon_closeout`.

**Milestone 2: the next schema migration (breaking).**

In `mj-controller/src/database/schema.rs`, add the next revision and set `SCHEMA_VERSION` in `mj-controller/src/database.rs` to it. At the time of writing `SCHEMA_VERSION` is 129, so the revision is 130. Other work lands migrations often, so take the number at merge time and renumber if master has moved. The migration:

1. Rebuilds `github_watch_items` with `creator_session_id TEXT` (nullable), keeping every column and row, and adds `lifecycle_event_key TEXT`, `lifecycle_event_json TEXT`, `lifecycle_author TEXT` and `lifecycle_at TEXT`. These four record the most recent delivered-or-filtered lifecycle event for the item: its base key, its serialized `MailboxEventBody` with `recipient` left as `Creator`, the GitHub login that caused it, and its timestamp.
2. Recreates the migration-121 triggers for the rebuilt table, with one change. When a session is deleted or destroyed, a watch it created is deleted only if no `interested=1, created=0` classification for another live session remains. Otherwise its `creator_session_id` becomes NULL. The insert trigger accepts a NULL creator.
3. Sets `minimum_compatible_version` to the new revision.

The comment beside it must say: "Breaking: `github_watch_items.creator_session_id` may be NULL, which an older daemon decodes as a string and fails on; and interested-session lifecycle rows in `mailbox_outbox.event_json` carry a `recipient` field that an older daemon refuses with `deny_unknown_fields`."

`GithubItemWatch.creator_session_id` in `github_watch.rs` becomes `Option<String>`, and `load_github_watches` reads the new columns.

**Milestone 3: fan-out, interested-only watches and credentials.**

In `commit_github_item_classification`:

- **Interested PR session** (`interested && !created && kind == "pull_request"`): insert a watch row with a NULL creator if none exists (`ON CONFLICT DO NOTHING`), with cursors at the item's `created_at`, as creator watches use today. Then, in the same transaction, if the watch row has a recorded lifecycle event, enqueue it for this session with key `{lifecycle_event_key}:{session_id}`, `recipient: Interested` and `wake=false`, unless the own-activity filter (below) says this session caused it.
- **Creator** (`created`): insert the watch as today. If a creator-less row already exists, set `creator_session_id` where it is NULL.

Because a new interested-only watch starts its cursor at the item's creation, its first poll reads the whole timeline, so a merge that happened before the watch existed is still found and delivered.

Change `commit_github_timeline` to take the mapped events instead of finished outbox rows. Each event carries its base key, its body, its author login and its timestamp. The function also takes the credential login. Inside its single write transaction it:

1. Reads the recipients: the watch's creator, if any, and every `interested=1, created=0` session for the item, excluding the creator.
2. For each event and recipient, applies the own-activity filter, using turn windows read through the same transaction. Refactor `authored_during_session_turn` into a synchronous function over `&rusqlite::Transaction`, using `load_github_session_turn_windows_from`'s queries and `harness_turn_windows`. The decision rule stays the same, and so does the "deliver it" fallback when turn windows cannot be read, with its warning.
3. Enqueues rows:
   - the creator gets every event kind as today, with today's key and `wake=true`;
   - each interested session gets lifecycle events only, with key `{base key}:{session_id}`, `recipient: Interested` and `wake=false`.
4. Records the last lifecycle event in the batch (by timestamp) in the watch's `lifecycle_*` columns.
5. Advances the cursors.

In `poll_item_timeline`, when the watch has no creator, skip the `pulls/{n}/comments` read and map only lifecycle events. The `pulls/{n}` merge-time read stays, because the merge decision needs it.

In `poll_repository`, choose the credential from the first live session, or else from the first watch that has a creator. If there is neither, skip the repository and log at debug level. A creator-less watch cannot supply a token.

**Milestone 4: measurement.** This is described under Validation and Acceptance. It needs no code in the repository.


## Concrete Steps

All commands run from the repository root, `/home/jonathan/Projects/mjolnir` or a clone of it. Run `cargo test` outside the restricted sandbox, because the suite uses loopback sockets.

During each milestone, run only the affected tests:

    cargo test -p mj-core mailbox
    cargo test -p mj-controller github_watch
    cargo test -p mj-controller mailbox_outbox
    cargo test -p mj-controller schema
    cargo test -p mj-transcript golden_mailbox_delivery_transcript

After the last milestone, run once:

    cargo clippy --all-targets -- -D warnings
    cargo test -p mj-core
    cargo test -p mj-controller
    cargo test -p mj-transcript
    cargo test -p mj-worker relay

Golden files are regenerated with the repository's usual golden update procedure for each test. Read each test's failure message, which names the command. Review every golden diff by hand: creator lines must be unchanged.


## Validation and Acceptance

These tests must fail before the change and pass after it:

1. Extend `watcher_filters_lifecycle_activity_by_actor_and_creator_turn` with an interested session for the same item. The outbox must hold the creator's rows unchanged (same keys, `wake=1`). It must also hold one row per lifecycle event for the interested session, with key `…:{session}`, `wake=0` and `"recipient":"interested"`, including shared-account activity during its own turn, and no comment or review rows for it.
2. Extend `watcher_reports_a_close_at_the_merge_time_as_a_merge` with a `mergemarshall[bot]` batch PR that has no creator and two interested sessions. After a `closed` entry at the `merged_at` time, both receive one `merged` row each. No `pulls/{n}/comments` request is made for that item; assert this on the fake server's request log.
3. A tricky-condition test, `an_interested_session_is_told_of_a_merge_in_any_commit_order`. It carries no Hard-won marker, because AGENTS.md keeps that marker for defects found after the code was believed done, and this race was handled in the design. Commit a timeline poll that merges the PR, then commit a classification for a new interested session. The new session must receive exactly one `merged` row, with the recorded key. Run the reverse order too: classification committed between the poll's network fetch and its commit. That session must also receive exactly one row.
4. A migration test, beside `github_timeline_migration_resets_progress_and_keeps_watches`. Build an isolated store at the previous revision with watch rows, triggers and outbox rows, then migrate it. Rows and creators must be preserved, the new columns must be NULL, `minimum_compatible_version` must equal the new revision, and a check from the previous revision must refuse the store. Extend `rows_of_a_gone_session_cannot_outlive_it`: destroying a creator that has interested sessions leaves the watch with a NULL creator; destroying one with no interested sessions deletes it.
5. A downgrade test in `mailbox_outbox.rs`. A protocol-64 worker receives an interested lifecycle row as `PlainText` with the new wording; a protocol-65 worker receives the structured body.
6. Goldens: `mailbox-events.txt` and `mailbox-delivery-transcript.txt` show the new interested lines, with creator lines unchanged.

**End-to-end check in an isolated instance**, never the default one. Run the new build with `--instance pr-lifecycle-test` against a scratch repository you control:
1. Open a PR from session A.
2. Make sure session B is told about it.
3. Merge the PR.
4. Prompt session B.

B's next turn must start with "PR … was merged by …", and A must be woken with "Your PR … was merged".

**Measurement**, after one week enabled on the live install. Read-only SQL with `mode=ro` and `PRAGMA query_only=1`:
- Deliveries per day: `SELECT count(*) FROM mailbox_outbox WHERE wake=0 AND event_json LIKE '%"recipient":"interested"%' AND created_at >= ?`. Filter by `target_session_id` first, because pattern scans over the whole table are slow.
- Delivery lag: the time from `json_extract(event_json,'$.created_at_ms')` to the transcript row `system:mailbox:delivered:{event_key}` in `materialized_transcript_items`.
- Effect: rerun the offline merge scan (`/mnt/optane/jev-panel-20261111/merge-scan/scripts`, then the anchors pass in `merge-anchors/`) on the week after installation. Compare with the Oct 8 – Nov 13 baseline:
  - the share of interested pairs whose session still treats the PR as unmerged after its first post-merge turn;
  - the time from the merge to the session's first statement that it merged.
- Risk: hand-read 20 interested lifecycle deliveries that were followed by a git rebase, a master merge or a `gh` write on that PR, for work nobody asked for.
- GitHub load: compare timeline requests per hour in the daemon tracing logs (`logs/mj-daemon-*.log`) before and after.

Report the results in `Outcomes & Retrospective`.


## Idempotence and Recovery

Every enqueue uses a deterministic per-recipient key and `ON CONFLICT DO NOTHING`, so replaying a poll or a classification cannot duplicate a notice. The timeline reader already replays the boundary second on purpose and relies on this.

The migration runs in one `BEGIN IMMEDIATE` transaction. Test it on isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` copies before any live install. Being breaking, it cannot be rolled back by running an older binary. To undo the feature without a schema change, stop the classification path from creating interested-only watches and the timeline commit from enqueueing interested rows; rows already queued deliver harmlessly.


## Artifacts and Notes

Study artifacts, outside the repository, in `/mnt/optane/jev-panel-20261111/`:

- `merge-scan/report.md`: the frame, its coverage and its classes;
- `merge-handread/` and `merge-handread2/summary.md`: hand-confirmed waiting cases, with gaps after the first post-merge turn of 41 s, 1.5 min, 4 min, 16 min, 34 min, 43 min, 45 min, 1.9 h, 2.1 h, 2.3 h, 2.4 h, 6.8 h, 7.1 h and 7.3 h, plus about 13 never resolved within 72 h;
- `outward-write-handread/summary.md`: 0 of 25 writes after queued opens were new work;
- `issue-handread/summary.md`: why issues are out of scope.


## Interfaces and Dependencies

In `mj-core/src/mailbox.rs`:

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum LifecycleRecipient { #[default] Creator, Interested }
    impl LifecycleRecipient { pub fn is_creator(&self) -> bool }

In `mj-core/src/relay.rs`:

    pub const RELAY_FOLLOWED_LIFECYCLE_PROTOCOL: u32 = 65;

In `mj-controller/src/database/github_watch.rs`:

    pub(crate) struct GithubItemWatch { pub creator_session_id: Option<String>, /* existing fields */ pub lifecycle: Option<RecordedLifecycle> }
    pub(crate) struct RecordedLifecycle { pub key: String, pub body_json: String, pub author: Option<String>, pub at: String }
    pub(crate) struct MappedGithubEvent { pub key: String, pub body: MailboxEventBody, pub author: Option<String>, pub timestamp: String, pub lifecycle: bool }
    pub(crate) fn commit_github_timeline(watch: GithubItemWatch, progress: GithubItemProgress, events: Vec<MappedGithubEvent>, credential_login: Option<String>) -> Result<()>

No new crates or dependencies.


Revision note (2026-11-13): first version, written after the code map. Jonathan then approved the breaking migration, and the Decision Log records it. After master moved (schema 129, relay protocol 64), the plan uses the next numbers, 130 and 65, and says to take them at merge time.

Revision note (2026-11-13, integration): the plan asked for a Hard-won marker on the commit-order test. That contradicts AGENTS.md, which keeps the marker for defects found after code was believed done, so the marker was removed.


- Decision: interested sessions have no own-activity filter; always enqueue lifecycle events for them.
  Rationale: a shared GitHub login and overlapping turn cannot establish which session performed the action. A queued echo costs one line, while a false skip permanently loses the merge notice this feature exists to deliver. The creator's existing filter remains unchanged.
  Date/Author: 2026-11-13, parent agent.
