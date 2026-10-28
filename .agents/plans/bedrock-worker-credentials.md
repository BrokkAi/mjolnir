# Provision AWS credentials for isolated Bedrock workers


This ExecPlan follows `.agents/PLANS.md` and must stay current as implementation proceeds.

## Purpose / Big Picture


Bedrock profiles currently work on the controller host but can enter endless timeout retries in isolated workers because selecting an AWS profile does not transport its credentials. Provision the selected host AWS chain into private worker storage before launching Claude or Codex, refresh temporary credentials through existing background reconciliation, and report source authentication failures during preparation. Existing workers may require restart or manual repair, as explicitly authorized by the user.

## Progress


- [x] (2026-10-28) Diagnose missing AWS credential sources in isolated workers and claim #1385.
- [x] (2026-10-28) Implement AWS process-format export validation, private launch staging, relay protocol 46 and background renewal.
- [x] (2026-10-28) Add regressions for staging, renewal, private transport, mixed local/remote profiles and missing credentials. Real AWS CLI resolution passed.
- [x] (2026-10-28) Validate controller suite (2,182 passed plus the corrected stale assertion passing separately), core suite (551 passed), worker suite (718 passed) and all included integration tests; workspace clippy is clean.
- [x] (2026-10-28) Commit the validated test correction (70fbe84c8) and AWS implementation, and publish them to origin/master.

## Surprises & Discoveries


The native harness reports missing remote credentials as repeated 60-second timeouts. The existing credential coordinator deliberately skips native login exchange for AWS profiles and therefore never supplies AWS credentials. The host AWS CLI supports `configure export-credentials --format process`, which resolves static keys, SSO and assumed roles without copying unrelated profiles or helper executables.

## Decision Log


The full controller suite found an existing stale assertion in `controller/resume/tests.rs`: commit 73ba8f905 changed the human notice to a child count, but this integration test still demanded child IDs and states in that notice. Remove those two notice assertions and retain both store-to-model context freshness assertions; the core notice regression already protects the count-only behavior. Validate this correction with only the failed test, and continue the remaining core and worker suites without repeating the passing controller suite.

Use AWS CLI resolution on the controller and a session-private credential_process file on the worker. Keep explicit target credential sources and EC2 instance roles authoritative. Add connection-only AWS credential state/install requests to the existing relay and refresh through its credential coordinator. Secrets must never enter journals or error text, including the subprocess disk-full observer. Existing workers need no live migration. The operator explicitly approved copying the selected default AWS credentials into the two affected containers. Both private installs succeeded. Both sessions resumed tool calls after their next prompt/restart, confirming live recovery.

## Outcomes & Retrospective


Provisioning and renewal are implemented. Focused AWS tests passed, including real CLI resolution and mixed local/isolated synchronization. Both live containers received the approved private credential repair. Focused controller tests (13), the worker private-transport regression and workspace clippy passed. The controller suite passed 2,182 tests and exposed only the stale resume assertion described below; that corrected assertion passed its targeted rerun. The core suite passed 551 tests, the worker suite passed 718, and all included integration tests passed. Workspace clippy is clean. Implementation, validation and live repair are complete; the final implementation commit publishes this plan together with the fix.

## Context and Orientation


`mj-controller/src/controller/worker_binary/launch.rs` prepares isolated profiles. `mj-controller/src/pollers/worker_targets.rs` publishes credential reconciliation targets; `mj-controller/src/worker_client/credential_sync.rs` runs that work in background tasks over actor-owned worker connections. `mj-core/src/relay/protocol.rs` defines connection requests; `mj-worker/src/worker_runtime/unix/requests.rs` handles secret installation outside the durable journal. Shared AWS export validation and process-provider configuration belong in `mj-core/src/aws_credentials.rs`; host chain selection and execution belong in `mj-controller/src/aws_credentials.rs`.

## Plan of Work


First implement and test the selected AWS export format and source selection. Then stage the exported credentials with the profile and point the isolated launch at its private file. Extend background credential reconciliation to re-export and push changed credentials using new version-gated relay requests. Keep all CLI execution bounded and off event loops. Add tests that run real AWS CLI resolution against isolated static credential files where available, plus isolated relay tests that prove renewal and secret exclusion from journals.

## Milestones


First establish the AWS format and launch contract: the controller resolves the selected identity with AWS CLI v2, stages owner-only process-format JSON and a native credential_process configuration, and isolated launches point at those files. The real CLI regression must resolve both initial and renewed fixture credentials, including paths containing quotes and Unicode, while missing profiles fail before harness startup.

Second connect renewal to the existing background coordinator and actor-owned relay connection. Protocol 46 adds AwsCredentialsState and InstallAwsCredentials; the worker writes their payload atomically without recording an event. The mixed local/isolated regression must push only to isolated sessions, and the worker transport regression must install and renew more than 64 KB without changing the journal.

Finally run complete dev-profile suites and workspace clippy, review the diff, commit on the current branch and push to origin/master. The user approved a separate manual repair of the two live containers; both resumed tool calls after that repair. No new build has been installed as part of this plan.

## Concrete Steps


Use ordinary mbx-managed Cargo commands from the repository root; do not change Cargo storage. Run focused tests during development, then full suites for touched crates and `cargo clippy --all-targets -- -D warnings` once at completion. Every `cargo test` runs elevated and any executable test uses isolated data directories or a named instance. From the repository root, the focused commands are `cargo test -p brokk-mj-controller aws_ -- --include-ignored` and `cargo test -p brokk-mj-worker credential_exchange_stays_on_the_connection_and_out_of_relay_state`. The final commands are `cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-worker -- --test-threads=8` and `cargo clippy --all-targets -- -D warnings`. Expect no failures; the real AWS CLI case is deliberately ignored in ordinary suites because it requires AWS CLI v2, and was explicitly included in focused validation.

## Validation and Acceptance


An isolated Bedrock launch receives only the selected credentials, startup fails clearly when their source is unavailable, explicit target sources retain their settings, and a changed temporary credential export reaches the worker atomically through private transport. No credential payload appears in the relay journal. Existing non-AWS sync tests and isolated upgrade regressions must keep passing.

## Idempotence and Recovery


Credential writes use atomic private files and synchronization compares fingerprints before pushing. A failed refresh reports an error while retaining the prior file. Launch failure does not start a harness with missing credentials. Live session recovery is a separate restart or manual repair after the code fix.

## Artifacts and Notes


Issue: https://github.com/BrokkAi/mjolnir/issues/1385. AWS export format reference: https://docs.aws.amazon.com/cli/latest/reference/configure/export-credentials.html.

## Interfaces and Dependencies


Reuse the AWS CLI, existing bounded subprocess executors, atomic private file writes, relay credential snapshots and background sync admission. Add no SDK or workspace crate. Advance the relay version for the additive AWS requests; old workers remain readable and receive a clear restart requirement for AWS synchronization.

Update (2026-10-28): Implemented provisioning and refresh. The native process provider preserves expiration semantics; a real AWS CLI regression passed with absolute and SSH-relative paths containing spaces, Unicode and quotes. Full validation and publication remain.

Validation update (2026-10-28): All required validation is complete. The AWS CLI test also passed when explicitly included; ordinary full suites keep that tool-dependent case ignored. Both repaired live sessions resumed tool calls.

Outcome (2026-10-28): Both affected live sessions recovered after the approved manual credential repair. The permanent source fix provisions isolated workers and refreshes temporary credentials; it takes effect when the updated build is installed. All relevant full suites and workspace clippy passed, with the pre-existing stale resume assertion corrected and revalidated independently.
