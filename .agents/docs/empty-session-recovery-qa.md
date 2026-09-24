# Empty native-session recovery QA

Issue #1063 concerns a Codex or Claude session opened without a prompt. The
adapter may not have written a native transcript yet, so a later native reload
can report that session missing. Mjolnir must keep its own session and queued
work, warn about the replacement, and refuse replacement once native history
may have been used.

The deterministic local proof uses the real `mj-worker` executable with a
scripted ACP adapter and a temporary worker root. Run these from the repository
root with normal Cargo build storage and outside the restricted sandbox, since
the tests use Unix sockets:

    cargo test -p brokk-mj-worker --test empty_session_recovery
    cargo test -p brokk-mj-worker --test worker_environment checkpoint_worker_remains_visible_and_stoppable_after_clean_reexec

The first test seeds an unused native ID and a queued prompt in a disposable
durable relay, launches a new worker process, and has the adapter answer its
native `session/load` with the Codex or Claude missing-session error. It checks
one replacement `session/new`, one delivered prompt, the replacement warning,
and command completion under the original Mjolnir ID. The second checks
`MJ_INSTANCE` in the exact worker PID's `/proc/<pid>/environ` after clean
re-execution, including a conflicting value in the launcher environment.
Existing `mj-worker/src/acp/tests.rs` coverage checks that used native history
is refused instead of replaced. These tests use no live provider account.

For an interactive provider check, use a disposable named instance with
isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR`, a temporary Git repository, and a
test profile. Open a session and send no prompt. Record its Mjolnir session ID
and native session ID with `mj sessions --session <id>`. Select **Restart
session** from the terminal command palette, or run `mj suspend --session
<id>` followed by `mj resume --session <id>` and `mj wait --session <id>`.
Do not use `mj daemon restart`: it leaves the worker attached. Check for a
native reload attempt and, only if the adapter reports the unused native ID
missing, the warning and a new native ID under the original Mjolnir ID. A
provider that persists empty native sessions may reload successfully instead;
that is also valid. Stop only the fixture-owned session through Mjolnir's
normal lifecycle commands. On Linux, use the worker PID recorded in its
worker root and inspect `/proc/<pid>/environ` for the named `MJ_INSTANCE`;
never select a process by name alone. A worker launched before instance
stamping needs a supported session restart to acquire the marker.
