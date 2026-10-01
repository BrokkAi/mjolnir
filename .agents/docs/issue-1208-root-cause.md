# Issue #1208: configuration investigation

Investigation date: 2026-10-01. Incident build: 2.24.0+e70d9d9c.

The preserved worker journals contradict the reported harness effort reset.
Both workers accepted effort changes promptly and kept reporting the requested
effort. The failure seen by the benchmark is consistent with delayed Mj session
publication. A deterministic probe of the actual publication channel identifies
session-ID-based starvation; historical consumer timing is not retained, so
attribution of the incident's exact delay to that mechanism remains an inference.

## Preserved incident evidence

Read-only sources: the two workers' initial relay-journal segments, the default
daemon database's `api_events` and `api_config_results`, the daemon log beginning
2026-10-01T11:09:46Z, and benchmark launch artifacts under
`/tmp/bifrost-eval33-20261001`. No session was configured, stopped, or restarted.

All times below are UTC. Journal ordinals refer to each session separately.

| Session | Event | Time | Ordinal | Reported effort |
| --- | --- | --- | --- | --- |
| Claude `7de4635abbab30e3f15810db12124a1f` | Initial options | 12:11:29.597 | 6 | high |
| Claude | First model setter completed | 12:20:03.234 | 13 | high |
| Claude | First effort setter completed | 12:20:08.227 | 19 | xhigh |
| Claude | Repeated model setter options | 12:21:31.107 | 23 | xhigh |
| Claude | Final pre-prompt setter completed | 12:25:17.685 | 43 | xhigh |
| Codex `a4e4f91ab1b14e46e32c478d3cd9b6a3` | Initial options | 12:08:20.816 | 6 | high |
| Codex | First model setter completed | 12:22:33.599 | 13 | high |
| Codex | First effort setter completed | 12:24:20.359 | 25 | max |
| Codex | Repeated model setter options | 12:26:25.874 | 35 | max |
| Codex | Manual effort setter completed | 12:33:43.299 | 61 | max |
| Codex | Final pre-prompt setter completed | 12:34:16.880 | 73 | max |

Every effort request in these histories reached command completion less than
70 ms after admission. Every subsequent options report retained the requested
effort, including repeated model setters. Neither history contains a
`config_option_update` notification between initialization and the first prompt.
There is no recorded reset, delayed harness reread, or overlapping model/effort
execution in either history.

The issue's Claude "launch-config saved at 12:11:29Z" time was its JSON
`updated_at` field, which reflects session metadata rather than file-save time.
The current launch-config files have subsequently been overwritten by successful
relaunches. Their present contents are not evidence of the original failure.
The first configuration commands actually reached the Claude worker at 12:20Z
and the Codex worker at 12:22Z. Codex already advertised initial options 13 seconds
after creation, despite the driver's reported readiness timeout minutes later.

## Mj publication mechanism

`mj sessions --session ID` uses `GET /api/v1/sessions/ID`. In
`mj-controller/src/server/api/sessions.rs`, both detail and list routes obtain
configuration from the asynchronous viewer snapshot. The detail route looks up
the live handle for background work and assessment, but does not refresh config.

In contrast, the setter in `server/api/config.rs` waits for durable completion,
then obtains config via `live_session_config_options`. The handle's explicit sync
publishes its new view before answering (`session_manager/actor.rs`). Thus a
setter can report the correct value while a subsequent detail read reports an
older value. #1091 added live validation and a live setter response; it did not
add a harness reread.

`SessionManagerUpdates::pop_pending` in `session_manager/channels.rs` calls
`BTreeMap::pop_first()`. Repeated updates from a lower session ID can indefinitely
overtake an already waiting higher ID. Coalescing limits memory but does not
provide fairness. This channel is used by the primary manager, continuation
forwarding, and the web manager, so the same hazard exists at multiple stages.
The separate delegation mailbox in `session_manager/delegation.rs` already uses
FIFO delivery of pending session IDs and coalesces their latest values.

## Reproducer and proposed scope

A temporary colocated unit probe uses the real channel, without a daemon or
live data:

1. Publish `a-hot` and `z-configured`.
2. Consume `a-hot`.
3. Publish a new `a-hot` update.
4. Require the next delivery to be `z-configured`.

The probe failed as expected: the actual channel delivered `a-hot` again,
although `z-configured` was already waiting. Repeating the hot publication
can repeat this indefinitely. Command:

```text
cargo test -p brokk-mj-controller --lib issue_1208_dashboard_delivers_waiting_session_before_republished_hot_session -- --nocapture
```

The elevated dev-profile run completed with 0 passed, 1 failed, and 2101
filtered out; the failure was the fairness assertion, not build or environment
failure. The temporary probe was removed afterward. No Rust source changes
remain, so the investigation does not ship a code change.

The smallest correction for prolonged publication lag is fair delivery in the
shared update channel, following the existing delegation mailbox pattern. Keep
queue membership and pending values under one owner/lock, preserve producer
replacement fencing and
upgrade admission ownership, and test hot-producer fairness, latest-value
coalescing, and actor replacement. This requires no harness patch, protocol
revision, config settled flag, or configuration state-machine refactor.
For configuration reads immediately after a successful setter, refresh the
detail route's configuration from the same live handle the setter uses; fair
delivery alone does not make asynchronous dashboard publication synchronous.
The detail route already obtains that handle, and the shared config projection
already exists. These are localized Mj changes.

An immediate driver workaround is to check the JSON returned by a successful
`set-config` for the requested model and effort rather than discard that response
and judge success from `mj sessions`. Report a mismatch instead of assuming
success. This uses an existing live read, but does not correct stale readiness or
general dashboard publication. Historical HTTP responses and queue service
timings were not saved, so that workaround has not been verified against a replay
of the original launch wave.
