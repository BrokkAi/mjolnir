# Codex direct MJ waits: #1287 live evidence

Validated on 2026-10-15 with the pinned Codex CLI 0.160.1 and
`@brokkai/codex-acp` 1.13.7. The exact Codex namespace is `mcp__mj_agents`.
Codex's native records store `namespace: "mcp__mj_agents"` separately from
`name: "wait"`.

The worker adds that namespace to
`features.code_mode.direct_only_tool_namespaces` in the session-private
Codex profile, preserving existing namespaces and the code-mode enablement
setting. `omit_tools_from = ["deferred"]` still exposes MJ tools initially.
The setting is documented in the [official configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).

A separate real ACP turn inspected the code-mode executor itself. It returned
`mj_wait_type: "undefined"`, no MJ entries in `ALL_TOOLS`, and 106 other
available tools. This confirms enforced executor exclusion while retaining
independent code-mode tools. Evidence is in `target/issue1287-routing/`.

## Live acceptance

The test used named instance `fix1287-32285f23`, a separate daemon, copied
credentials, isolated profile homes and a fixture repository. Code mode was
explicitly enabled. Real Codex parents spawned real MJ children whose shell
processes waited on files controlled by the driver. Source profile project
listings were checked before and after the run.

After each parent entered its blocking wait, the driver compared its native
response items and token-usage events throughout the pending interval. No
additional parent model activity appeared. Every MJ wait was a direct tool
call; no parent called `functions.exec` or `functions.wait`.

| Case | Parent session | Observation |
| --- | --- | --- |
| Slow report | `b4f323cef9a6bcb698dc2d1bd167ed1f` | 110.95 seconds without additional model activity, then successful child-report delivery. Parent tools: spawn, wait. |
| Deadline renewal | `3ae560935362867610242807d9967df8` | First wait returned `still_running` with `waited_seconds: 240`. Parent renewed with another direct wait and collected the report. No activity while either call remained pending. |
| Initial cancellation | `178c451b40dc4f5d22c4ad32187c6717` | Interrupt returned parent to idle in 2.89 seconds and the child kept running. Actual tool output later revealed that the cancelled wait consumed the report; this was not a successful report-delivery check. |

The local evidence directory is `target/issue1287-live-2/`. It contains the
driver, native records, API history, staged Codex configuration, trace and
summary. The driver reuses `SteerLab` from `tests/e2e/steer_live.py` and
destroys its fixture sessions before stopping its isolated daemon.
The test-local Podman pause process and the isolated runtime were also removed;
source profile project listings were unchanged.

## Cancellation ownership follow-up

The first driver accepted the parent's echo of a known marker. Reviewing the
actual native payload showed an aborted first wait followed by
`nothing_to_wait_for` with null output. The parent repeated the marker from
its prompt; no handback payload reached the second wait.

The correction ties delivery to the worker relay's originating turn, rather
than socket liveness. Completion and interruption take the relay owner before
the queue lock. Cancellation retains its existing durable target until that
prompt ends; an acknowledgement cannot reopen delivery or retarget a newer
turn. Queued replacement work also waits for old cancellation controls to
drain. The daemon serializes each parent's waits through delivery receipt and
report-marker restoration, including failed delivery and task recovery. Other
parents, spawns and messages remain concurrent. No database, journal or wire
field is added.

The stronger live follow-up must observe a false receipt for the retired wait
and an actual `reported` payload containing the child's `handback` in a fresh
parent turn. This follow-up is still pending while its test binaries build.

## Initial automated validation

The two Codex-profile regression tests passed. They exercise the on-disk
configuration contract, existing namespace preservation, repeated worker
startup, legacy boolean code-mode settings and failure without partial writes.
The MCP initialization contract test passed after replacing its old
`ALL_TOOLS` expectation with direct-call guidance.

After merging upstream release 2.46.0, the complete worker suite passed:
`cargo test -p brokk-mj-worker -- --test-threads=4` reported 677 library
passes, 11 ignores, and 23 binary/integration passes. The documentation target
contained no tests. Workspace `cargo clippy --all-targets -- -D warnings`
also passed in the dev profile.

Those checks precede the cancellation ownership correction. Its final core and
worker suites passed 1,205 tests with 12 ignored. All seven final dispatcher
regressions and workspace Clippy passed. The full controller suite is running;
one checkpoint subprocess needs an isolated rerun after the suite completes. Logs are under
`/home/jonathan/.cache/mjolnir/build/issue1287/`.

An earlier overloaded run timed out in the unchanged
`saturated_wait_lane_keeps_send_message_available` test at its two-second
admission bound. It passed in the final complete suite with reduced test
concurrency. All Cargo tests ran outside the restricted sandbox.
