# Issue #1302 parent-wait validation

The final real Codex run passed on 2026-10-16 after merging upstream's incremental delegation exchange. The combined build retains feed protocol 38 and uses protocol 39 for explicit child waits; its store revision is 84.

The current opt-in live driver is `tests/e2e/parent_report_notifications.py`. The historical 2026-10-16 parent-wait validation used named instance `issue-1302-waits`, development CLI/worker binaries, and separate configuration, data, project and copied login files under `/home/jonathan/.cache/mjolnir/build/issue-1302-live-5`. The default instance and real profile homes were not modified.

Parent: `e1b538b263d64cc2fb59fe1266c30d2e` (`gpt-6.1-sol`, medium).
Children (`gpt-6-luna`, high): `95ea37960ca45fea0abd75b9d5951e8e` (delayed-a), `ae8d330e20746fc5adac41f5478d1f79` (delayed-b).

The parent spawned two children waiting on externally released gate files. It called `await_children` and ended its turn. For 30 seconds, worker status stayed between turns with the same completed command, no active prompt or harness turn, durable child-wait intent, and `expecting` activity. Restarting the isolated daemon preserved that state and left both children working.

User steering resumed the parent, which wrote the requested acknowledgement and awaited again. Releasing A produced one notification; the parent collected `REPORT_A_1302` and ended another waiting turn for B. Releasing B produced one more notification; the parent collected `REPORT_B_1302`, wrote both tokens, and finished with idle activity and no wait intent.

Exactly four parent turns were persisted: the initial request, steering, and two `subagent-wait-...` notifications. There were no `auto-continue-...` turns. I inspected the actual mj transcript and final marker files. Evidence is in `observations.json`, `parent-turns.json`, `parent-transcript.json`, `cli.jsonl`, and project marker files under that isolated root. No harness-native session files were used.

Cleanup waited for suspension to persist before stopping the daemon. The final store contains the stopped parent; its children were released. The driver exited successfully.

Final merged validation passed: controller 2,007, core 502, worker 685, chat 412 and CLI 212 active library/binary unit tests, plus the full integration and doc-test targets. Existing ignored cases remain ignored. The command was `cargo test -p brokk-mj-core -p brokk-mj-worker -p brokk-mj-controller -p brokk-mj-chat -p brokk-mjolnir --no-fail-fast -- --test-threads=2`. Workspace `cargo clippy --all-targets -- -D warnings`, formatting, whitespace and Python syntax checks passed. The isolated automatic daemon upgrade regression passed with the historical fixture corrected for migration 84.

An earlier real run exposed a parent making a second bounded wait after collecting A. The collection contract now explicitly says one wait returns every ready report and an otherwise idle parent should await_children again with unfinished children. The shared Claude instructions remain within its 2,048-character limit at 2,037 characters. Bounded blocking waits retain their existing timeout behavior.
