# Issue #1302 parent-wait validation

The final real Codex acceptance run passed on 2026-10-16 using `tests/e2e/codex_parent_wait.py`, development CLI/worker binaries, and named instance `issue-1302-waits`. Configuration, data, project and copied login files lived under `/home/jonathan/.cache/mjolnir/build/issue-1302-live-4`. The default instance and real profile homes were not modified.

Parent: `c77b95c94e4d31e669f43a397423e430` (`gpt-6.1-sol`, medium).
Children (`gpt-6-luna`, high): `e4b94117714547a9ed8a7187450bdfd4` (delayed-a), `8b9542abde4b33fd535e55c9ff8198e4` (delayed-b).

The parent spawned two children waiting on externally released gate files. It called `await_children` and ended its turn. For 30 seconds, worker status stayed between turns with the same completed command, no active prompt or harness turn, durable child-wait intent, and `expecting` activity. Restarting the isolated daemon preserved that state and left both children working.

User steering resumed the parent, which wrote the requested acknowledgement and awaited again. Releasing A produced one notification; the parent collected `REPORT_A_1302` and ended another waiting turn for B. Releasing B produced one more notification; the parent collected `REPORT_B_1302`, wrote both tokens, and finished with idle activity and no wait intent.

Exactly four parent turns were persisted: the initial request, steering, and two `subagent-wait-...` notifications. There were no `auto-continue-...` turns. I inspected the actual mj transcript and final marker files. Evidence is in `observations.json`, `parent-turns.json`, `parent-transcript.json`, `cli.jsonl`, and project marker files under that isolated root. No harness-native session files were used.

Cleanup waited for suspension to persist before stopping the daemon. The final store contains the stopped parent; its children were released. The driver exited successfully.

Rust validation: full controller suite (2,006 passed, 12 existing ignores), core suite (502 passed), worker suite (680 passed, 11 existing ignores, one fixture failure corrected and rerun successfully), remaining binary/doc tests, workspace Clippy with warnings denied, formatting, whitespace checking, and Python syntax checking. The sole final-suite failure was the new mailbox fixture leaving ACP readiness false; recording SessionConfigured made it represent an actual waiting parent. Production behavior did not change. The final collector-admission/steering regression and all 24 migration reader regressions also passed.

An earlier real run exposed a parent making a second bounded wait after collecting A. The collection contract now explicitly says one wait returns every ready report and an otherwise idle parent should await_children again with unfinished children. The shared Claude instructions remain within its 2,048-character limit at 2,037 characters. Bounded blocking waits retain their existing timeout behavior.
