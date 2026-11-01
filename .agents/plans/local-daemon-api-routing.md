# Route local API requests to the daemon's actual listener

This ExecPlan follows `.agents/PLANS.md`. Keep its progress, discoveries,
decisions, and outcome current throughout implementation.

## Purpose / Big Picture

The Usage by session dialog and local API commands must work when the web
viewer's advertised hostname cannot resolve. The daemon already runs on the
same host, and its control connection works independently of that hostname.
After this change, the daemon publishes a connectable address for its actual
HTTP listener. Local clients connect there while preserving the advertised
HTTPS hostname and certificate checks.

## Progress

- [x] (2026-11-01) Diagnose the DNS failure and confirm the HTTPS listener answers locally with certificate verification preserved.
- [x] (2026-11-01) Claim issue #1440 and settle listener ownership and client routing design.
- [x] (2026-11-01) Publish the actual local listener address; targeted recovery and wire/port tests passed.
- [x] (2026-11-01) Route local API requests through the published address; API transport tests (13) and viewer readiness tests (4) pass.
- [x] (2026-11-01) Complete integrated parent and independent review, including the corrected IPv6 parsing path.
- [x] (2026-11-01) Complete full touched-crate suites, workspace Clippy, and formatting checks.
- [x] (2026-11-01) Accept the integrated fix and prepare the validated changes for commit and push to origin/master.

## Surprises & Discoveries

The normal terminal UI uses a local TCP control connection. Its usage dialog
creates an independent HTTP API client, which currently uses the advertised
web-viewer URL. That URL may contain a Tailscale hostname. Therefore a DNS
failure can break this dialog while the rest of the UI works.

The control protocol currently carries the advertised URL but no HTTP listener
socket. The control socket in daemon metadata is a different listener and must
not be reused for HTTP. `web_viewer::serve_inner` already owns the actual HTTP
socket after binding, including an operating-system-assigned port and listener
recovery. It is the authoritative place to publish local connection details.

Reqwest's DNS override preserves the request URL and HTTPS server name, but
the URL's explicit port takes precedence over the override's port. The client
must use one consistent readiness response for the URL, socket, and certificate.

Independent review found that `Url::host_str()` includes brackets around IPv6
addresses, which defeats parsing it directly as `IpAddr`. Use typed `url::Host`
variants to distinguish IP addresses and DNS authorities. Add the existing
workspace `url` dependency to the CLI rather than parsing brackets manually.

## Decision Log

Publish an optional `local_address: SocketAddr` in `WebViewerAccess::Ready`,
using serde defaults for decoding older JSON and omitting absent values.
Normalize wildcard listener addresses to loopback in the daemon after binding.
This keeps knowledge of listener binding with its owner instead of teaching
clients the current Tailscale IPv4 binding rule. Advance the local control
protocol from 69 to 70; frozen management replies remain unchanged so daemon
upgrade coordination continues to work.

Preserve the advertised DNS hostname for HTTP Host and HTTPS certificate
verification, and direct the socket connection to the published local address.
Disable proxies and redirects for local daemon requests. Retain the existing
certificate pin behavior for operator-configured certificates and normal
certificate verification for publicly trusted certificates. Do not add DNS
fallbacks or weaken TLS when the published address is missing or inconsistent.

## Outcomes & Retrospective

The implementation is complete and integrated review found no remaining
issues. The existing real-listener port recovery test now binds IPv4 wildcard,
recovers to an assigned port, and successfully connects through the published
loopback address. CLI tests cover an unresolvable HTTPS authority with an
authenticated request and preserved Host, refusal of a wrong certificate,
proxy bypass, redirects, and actual IPv6 transport. Readiness now reads one
bounded access publication for both desktop and API callers.

Final dev-profile validation passed: the three touched-crate suites produced
2,530 passing test results (including isolated child-test executions), zero
failures, and 22 ignored results. Workspace Clippy with warnings denied and
workspace formatting checks passed. No user configuration, live daemon,
database, or installation was changed. The accepted changes are ready for the
required commit and push; deployment is outside this implementation task.

## Context and Orientation

`mj-cli/src/api_client.rs` owns the typed HTTP client used by CLI commands and
the usage dialog in `mj-cli/src/dashboard/usage_report.rs`. Its `connect_to`
currently obtains a URL through `daemon::wait_for_web_viewer`, then separately
obtains certificate details through `DaemonClient::web_access`.

`mj-client/src/web.rs` defines `WebViewerAccess`, the readiness response shared
by the controller and clients. `mj-client/src/daemon.rs` defines the control
protocol revision and JSON framing. `mj-controller/src/server_runtime/run.rs`
creates provisional viewer access details. `mj-controller/src/web_viewer.rs`
binds the real listener and publishes the final readiness response using
`ready_at`. A socket address is a numeric IP address plus port; a wildcard IP
accepts connections on all interfaces and is converted to the matching
loopback IP for connections from this host.

