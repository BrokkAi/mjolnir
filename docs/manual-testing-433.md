# Issue 433 manual testing

This document is a manual validation guide, not an ACP fixture. Use an ACP
adapter installed outside this repository that advertises selectable session
configuration options.

| Area | Before | After |
| --- | --- | --- |
| Session controls | `/mjconfig` settings overlay | `/mjconfig` has separate Agents and Subagents option rows |
| Primary option | Current session-only picker | Saved default plus a best-effort live primary update |
| Subagent option | No independent default | Saved default, used by newly launched workers only |
| Removed adapter option | Value was invisible or lost | `stale; retained` remains visible and is not sent |

Validation strategy:

1. Start `mj` with an adapter exposing more than nine select options and open
   `/mjconfig` before launching a worker. Confirm the detached startup probe
   supplies the configured primary and subagent controls (or clearly leaves a
   failed probe unavailable); the four tabs are Agents, Subagents, ACP
   Servers, and Appearance; every role-specific option is reachable with
   Up/Down and long rows wrap or scroll rather than truncate.
2. Change an Agent option, save, and confirm the visible active value changes
   when the adapter accepts `SetSessionConfigOption`; start `/new` and verify
   the saved value is applied during session setup.
3. Change the matching Subagents option, save, then launch a new subagent.
   Confirm that worker receives the new value while the already-running
   primary does not change.
4. Remove an option/value from the external adapter, reopen `/mjconfig`, and
   confirm its saved value is labelled `stale; retained`; restoring the option
   should make it selectable again.
5. Confirm F1–F9, Ctrl-1–9, and AZERTY number-row variants no longer open a
   session-option picker and no option shortcut row appears under the input.

The external adapter and any failure behavior are intentionally outside this
repository. Do not add a product test fixture solely to simulate ACP failures.
