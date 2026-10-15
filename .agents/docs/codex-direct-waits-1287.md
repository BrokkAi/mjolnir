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
| Cancellation | `178c451b40dc4f5d22c4ad32187c6717` | Interrupt returned parent to idle in 2.89 seconds. Child kept running. A subsequent prompt collected its report with a direct wait. |

The local evidence directory is `target/issue1287-live-2/`. It contains the
driver, native records, API history, staged Codex configuration, trace and
summary. The driver reuses `SteerLab` from `tests/e2e/steer_live.py` and
destroys its fixture sessions before stopping its isolated daemon.

## Automated validation

The two Codex-profile regression tests passed. They exercise the on-disk
configuration contract, existing namespace preservation, repeated worker
startup, legacy boolean code-mode settings and failure without partial writes.
The MCP initialization contract test passed after replacing its old
`ALL_TOOLS` expectation with direct-call guidance.

The worker library suite had 676 passes and 11 ignores in its final run,
with one timeout in the unchanged
`saturated_wait_lane_keeps_send_message_available` test. It also timed out
serially at its existing two-second admission bound under host load above
300. That same test passed in the earlier suite run. All 23 binary and
integration tests passed; the documentation target contained no tests.