## Plan of Work

First extend readiness publication with the actual listener address, retaining
specific configured addresses and the actual bound port. Extend existing
listener-recovery coverage to verify the published socket can be used after
port assignment and recovery. Update provisional constructors and advance the
local control protocol without changing the database or frozen management
messages.

In parallel, update `ApiClient::connect_to` to obtain all connection details
from the same ready response. Reuse bounded readiness handling for a listener
that is still starting. Configure reqwest to use the local socket while keeping
the advertised HTTPS authority. Handle numeric wildcard URL authorities using
the daemon's connectable address; verify URL and socket ports agree. Extend
existing real HTTP/TLS fixtures to prove unresolved hostname routing,
authentication, certificate rejection, and redirect/proxy behavior.

Finally review both slices together, have an independent reviewer seek contrary
evidence, run final required checks once, and commit and push the fix. Children
own disjoint source files; the parent owns this plan, integrated review, and
delivery.

## Concrete Steps

Run commands from the repository root using the existing mbx wrapper. Never
redirect Cargo target output or change Cargo/mbx configuration. Intermediate
checks should use the affected package and test filter, for example:

    mbx test -p brokk-mjolnir api_client::tests::

Every Cargo test invocation runs outside the restricted sandbox. After the
last code change, run full suites of all touched crates and workspace Clippy:

    mbx test -p brokk-mjolnir -p brokk-mj-client -p brokk-mj-controller
    mbx clippy --all-targets -- -D warnings

Confirm exact package names from the crate manifests before invoking checks.
Any real daemon or CLI regression uses its own named `--instance`; existing
test isolation must remain intact. Never exercise a new build against the live
default instance. Stage only this task's files, commit on the current branch,
and push `HEAD:master` to origin. If upstream advances, fetch and merge it,
then push the merge; do not rebase or switch branches.

## Validation and Acceptance

A real HTTPS fixture advertised with an unresolvable hostname must accept an
unauthenticated API probe and an authenticated request over its local socket.
The request must retain the advertised Host authority. A wrong certificate
must still be rejected. Local traffic must bypass proxy routing and refuse
redirects. Listener readiness must publish the current socket after ephemeral
port assignment or port-conflict recovery. Existing isolated daemon startup and
handoff tests must continue to pass across the protocol revision change.

Review formatting and the final diff. Reuse successful targeted checks; rerun
only failed checks when correcting a failure. Record final validation evidence
here before committing.

## Idempotence and Recovery

Tests use isolated listeners and existing isolated daemon fixtures. Repeating
them does not touch the live store or change user configuration. This change
has no migration and no installation step. If implementation uncovers another
listener state, update this plan and its single-owner design before adding a
new path. Stop owning test processes before removing their temporary files.

## Artifacts and Notes

Issue: https://github.com/BrokkAi/mjolnir/issues/1440.
Child reports are retained under `.mj/agents/7ca9cb4107c0cdc8253a6276e388dc65/`
and `.mj/agents/79131afc0356f995cfb652579288a947/` for this session.
The first report's `validation/` directory holds `touched-crates-test.log`,
`workspace-clippy.log`, and `format-check.log`; all checks passed. Independent
review report `9dd01285767689f20001884f8498dae6` found no remaining issue after
the typed IPv6 host correction.

## Interfaces and Dependencies

Use existing serde, reqwest, rustls, axum-server, and rcgen dependencies. Do not
add a new crate. Add only the optional local socket to `WebViewerAccess::Ready`;
the separately exposed status and management response structures stay stable.
The socket publication occurs after successful listener bind and before the
Ready state is visible. The client uses that same response for its URL, socket,
and certificate identity.

Initial plan written 2026-11-01 after tracing both transports and assigning the
listener publication and CLI transport slices.

2026-11-01: Confirmed the package manifests and corrected the final validation
command to use `brokk-mj-client` rather than its Rust library name.

2026-11-01: Recorded completed listener publication checks and the independent
review's IPv6 parsing finding; the CLI implementation uses typed URL hosts.

2026-11-01: Corrected wrapper syntax from the initial investigation: mbx forwards
Cargo subcommands directly (`mbx test` and `mbx clippy`). Targeted controller
checks confirmed that syntax; `mbx cargo` is not a valid subcommand.

2026-11-01: Recorded completed API transport/readiness checks and clean
independent review. Final validation is assigned with no further edits planned.

2026-11-01: Recorded final passing touched-crate suites, Clippy, formatting,
and accepted implementation outcome. No required implementation work remains.
