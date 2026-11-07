# Scoped, short-lived AWS credentials for sessions, and IMDSv2 on Mjolnir-launched EC2

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds.

This plan must be maintained in accordance with `.agents/PLANS.md` at the repository root. It addresses GitHub issue #1424, "Security: full AWS profile credentials are synced to every worker and EC2 IMDS is never constrained". It is a design only: no implementation has started.

## Purpose / Big Picture

Today, a session that runs a Bedrock-backed harness (a Claude Code or Codex profile that talks to Amazon Bedrock through the AWS credential chain) on a container or SSH target receives a full copy of the controller host's AWS credentials: access key id, secret access key, and session token for whatever AWS identity the host's selected profile resolves to. Malicious code that runs inside the session can read those credentials and act with every permission that identity has. Mjolnir does not bound that authority. Separately, when Mjolnir launches an EC2 instance for an `aws-ec2` machine, it passes nothing about the instance metadata service (IMDS), so whether code on the instance can read the instance profile's credentials with a plain HTTP GET (IMDSv1) depends on whatever the operator's launch template happens to say.

After this change, a person can configure, per harness profile, an IAM role and an optional narrowing policy. Mjolnir then asks the AWS Security Token Service (STS) for credentials that belong to that role, narrowed by the policy, valid for a bounded time, and tagged with the Mjolnir session id so that AWS CloudTrail (AWS's audit log) shows which session made each call. Those scoped credentials, not the host's own, travel to the session worker and are refreshed before they expire. The host's own credentials never leave the controller host for that profile. Where no role is configured, behavior is exactly what it is today, and `mj doctor` warns that sessions receive unscoped credentials. Separately, every `aws ec2 run-instances` call that Mjolnir makes will require IMDSv2 (the token-based metadata protocol that plain GETs, such as those from a server-side request forgery bug, cannot use), Mjolnir will verify the launched instance reports that setting, and `mj doctor` will flag a launch template that allows IMDSv1.

How to see it working: configure `[profiles.<id>.aws]` with a role ARN, start a container session on that profile, and run `aws sts get-caller-identity` inside the session. The output names `assumed-role/<role>/mj-<session id>` rather than the host's identity. Run `mj doctor` and see the profile's scope check pass (it performs a real, throwaway AssumeRole). Remove the `[profiles.<id>.aws]` table, run `mj doctor`, and see a warning that the profile's sessions receive unscoped credentials. For EC2, launch a session on an `aws-ec2` machine and run `aws ec2 describe-instances --instance-ids <id> --query 'Reservations[0].Instances[0].MetadataOptions.HttpTokens'`; the answer is `required`.

## Progress

- [x] (2026-11-06 00:00Z) Read issue #1424 and researched the code paths named in this plan.
- [x] (2026-11-06 00:00Z) Wrote this ExecPlan (design only).
- [x] (2026-11-06 Z) Parent reviewed and accepted the plan and all five open decisions (see Decision Log, 2026-11-06 acceptance entries).
- [x] (2026-11-07 03:58Z) Milestone 0: live AWS prototype done against the user's account with the limits the user set; all resources created were deleted and verified gone (see Artifacts and Notes, "Milestone 0 resource log"). Fixtures recorded under `mj-controller/tests/fixtures/`. Findings are in Surprises & Discoveries.
- [x] (2026-11-07 Z) Milestone 1: IMDSv2 on Mjolnir-launched EC2 (commit e201e33ee): `with_required_imds` and `require_imdsv2` in `mj-controller/src/targets/provision.rs`, verification in `backend.rs` and the Move preparation, doctor template check.
- [x] (2026-11-07 Z) Milestone 2: `[profiles.<id>.aws]` (`AwsRoleConfig`), `AwsCredentialSource.role`, and the shared `receives_synced_credentials` predicate (commit 0500d3881). Settings form deferred (see Decision Log).
- [x] (2026-11-07 Z) Milestone 3: `mj-controller/src/aws_assume_role.rs` with the per-session cache, launch-time `obtain` (commit 0f5ce42b3).
- [x] (2026-11-07 Z) Milestone 4: credential sync mints per session inside the admission-gated step (commit f58962e4d).
- [x] (2026-11-07 Z) Milestone 5: doctor scope check with a real throwaway AssumeRole.
- [x] (2026-11-07 Z) Milestone 6: documentation (`docs/AWS.md`, `security.md`, `profiles.md`), workspace clippy, touched-crate test suites (results under Outcomes & Retrospective).

## Surprises & Discoveries

- Observation: Only Bedrock-style profiles on isolated targets receive synced AWS credentials at all, and the decision is made in exactly one function.
  Evidence: `mj-controller/src/aws_credentials.rs`, function `source`, returns `None` unless `profile.auth_scheme() == AuthScheme::AwsCredentialChain`, and also returns `None` for `TargetLocator::LocalBare` and `TargetLocator::AwsEc2` backends and for any target whose own environment already carries AWS credential variables or `AWS_PROFILE`. So a per-target "scope" setting has nothing to attach to: a target that owns its credentials already opts out of the sync, and the only profiles that reach the sync are Bedrock profiles. The scope therefore belongs on the profile.

- Observation: The credential sync already carries expiring credentials and renews them; nothing in the wire format needs to change.
  Evidence: `mj-core/src/aws_credentials.rs`, struct `AwsCredentials`, has `session_token` and `expiration` fields in the AWS CLI "process" credential format (`{"Version":1,"AccessKeyId":...,"SecretAccessKey":...,"SessionToken":...,"Expiration":...}`); `validate` rejects an expired export; `state` reports a stored but expired file as absent, which makes the next reconciliation replace it; `RelayRequest::InstallAwsCredentials { data }` (relay protocol 46, `RELAY_AWS_CREDENTIALS_PROTOCOL`) carries the bytes. The temporary credentials STS returns have exactly these fields.

- Observation: The current export is computed once per profile per sync pass and shared by every session of that profile; scoped credentials must be per session.
  Evidence: `mj-controller/src/worker_client/credential_sync.rs`, `reconcile_profile_guarded`, picks the first target's `aws_credentials` source, runs `crate::aws_credentials::export` once, and passes the same result to every `reconcile_session`. The session id and CloudTrail attribution require a per-session mint, which moves the AWS step inside the per-target loop for role-configured profiles.

- Observation: Local bare sessions and EC2 sessions never use this sync, so scoping cannot cover them, and the plan must say so rather than imply otherwise.
  Evidence: the `LocalBare`/`AwsEc2` exclusion in `source` above. A local bare worker runs as the controller's user and can read `~/.aws` directly; an EC2 session uses its instance profile through IMDS.

- Observation: The EC2 `run-instances` response already carries the instance's effective metadata options, so Mjolnir can verify the setting it asked for instead of trusting it.
  Evidence: `mj-controller/src/controller/backend.rs` (~L1294) parses the `run-instances` JSON for `/Instances/0/InstanceId`; the same document has `/Instances/0/MetadataOptions/HttpTokens` (to be confirmed with a recorded real response in Milestone 0).

- Observation: A Move to a new EC2 destination persists its `run-instances` argument vector in the database before running it, so a Move prepared by an older daemon will not carry the new flag.
  Evidence: `mj-controller/src/controller/move_session/destination.rs` (~L103-L118) stores `launch_args: command.args` in `PreparedDestinationState::LaunchPending`, and executes those stored arguments later.

- Observation (Milestone 0): the AWS CLI v2 error shape is `aws: [ERROR]: An error occurred (<Code>) when calling the AssumeRole operation: <message>`, with the literal prefix `aws: [ERROR]: `. A client-side range error has a different shape and no "when calling" clause.
  Evidence: fixtures `aws-sts-assume-role-access-denied.txt`, `aws-sts-assume-role-validation-error.txt`; `--duration-seconds 100` gave `aws: [ERROR]: An error occurred (ParamValidation): Parameter validation failed:` then `Invalid value for parameter DurationSeconds, value: 100, valid min value: 900`. So the plan's error-line parser must accept the `aws: [ERROR]: ` prefix and may match on `An error occurred (<Code>)` alone; for `ParamValidation` the next line carries the useful text. A 12-hour request against a one-hour role gave `ValidationError ... The requested DurationSeconds exceeds the MaxSessionDuration set for this role.` (no maximum is named, which confirms the decision not to guess one).

- Observation (Milestone 0): `aws sts assume-role` output and the `--policy-arns arn=<arn>` syntax work as planned; omitting `--duration-seconds` gives a one-hour credential; the session policy narrows the role.
  Evidence: fixture `aws-sts-assume-role.json` (keys `Credentials.{AccessKeyId,SecretAccessKey,SessionToken,Expiration}`, `AssumedRoleUser.{AssumedRoleId,Arn}`, `PackedPolicySize`, `SourceIdentity`; `Expiration` looks like `2026-11-07T04:03:09+00:00`, accepted by `chrono::DateTime::parse_from_rfc3339`). A call without policy, session name `mj-<32 hex>`, no duration returned `Expiration` exactly one hour after the call. The assumed ARN is `arn:aws:sts::<account>:assumed-role/<role>/mj-<session id>`. With the role allowing `bedrock:List*` and a session policy of only `bedrock:InvokeModel`, `aws bedrock list-foundation-models` returned `AccessDeniedException`, while the same call with a session policy that included `bedrock:ListFoundationModels` succeeded: the session policy narrows, as designed. `sts:SetSourceIdentity` in the trust policy made `--source-identity` succeed.

- Observation (Milestone 0): the `--metadata-options` override over a launch template could NOT be proven. The user's existing launch template (`hel-runson`, us-east-1) already sets `HttpTokens=required` and `HttpPutResponseHopLimit=1`, which is also the EC2 default, so passing `--metadata-options HttpTokens=required` produced the same result with or without the override, and the hop-limit reset question is likewise indistinguishable (template 1, default 1). Creating a template with `optional` was outside the authorized resources.
  Evidence: `describe-launch-template-versions` for `hel-runson` `$Default` shows `MetadataOptions` `{HttpTokens: required, HttpPutResponseHopLimit: 1, HttpEndpoint: enabled}`; the `run-instances` response and a later `describe-instances` both show `{"State":"pending","HttpTokens":"required","HttpPutResponseHopLimit":1,"HttpEndpoint":"enabled","HttpProtocolIpv6":"disabled","InstanceMetadataTags":"disabled"}` (fixture `aws-run-instances-imdsv2.json`). The response-verification step in Milestone 1 is therefore the guard, as the plan intended; Milestone 1 should also keep the plan's note that the precedence is unproven for a template that sets `optional`.

- Observation (Milestone 0): `aws ec2 run-instances` with a launch template accepts no `--min-count`/`--max-count` in AWS CLI 2.36.24 (`--count` only), so the Move path in `mj-controller/src/controller/move_session/destination.rs` (~L107-L112), which appends `--min-count 1 --max-count 1`, is rejected by this CLI.
  Evidence: `aws: [ERROR]: Unknown options: --min-count, --max-count, 1, 1` for a run with the same flags as the Move path; the same command without them launched one instance. This predates #1424 and is outside this plan's scope; it is reported to the parent as a separate defect (the Move EC2 launch cannot work with this CLI version). Milestone 1 must not depend on that argument pair.

- Observation (Milestone 0): through isolated `mj` sessions, both Bedrock harnesses complete a turn when the scoped credentials allow only `bedrock:InvokeModel` and `bedrock:InvokeModelWithResponseStream` (Claude Code) or only `bedrock:InvokeModel` (Codex).
  Evidence: the lab used a local bare target, profile environment `AWS_PROFILE=mj-m0` whose `credential_process` assumed the test role with a chosen session policy, in an isolated `--instance`. Claude (`CLAUDE_CODE_USE_BEDROCK=1`, `us.anthropic.claude-haiku-4-5-20251001-v1:0`): six-action policy completed; Invoke+InvokeWithResponseStream completed; `InvokeModel` alone failed with `ACP auth_required` (so the scoped credentials were in use and streaming is required). Codex (`amazon-bedrock-runtime`, `global.openai.gpt-6-luna`): six-action policy, Invoke+Stream, and `InvokeModel` alone all completed; a policy allowing only `ListFoundationModels` failed with `ACP auth_required` after 14 s. `mj models --profile` listed models for both profiles with the invoke-only policy, so discovery needs no List action in this build. No Bedrock action beyond these was needed, so the provisional default does not need `bedrock:*`.

## Decision Log

- Decision: Put the scope on the harness profile, as an optional table `[profiles.<id>.aws]`, and nowhere else. No per-target or per-machine scope.
  Rationale: The sync is selected by profile and backend kind (see the first discovery), and a target that carries its own AWS credentials opts out of the sync entirely, so a per-target setting would have no behavior to control. The profile already owns the AWS identity selection (`AWS_PROFILE` in `[profiles.<id>.environment]`, or the Codex provider's `aws.profile`), so the role that narrows that identity sits next to it. This extends the existing profile configuration instead of adding a parallel mechanism (AGENTS.md "one mechanism"). The `aws_profile` key on `[machines.<name>]` is a different thing (the identity that launches EC2 instances) and is unchanged.
  Date/Author: 2026-11-06 / Claude (plan author), pending parent review.

- Decision: Table fields are `role_arn` (required), `session_policy` (optional inline policy, a JSON string), `session_policy_arns` (optional list of managed policy ARNs), `duration_seconds` (optional), and `source_identity` (optional boolean, default false).
  Rationale: These map one-to-one to the `aws sts assume-role` options `--role-arn`, `--policy`, `--policy-arns`, `--duration-seconds`, and `--source-identity`, so a reader who knows STS can predict the behavior. The names avoid the bare word `policy` because the role itself has policies and the table narrows them.
  Date/Author: 2026-11-06 / Claude.

- Decision: When `role_arn` is set and neither `session_policy` nor `session_policy_arns` is given, Mjolnir passes a built-in Bedrock invocation policy as the session policy. If either is given, Mjolnir passes exactly what the person configured and nothing built in.
  Rationale: Every profile that reaches the sync is a Bedrock profile, so "a sensible scope for a Bedrock profile" has one answer: permission to invoke and list Bedrock models and nothing else. A session policy can only remove permissions from the role, never add them, so a too-narrow default cannot widen anything; at worst a harness gets `AccessDenied`, visibly. The list is provisional until Milestone 0 confirms it against both harnesses (see Interfaces for the text). Fallback if Milestone 0 finds more actions in use: widen to `bedrock:*`, still limited to the Bedrock service.
  Date/Author: 2026-11-06 / Claude. Open for parent: confirm the default-policy approach (alternative: pass no policy and rely on the role alone).

- Decision: Always set the STS role session name to `mj-<session id>`; set `SourceIdentity` to the same value only when the profile says `source_identity = true`.
  Rationale: The role session name appears in the assumed-role ARN in every CloudTrail event (`arn:aws:sts::<account>:assumed-role/<role>/mj-<session id>`) and needs no trust-policy change, so attribution works for everyone. `SourceIdentity` is a stronger, immutable attribute that survives role chaining, but STS refuses the call unless the role's trust policy allows `sts:SetSourceIdentity`, so making it the default would break first-time setups. Making it opt-in keeps the default working and gives the person who needs it a one-line switch. A session id is 32 lowercase hex characters (`mj-core/src/state.rs`, `new_session_id`), so `mj-<id>` is 35 characters and fits STS's 64-character limit and character set for both fields.
  Date/Author: 2026-11-06 / Claude. Open for parent: user's wording was "SourceIdentity or a session tag"; this plan chooses role session name always plus opt-in SourceIdentity, and does not use session tags (they need `sts:TagSession` in the trust policy for the same reason).

- Decision: Do not choose a duration. When `duration_seconds` is unset, omit `--duration-seconds` and let STS apply its own documented default of 3600 seconds. When it is set, pass it unchanged and let STS reject out-of-range values.
  Rationale: STS accepts 900 seconds up to the role's `MaxSessionDuration` (1 to 12 hours), but when the calling identity is itself an assumed role (role chaining) the cap is 1 hour. A 3600-second request is valid in every case, including chaining, and it is STS's default, so Mjolnir invents no constant. Reading the role's maximum would need `iam:GetRole` on the controller's identity, which the person may not grant, and a longer lifetime widens the window in which stolen credentials work. Refreshing costs one STS call per session per half hour. The refresh rule below derives from the granted lifetime and so scales with whatever duration applies. STS's own error text names an out-of-range duration, which Mjolnir surfaces.
  Date/Author: 2026-11-06 / Claude. Open for parent: the request said "take the maximum from the role". This plan departs from that deliberately, for the reasons above; say so if the maximum is wanted (it would add an `iam get-role` call and its permission).

- Decision: Refresh a cached scoped credential when half of its granted lifetime has elapsed.
  Rationale: The GitHub App token cache refreshes below a fixed ten-minute threshold (`TOKEN_REFRESH_THRESHOLD` in `mj-controller/src/controller/github_app.rs`) because its tokens always last an hour. A fixed threshold would be wrong for a configurable lifetime (a 15-minute lifetime would refresh on every pass). Half the granted lifetime is derived, not guessed: it leaves at least 7.5 minutes (the shortest STS lifetime is 15 minutes) between refresh and expiry, against a credential sync cadence of about one minute (`docs/src/content/docs/security.md`, "Credential synchronization and exclusion"), so a transient STS failure gets several retries before the session's credentials lapse.
  Date/Author: 2026-11-06 / Claude.

- Decision: The controller mints, using the host's own chain through the AWS CLI, run through the shared subprocess helpers.
  Rationale: `crate::aws_credentials::export` already shells out to `aws configure export-credentials` through `CommandExecutor` and `BoundedProcessExecutor` with the selected `AWS_PROFILE` and environment (`AwsCredentialSource`). Minting uses the same executor, environment and `--profile`, so whichever login the person has (SSO, static keys, a credential process) works the same way and no AWS SDK dependency is added. The existing 30-second bound on that command is reused; it is not a new cap.
  Date/Author: 2026-11-06 / Claude.

- Decision: Failure behavior. If AssumeRole fails at session launch or resume, the launch is refused with the AWS error code and a stated remedy; the session does not start with unscoped credentials. If a refresh fails during a running session, the worker keeps the credentials it already has (they are scoped and still valid until their expiry), the sync reports a failed outcome that the existing credential-sync notices show to the person, and the next pass retries. There is no fallback to the unscoped export at any point.
  Rationale: AGENTS.md says a narrow fallback hides a broken design and that failure must be visible. A Bedrock harness cannot call its model without AWS credentials, so a session that started with none would be a dead end that looks alive; refusing the launch with the reason is the same behavior as today's failed export ("Could not resolve AWS credentials for the isolated worker") and tells the person how to proceed. The remedy text includes the one-step exit "remove `role_arn` to return to unscoped credentials", so the person is never stuck. Keeping the previous scoped credentials during a transient outage is not a weaker scope, so it is not the forbidden kind of fallback.
  Date/Author: 2026-11-06 / Claude. Open for parent: confirm refusal at launch (versus starting the session without AWS credentials).

- Decision: Local bare and EC2 targets are out of scope for scoped credentials and the documentation says so. Doctor's unscoped warning is raised only when the profile would actually receive synced credentials on some configured target.
  Rationale: Scoping files copied to a local bare worker would be a mirage because it can read the host's own AWS files; EC2 sessions use the instance profile. A warning that fires for a profile used only on local bare would be noise the person cannot act on. The "does this profile and backend receive synced credentials" question is answered by one shared predicate used by `source`, by the launch environment code, and by doctor, so the three cannot disagree (AGENTS.md: two predicates for one question are a bug).
  Date/Author: 2026-11-06 / Claude.

- Decision: Use `--metadata-options HttpTokens=required` only (as decided by the requester), verify it in the launch response, and have doctor warn on a launch template that allows IMDSv1.
  Rationale: Command-line parameters to `run-instances` take precedence over the launch template's values, but this is the one point the plan could not verify without an AWS account, so Milestone 0 confirms it, and also confirms whether passing only `HttpTokens` leaves the template's `HttpPutResponseHopLimit` alone. Verification in the response means that if the override ever does not apply, the launch fails visibly instead of leaving an IMDSv1 instance. Doctor's warning is a warning, not a failure, because Mjolnir's own launches are protected regardless; the template matters to other users of it.
  Date/Author: 2026-11-06 / Claude.

- Decision: No relay protocol change, no database migration, no new wire message.
  Rationale: See the second discovery. Older workers that already understand protocol 46 accept scoped credentials as they accept today's. Older workers (below protocol 46) are already skipped for AWS sync. Configuration is additive and optional, so an existing `config.toml` loads unchanged.
  Date/Author: 2026-11-06 / Claude.

- Decision (2026-11-06 acceptance by the user's review of this plan): all five open decisions are accepted as written above: (1) omit `DurationSeconds` so STS applies its 3600-second default, no role-maximum lookup; (2) role session name `mj-<id>` always, `SourceIdentity` opt-in via `source_identity = true`, no session tags; (3) a built-in Bedrock invoke/list session policy is used when a role has no policy configured, provisional until Milestone 0; (4) a failed AssumeRole refuses the launch with the AWS error and a remedy, with no unscoped fallback and no credential-less session; (5) an IMDSv1 launch template is a `mj doctor` warning.
  Rationale: user decision recorded verbatim in the review message that started implementation.
  Date/Author: 2026-11-06 / user, recorded by Claude.

- Decision (Milestone 0 result): keep the six-action default session policy (`InvokeModel`, `InvokeModelWithResponseStream`, `ListInferenceProfiles`, `GetInferenceProfile`, `ListFoundationModels`, `GetFoundationModel`); do not widen to `bedrock:*`.
  Rationale: both harnesses completed turns with only the two invoke actions (Codex with `InvokeModel` alone), so the required actions are covered; the four list/get actions are kept as a small superset for model discovery and harness updates that may call them, at no widening of the role. The default is still a narrowing: the live test showed `s3` and `iam` calls denied under it.
  Date/Author: 2026-11-07 / Claude.

- Decision (Milestone 0 result): the error-line parser accepts the AWS CLI v2 prefix `aws: [ERROR]: ` and the shape `An error occurred (<Code>) when calling the AssumeRole operation: <message>`; for `ParamValidation` it reports the code and the following `Invalid value` line, and for anything else only the exit status.
  Rationale: this is what the recorded real output looks like.
  Date/Author: 2026-11-07 / Claude.

- Decision (implementation): the settings form for `[profiles.<id>.aws]` is deferred; the table is edited in `config.toml`, round-trips through Settings because it is a typed profile field, and is documented as such in `profiles.md`.
  Rationale: Settings builds its pages from JSON defaults, and a new profile section would need a default object whose required `role_arn` is empty, which would be written to every profile on save and fail validation. A correct form needs "optional section with a required field" support in the settings schema, which is a separate change; config.toml editing is the same route GitHub App settings use today.
  Date/Author: 2026-11-07 / Claude. Deferred item reported to the parent.

- Decision (implementation): the "applies only to a profile that authenticates with the AWS credential chain" refusal is in `HarnessProfile::ensure_ready`, not `validate`; `validate` only checks the table's shape.
  Rationale: whether a profile uses the AWS chain depends on its environment and, for Codex, on its home's `config.toml`, which `ensure_ready` already reads (the same split `guardian_review_model` uses). The refusal reaches the person through the same profile-refusal path and doctor.
  Date/Author: 2026-11-07 / Claude.

- Decision (implementation): the cache uses a `std::sync::Mutex` per key held across the blocking mint, and callers run it in `spawn_blocking`, instead of the plan's `tokio::sync::Mutex`.
  Rationale: session launch is blocking code and the sync is async; one blocking implementation serves both, and single-flight per key holds the same. The map lock is held only to find the cell.
  Date/Author: 2026-11-07 / Claude.

- Decision (implementation): a failed renewal is always an error, even while the previous credentials are still valid.
  Rationale: the worker keeps what it already has (nothing is pushed on failure), the failure is reported at once instead of at expiry, and the code has no in-memory fallback path. This matches the failure-behavior decision above.
  Date/Author: 2026-11-07 / Claude.

- Decision (review fixes, 2026-11-07): IMDSv2 is enforced on the instance by one rule, `ensure_imdsv2` in `mj-controller/src/controller/backend.rs`, called from `ec2_locator_after_launch`, which every path that adopts an EC2 instance passes through (fresh launch, a Move destination, and a `Created` Move destination found again after a restart). It reads `MetadataOptions.HttpTokens`; if it is not `required` it runs `aws ec2 modify-instance-metadata-options --instance-id <id> --http-tokens required` and reads again, failing (and so triggering the caller's cleanup) only if that fails. This replaces the earlier check of the `run-instances` response, which terminated an accepted instance and did not cover older records. The `modify` response shape is from the AWS CLI reference and is recorded in `aws-modify-instance-metadata-options.json`; it was not live-verified.
  Rationale: terminating an accepted instance that may hold work is a worse outcome than fixing its setting, and a rule at one place cannot be skipped by a path.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (review fixes): a Move launch is replayed exactly as recorded. The stored argument vector already records whether the request carried `--metadata-options`; a record from an older daemon holds none, so it is sent without. Adding the option on a retry changes the parameters of an accepted client token, which EC2 refuses (IdempotentParameterMismatch), orphaning the instance.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (review fixes): per-session mints in a sync pass run concurrently (`join_all` over blocking tasks, no new cap); the cache key includes the credential source's whole environment; one `SessionHost` value, built from a locator or from a target template with exhaustive matches, decides whether a host keeps its own AWS credentials; the cache's sweep keeps any cell another caller holds and also runs at the start of every credential sync pass.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (second review): `ensure_imdsv2` treats the requirement as met only when `HttpTokens` is `required` and `MetadataOptions.State` is `applied`. EC2 reports a new value with state `pending` before it is in effect (the recorded run-instances and modify responses both show it), so a `pending` state is polled with doubling pauses, starting at the SSH-readiness retry delay, until it changes. Any state other than `applied` or `pending` fails at once. The wait is bounded by the deadline the launch already uses for a new instance (`AWS_SSH_READY_TIMEOUT`) and ends on cancellation; the last pause is cut to the time left.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (second review): the credential sync reconciles each role-scoped session as its own mint completes (all mints start together on blocking threads and are consumed through a `FuturesUnordered`), so a slow or failing mint never delays another session's install. The per-session body moved to `reconcile_target`.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (second review): `mj doctor` probes with a role session name (and `SourceIdentity`, if the profile sets it) of the real shape, `mj-` plus 32 random lowercase hex digits, and its output says it is a probe. A trust policy that allows only the documented shape then no longer refuses the probe.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (third review): when the profile requires role-scoped credentials and no mint can be issued, the sync withdraws whatever AWS credentials the worker holds unless it provably holds scoped credentials this controller issued (its fingerprint equals the unexpired cache entry's). Static host keys never expire, so leaving credentials of unknown origin would keep the old, wider scope indefinitely. Withdrawal needs a new relay request, `RemoveAwsCredentials` (relay protocol 55), which deletes the credential file; a worker below 55 cannot withdraw, and the sync reports that and tells the person to restart the session. Known cost: after a daemon restart during an STS outage the controller cannot prove what a worker holds, so it withdraws scoped credentials too (fail closed); the next successful pass installs new ones. Writing the scope into the credentials and reading it back would avoid this, at the price of a larger wire change.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (third review): the AWS step of a session's sync fails on its own. `reconcile_on` returns the actions taken plus the AWS error, runs the GitHub token, skills and slopcop steps regardless, and the pass records the AWS error as the session's outcome.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (fourth review): every step of a session's sync is independent. The AWS credentials, harness login, skills, GitHub token and slopcop login each run whether or not another failed, a failure of any is recorded on its own, and the outcome keeps the actions taken next to the failures (`CredentialSyncOutcome::step_failure`), so the counters and `pushed_to` still see the work. The canonical inputs a step needs (the profile's login, the GitHub token) are read lazily and fail only that step.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (fourth review): when the profile requires role-scoped credentials and none can be issued, the controller always sends `RemoveAwsCredentials` unless the worker provably holds scoped credentials it issued. The reported state is not consulted because it is "absent" when a provider file is missing or differs although the secret file exists; the worker's removal deletes the secret file whatever the other files look like.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (fourth review): a sync pass reconciles all sessions together (a `FuturesUnordered`), each starting when its own AWS credentials are known, so a slow session actor or mint holds only itself. The per-profile busy flag is unchanged: a pull rewrites the canonical login, so one profile is never reconciled by two passes at once, and ending a pass before a stuck session answered would need a timeout nobody has measured. A stuck session still holds its profile's next pass; the others have been served by then.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (fifth review, replaces the busy-flag part of the previous entry): in-flight reconciliation is tracked per session, not per profile. `SessionsInFlight` (owned by the coordinator, passed to each pass) hands out one `SessionClaim` per session, released when its reconciliation ends on any path; a pass skips sessions another pass still holds, so a session actor that never answers holds only itself and the next pass, started on schedule, serves the profile's other sessions. A trigger whose own session is in flight waits for it, and a freed session wakes the coordinator. The one thing that must not overlap, rewriting the profile's canonical login, has its own per-profile lock held only around a compare-and-write: after a worker's fresher login arrives, the controller re-reads the canonical file under the lock and writes only if the session's copy is still the fresher one. The lock is never held across a call to a worker, and no timeout was added.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (fourth review): the wait for IMDSv2 to apply sleeps through the existing `targets::sleep_unless_cancelled`, and checks cancellation again before each poll. A `[profiles.<id>.aws]` table on a profile that does not use the AWS chain is no longer a refusal: the session starts without scoping, with a launch notice, and doctor warns. The cache's sweep runs once per sync pass, not on every call.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (sixth review): what the worker holds is recognised from a record the worker keeps, not from the daemon's memory. `InstallAwsCredentials` carries an optional scope (role ARN, role session name, expiration); the worker writes it beside the secret (`aws-scope.json`, bound to the secret by fingerprint, written before the secret is published and removed with it), and a new request `AwsCredentialsScope` reports it. After a failed mint, credentials stay only if the worker's report names this session's role and role session name and has not expired; anything else, including credentials with no record, is withdrawn. The scope is sent only to workers at protocol 55 or later (older workers reject unknown fields), and the scope file is also written when launch stages the credentials. This replaces the earlier comparison with the cache entry, which a restart emptied. Relay protocol 55 is unreleased, so it was extended rather than bumped.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (sixth review): a worker before protocol 55 is not a dead end. It cannot withdraw, so the sync reports that Mjolnir is replacing it automatically; no new mechanism was added because the worker upgrade coordinator already replaces a worker whose build differs from the controller's, once its session is idle and keeping its home (`worker_upgrade::tests` pins that a quiet session with an old build is due and a busy one waits), and the next pass then withdraws on the new worker.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (sixth review): push and pull of the profile's login are one decision under the per-profile lock. The canonical file is re-read under the lock and compared with the session's copy before either a push (the bytes pushed are the fresh ones) or a pull (decided again after the session's bytes arrive, before the write). The lock is never held across a call to a worker.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (sixth review): the session claim is the single decision about who works on a session and is taken at the start of the pass, before the mint, the export or anything else for it; the earlier `is_claimed` filter is gone. A session's claim is released when its own reconciliation ends and taken again for the second pass. A pass that finds the triggering session held reports `trigger_skipped`; the coordinator requeues that trigger, holds it while the session is claimed and runs it when a freed session wakes it, so a cause-carrying trigger is never dropped.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (seventh review, reverses part of the third to sixth rounds): the profile's harness login, skills, GitHub token and slopcop login are again reconciled one session after another within one pass per profile, with the coordinator's per-profile busy flag and its deferral of triggers while a pass runs, as before #1424. Making the whole sync concurrent per session, which I did at the parent's request, turned one profile-wide fact (the canonical login) into a set of races: a push from a stale snapshot, two predicates for "is this session busy", and triggers dropped while sessions were claimed. Each patch narrowed one window and opened another, so the structure was changed, not patched again. The per-profile pull lock, the per-session `SessionsInFlight` claims, the re-queueing of skipped triggers and `ProfilePass` are removed. What was kept from the earlier rounds: each step's failure is recorded on its own and does not stop the others, and a step failure is kept beside the actions in the outcome.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (seventh review): only the role-scoped AWS install is an independent per-session operation. Each such session is claimed once at the start of the pass (`AwsInstalls`), before its mint, and that claim is the only decision about whether its install runs; the install (mint, then install or withdraw on the session's own connection under background admission) runs alongside the sequential login pass and is merged into the session's outcome. One session that does not answer therefore delays only its own AWS install and, as before #1424, its profile's sequential login pass. Known limit: a pass that never ends keeps its profile busy, so the other sessions' AWS credentials are not renewed by later passes either; this is the same limit the login sync had before #1424, and ending a pass early would need a timeout nobody has measured.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (seventh review): the scope the worker records includes a fingerprint of the effective session policy (inline policy and managed ARNs, sorted), and credentials are kept after a failed mint only if it equals the configured policy's, so narrowing `session_policy` and failing to assume the role cannot leave broader credentials in place. An `Install` also checks the worker's scope record, not just the secret's fingerprint, and installs again when the record is missing or differs: a worker replaced by an upgrade keeps the secret it was given before it could record a scope. The credential cache is swept on the sync schedule whether or not any session is left; a clean later pass replaces the failure an earlier pass reported for a session; and the AWS files a session's home holds (including the scope record) are one list, `aws_credentials::FILES`, used when profile configuration is refreshed.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (eighth review): a session's login step and its AWS install take turns (`SessionSteps`, one lock per session within a pass) before either asks the recovery gate for the session's admission slot. The gate admits one holder per session and turns a second taker away; without the turn-taking the install was skipped whenever the login step held the slot when the mint completed, silently and again at every sync. Each waits only for the same session's other step, never another session's, and external holders (a recovery copy, a worker upgrade, a lifecycle operation) still turn either away as before. The currency check now also precedes the mint (the mint stays outside admission).
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (eighth review): the question "does this session receive synced AWS credentials, and if not why" has one owner, `credentials_withheld`, returning the reason (not a Bedrock profile, raw local, EC2, target environment sets its own AWS identity, profile environment sets AWS credentials); `receives_synced_credentials` is its absence. A `[profiles.<id>.aws]` role that cannot apply gets a launch notice naming that reason, and doctor reports it per target, whether or not another target is scoped. AWS step errors are labelled by the branch that failed (install or withdrawal).
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (ninth review): AWS renewals are scheduled per session, outside the profile pass. A task per role-scoped session (`RenewalTasks`, started and stopped as targets change, aborted with the coordinator) runs one cycle (claim, mint or take from the cache, install or withdraw under background admission, take turns with the same session's login step), then waits until the cached credentials reach half their lifetime, or the sync interval after a cycle that did not complete cleanly. A stalled session therefore holds its own renewal and, as before #1424, its profile's sequential login pass; it no longer keeps any other session's AWS credentials from being renewed. The skills scan and the profile pass no longer gate a mint. A renewal's outcome reaches the UI as a `partial` result, which can end only the failures of the sessions it names.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (ninth review): the scope record also names the `SourceIdentity` the credentials were issued with, and is authenticated. The controller computes an HMAC-SHA256 over the secret's fingerprint and the record's fields with a key derived from its existing private signing key (the phone cookie key, owner-only in the data directory, created once), so no new secret file was added. The worker stores the tag beside the record; the controller verifies it when it reads the record, and a record that does not verify counts as no record (the credentials are withdrawn). Rotating the signing key therefore withdraws what a failed mint would otherwise keep, the safe direction.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (ninth review): merging an AWS step's result into a session whose login step failed keeps both (the login failure becomes a step failure beside the AWS actions). The "Hard-won" markers on #1424's tests are removed: they cited commits that will never be on master, and the defects were found in review before the feature landed.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (tenth review): a session's queued job owns what authorises it. `JobHold` bundles the session's background admission, its turn among the session's steps and, for a renewal, its install lock; the `CredentialSyncJob` carries it from the moment it is queued on the actor until it has run or been dropped. Stopping the renewal task that queued the job (its target changed, the session went away) therefore no longer releases the admission while the job waits: a replacement task's cycle takes the install lock after the old job finishes. When the job's turn comes the actor drops it unrun if its reply receiver is gone (the awaiting task was stopped) or its admission was cancelled by a lifecycle operation, and refuses it as deferred if the session is no longer current. The pre-mint "claim or skip" is replaced by awaiting the session's install lock, so a replaced task's first cycle waits for the old one instead of skipping and sleeping a sync interval.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (tenth review): an authentication-triggered sync wakes the triggering session's renewal task. The task drops the cached credentials (`ScopedAwsCredentials::forget`), mints new ones and reinstalls or withdraws at once, and answers with its outcome, which the trigger's result merges in, so the notice names an AWS failure instead of advising `mj login`. If nobody is waiting for the answer the task reports it as an ordinary partial result. The task still reports the end of an earlier failure itself.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (tenth review): failures are keyed by the step they belong to (`SyncStep`: role-scoped AWS, the profile's AWS export, harness login, skills, GitHub token, slopcop), in `CredentialSyncOutcome::step_failures`, and the notice dedup key is (profile, session, step). A result clears only the steps it checked: the renewal task's partial result clears the `Aws` failure of the sessions it names; every other result clears everything except `Aws`. Neither an AWS success hides a login failure nor the reverse. A session whose whole login step failed keeps that failure as a `HarnessLogin` step failure when an AWS result is merged beside it.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (tenth review): renewal tasks are indexed by session id when reconciled against the published targets (one map lookup per task, not a scan of all targets per task). The renewal delay and the reuse test share one half-life function (`ScopedEntry::refresh_at`); a delay of zero (credentials due already) is reported as "no schedule" and the task retries after the sync interval instead of renewing in a tight loop.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (eleventh review): a session's AWS step reports one failure that carries every message, the withdrawal's first (it says unscoped credentials may remain), then the refused mint's. The notice map is keyed by (profile, session, step), so two failures under one key would have hidden the first; the step now emits one, `merge_aws_cycle` logs it, and the notice joins any further failures under a key instead of replacing them.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (eleventh review): the signing key that authenticates scope records has one creator. `load_or_create_cookie_key` re-reads and creates the key under a file lock beside it, which serializes every caller in every process (the phone server starting, launches, renewals); an existing valid key is returned without the lock because the file is replaced atomically. Concurrent first uses therefore end with one key, and records signed with it keep verifying.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (eleventh review): `scope_is_authentic` returns a `Result`. A signing key that cannot be read means the answer is unknown, not that the record was forged, so the session keeps its credentials and the step reports why it could not verify (and the log records it). Withdrawal stays the answer to a record that verifiably does not belong to the controller.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (eleventh review): the per-session lock maps are indexed by session id with no scan. A `SessionTurn` removes its own entry on release when nobody else holds or awaits the lock (a waiter holds a strong reference taken under the map's mutex, so the entry survives for it), including when a waiting future is dropped. Taking a lock for a batch of sessions costs one lookup each instead of a pass over every entry.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (twelfth review): a queued job's hold has one owner, the hold's own state machine (`JobHold`: queued, running, over). While the job is only queued, the task awaiting it can abandon it, by being stopped or by the gate cancelling its admission, and abandoning releases the admission, the session's turn and the install lock at once. A close, restart or move waiting for the session, and a daemon handoff behind it, therefore never wait for a session actor busy with a slow worker to reach a job nobody wants. Once the actor starts the job it keeps the hold to the end, because the job is using the session's connection (a lifecycle operation needs that connection anyway). The actor finds an abandoned job over and drops it unrun; there is no second predicate (the old "reply closed" check is gone).
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (twelfth review): the notices own whether a clean AWS cycle ends a failure. Every renewal cycle that reached the session reports its outcome, clean or failed, as a partial result, and `CredentialSyncNotices` clears or keeps the session's `Aws` key from it; the task keeps no `failed_before` of its own, so a replaced task cannot forget an open failure. A cycle that woke for an authentication trigger gives the trigger its full outcome and reports the same checks, without the actions, as its own partial result. A deferred cycle checked nothing and says nothing. The notice lists every newly failed step of a pass in one notice.
  Date/Author: 2026-11-07 / Claude, from review.

- Decision (twelfth review): `sign_scope` failures keep their own message. Only the call to AWS and reading its answer are wrapped in "Could not assume AWS role... or remove [profiles.<id>.aws]", whose advice (give sessions unscoped credentials) does not answer a signing-key problem. The signing key is derived once per process (`SCOPE_KEY`) and kept; the async verification path loads it on the blocking pool on first use and afterwards reads the kept key, so it never takes the key file's lock on the runtime. A key rotation takes effect at the next daemon start, as for phone cookies.
  Date/Author: 2026-11-07 / Claude, from review.

## Outcomes & Retrospective

Implemented through Milestone 6 on 2026-11-07. A Bedrock profile with `[profiles.<id>.aws] role_arn = ...` now gives each container or SSH session credentials assumed for it alone (role session name `mj-<session id>`), narrowed by a session policy, renewed after half their lifetime through the existing credential sync, with no relay or database change. A refused role fails the launch with AWS's error line and a remedy, or fails the running session's sync outcome while the worker keeps what it has; nothing falls back to the unscoped export. Profiles without a role behave as before, and `mj doctor` warns about them and probes a configured role with a real throwaway AssumeRole. Every EC2 launch (ordinary and Move) requires IMDSv2, verified in the launch response, and doctor warns about a launch template that allows IMDSv1.

Validation on the dev profile: workspace `cargo clippy --all-targets -- -D warnings` (default members) clean; `mj-tui` 582 passed; `mj-cli` all targets passed; `mj-core` library 614 passed with 5 failures that also fail on origin/master (`local_git::tests::project_identity_uses_default_branch_tracking_and_keeps_selected_worktree`, which sees this host's git URL rewriting, and four `targets::ssh::tests` against a real sshd); `mj-controller` 2404 passed with 4 failures that also fail on origin/master e2b34009e (`controller::tests::selecting_a_linked_checkout_reuses_identity_and_retains_its_push_settings`, `daemon::tests::child_close_preserves_history_and_reports_without_exporting_the_workspace`, `daemon::tests::failed_reviewer_recovery_releases_parent_upgrade_after_saving_its_journal`, `daemon::tests::child_close_cancel_before_commit_leaves_it_running_and_allows_a_fresh_close`). One further core test, `harness_install::tests::direct_grok_artifact_installs_relocatable_launchers`, failed once under load with "Text file busy" and passed alone and on rerun.

Remaining or deferred: the Settings form for the `aws` table; a live end-to-end run of the finished build against AWS (only Milestone 0 touched AWS, with the same commands and policy); the `--min-count`/`--max-count` rejection by AWS CLI 2.36.24 in the Move launch (filed separately by the parent); proof that `--metadata-options` overrides a launch template that sets `optional` (the response check is the guard). Lesson: the AWS CLI v2 error line has an `aws: [ERROR]: ` prefix that a parser written from the API documentation would have missed; recording real output first (Milestone 0) caught it.

## Context and Orientation

Mjolnir has a controller (the long-running `mj daemon-run` process, crate `mj-controller`) that owns configuration, the database and session lifecycle, and workers (crate `mj-worker`, one per session, running inside a container or on an SSH host or locally) that run the harness. A harness is the coding agent program, such as Claude Code or Codex. The controller talks to each worker over the relay, a private control connection; `RelayRequest` in `mj-core/src/relay/protocol.rs` lists its messages. Credentials travel only on that connection and are kept out of the durable event journal.

Amazon Bedrock is AWS's hosted model service. A Bedrock profile is a harness profile whose `auth_scheme()` (in `mj-core/src/config/harness.rs`, around L1320-L1350) is `AuthScheme::AwsCredentialChain`: Claude with `CLAUDE_CODE_USE_BEDROCK=1` in its environment, or Codex with a Bedrock model provider. Such a profile signs requests with AWS credentials found by the AWS "credential chain" (environment variables, a named profile in `~/.aws/config`, single sign-on, and so on). docs/src/content/docs/profiles.md, section "Codex on Amazon Bedrock", documents the profile side.

How credentials reach a session today. `crate::aws_credentials::source` in `mj-controller/src/aws_credentials.rs` decides whether a session gets synced AWS credentials and builds an `AwsCredentialSource` (defined in `mj-core/src/aws_credentials.rs`: a map of environment variables the AWS CLI should run with, including `AWS_PROFILE`). `Controller::aws_credential_source(session_id)` in `mj-controller/src/controller/worker_binary/launch.rs` (~L56) calls it for a session. At launch (same file, ~L208-L212) the controller runs `crate::aws_credentials::export`, which executes `aws configure export-credentials --format process --profile <profile>` through the `CommandExecutor` trait (`mj-core/src/targets`), turns the output into an `AwsCredentials` value, and installs it into the staged profile home with `AwsCredentials::install`. That writes three files into the session's home: `aws-credentials` (the JSON), `aws-config` (a profile whose `credential_process = cat <path>/aws-credentials`, so the AWS SDK inside the session re-reads the file when its credentials expire), and `aws-empty-credentials`. The same file's launch-environment code (~L1048-L1065) points `AWS_CONFIG_FILE`, `AWS_SHARED_CREDENTIALS_FILE` and `AWS_PROFILE` at them. While the session runs, the credential sync reconciles the worker's copy: `CredentialSyncCoordinator` and `reconcile_profile_guarded` in `mj-controller/src/worker_client/credential_sync.rs` run about once a minute per profile, call `export` once per profile, and `reconcile_on` compares the fingerprint of the controller's `AwsCredentials` bytes with the worker's (`RelayRequest::AwsCredentialsState`) and pushes with `RelayRequest::InstallAwsCredentials` when they differ. The worker side is `mj-worker/src/worker_runtime/unix/requests.rs` (~L428 and ~L455), which decodes, validates (rejecting expired JSON), and installs. Targets are built for the sync by `credential_sync_targets_from_sources` in `mj-controller/src/pollers/worker_targets.rs`; `CredentialSyncTarget` (in `mj-core/src/credentials.rs`, ~L660) carries the `AwsCredentialSource`, and the code that decides whether a queued sync is still valid compares whole targets for equality, so a configuration change that alters the source invalidates obsolete work.

The GitHub App token cache is the model for this design. `GithubAppTokenProvider` in `mj-controller/src/controller/github_app.rs` is shared process-wide (`shared()`), keeps a map from a cache key to a tokio mutex around an optional entry (`token`, `expires_at`), takes an injectable clock (`now: Clock`) for tests, serves a cached token while enough lifetime remains, and mints a new one under the per-key mutex so concurrent callers share one mint.

Failures during a sync are reported by `CredentialSyncOutcome { outcome: Err(String) }` and shown by `CredentialSyncNotices` in `mj-controller/src/pollers/credential_sync.rs`, which posts a notice when the failure message for a profile and session changes.

EC2 provisioning. `ec2_launch_command` in `mj-controller/src/targets/provision.rs` (~L575-L608) builds `aws --profile <p> --region <r> ec2 run-instances --launch-template ... --tag-specifications ... --output json`. It is used both by ordinary provisioning (`provision.rs` ~L164) and by Move to a new EC2 destination (`mj-controller/src/controller/move_session/destination.rs` ~L103, which appends a client token and stores the arguments in the database). The response is parsed in `mj-controller/src/controller/backend.rs` (~L1294) and in `destination.rs` (~L141-L152). Doctor's EC2 check is `aws_target_check` in `mj-controller/src/doctor.rs` (~L2890-L3000); it calls `describe-launch-templates` only. Cleanup of a managed instance is `mj-controller/src/targets/cleanup.rs` (`terminate-instances`). docs/AWS.md documents the machine configuration and IAM permissions; docs/src/content/docs/security.md has the section "Credential synchronization and exclusion".

Terms. STS (Security Token Service) issues temporary credentials. `AssumeRole` is the STS operation that returns temporary credentials for an IAM role. A session policy is a policy passed to AssumeRole; the resulting credentials get the intersection of the role's own permissions and the session policy. A role's trust policy says who may assume it. CloudTrail records API calls with the caller's identity. IMDS is the instance metadata service at 169.254.169.254 from which code on an EC2 instance can fetch the instance profile's credentials; IMDSv2 requires a session token obtained with an HTTP PUT first, which blocks simple request-forgery reads.

Repository rules that shape this plan (AGENTS.md): tests only in the four categories (golden, end-to-end, integration against a third-party, tricky condition), no unit tests of ordinary happy paths; no protective caps on speculation; one mechanism; no silent fallback; run child processes through the shared helpers; test in a named `--instance`; the daemon's handoff admission rules (any new daemon-owned operation must be short and bounded or resumable). Minting runs inside the existing supervised credential-sync task and the launch path, both already admission-aware, and spawns no new background operation.

## Plan of Work

The work has seven parts, ordered so each leaves the repository working. EC2 IMDSv2 is independent of the credential work and goes first because it is small and shippable alone.

Milestone 0, a live prototype, removes the three facts that code reading cannot settle: the exact `aws sts assume-role` output and argument syntax, how `--metadata-options` composes with a launch template, and which Bedrock actions the harnesses need.

Milestone 1 adds the IMDSv2 flag to `ec2_launch_command` in `mj-controller/src/targets/provision.rs`: arguments `--metadata-options HttpTokens=required`. It adds one shared function, in the same module, that takes the parsed `run-instances` response and returns an error unless `/Instances/0/MetadataOptions/HttpTokens` is `required`; the error names the instance and says Mjolnir required IMDSv2 and the response disagrees. Both response parsers (`backend.rs` and `destination.rs`) call it, after the instance id has been persisted so the existing cleanup paths own the instance (read `targets/cleanup.rs` and the provisioning failure handling to confirm that a failure after launch leads to termination, and add a test if it does not). For Move, the stored argument vector of a pending launch may predate this change, so the code that executes a `LaunchPending` destination must apply the same flag idempotently (a small function `with_required_imds(args)` that appends `--metadata-options HttpTokens=required` unless the vector already contains `--metadata-options`), used by `ec2_launch_command` and by the Move execution path, so one definition owns the flag. In `aws_target_check` in `mj-controller/src/doctor.rs`, after the template is found, call `ec2 describe-launch-template-versions` for the configured version (the target's `launch_template_version`, or `$Default`), query `LaunchTemplateVersions[0].LaunchTemplateData.MetadataOptions.HttpTokens`, and return `DoctorCheck::warning` when it is `optional` (or absent, which EC2 treats as the account's default; report absent as "does not set it" without a warning if the response cannot show the account default), with remediation text: set HttpTokens to `required` in the template; Mjolnir requires it for its own launches either way. If the describe call fails (it needs `ec2:DescribeLaunchTemplateVersions`, which docs/AWS.md already lists), keep the existing Ready result and say the setting could not be read. Update docs/AWS.md: the exact `run-instances` command now includes `--metadata-options`, the new doctor check, and a statement that sessions on EC2 read instance-profile credentials through IMDSv2 only.

Milestone 2 adds configuration. Create `mj-core/src/config/aws.rs` with `AwsRoleConfig` (see Interfaces), re-exported like the GitHub types in `mj-core/src/config`. Add `pub aws: Option<AwsRoleConfig>` to `HarnessProfile` in `mj-core/src/config/harness.rs` with `#[serde(default, skip_serializing_if = "Option::is_none")]`. Extend the profile's validation (the same place that rejects `muse_auto_review` on a non-Muse profile) to reject `aws` on a profile whose `auth_scheme()` is not `AwsCredentialChain`, with the message "`[profiles.<id>.aws]` applies only to profiles that authenticate with the AWS credential chain (Bedrock)", and to call `AwsRoleConfig::validate`. Extend `AwsCredentialSource` in `mj-core/src/aws_credentials.rs` with `pub role: Option<AwsRoleConfig>` so that a configuration change makes the sync target compare unequal (the existing equality-based staleness check then does the right thing); `Debug` already prints only variable names and must keep doing so (the role ARN is not secret, but the inline policy can be long; print only whether a role is set). Factor the predicate out of `crate::aws_credentials::source` in `mj-controller/src/aws_credentials.rs` into one function (for example `pub(crate) fn backend_receives_synced_credentials(profile, backend_kind_or_locator, target_environment) -> bool`) that `source` calls, that the launch-environment code at launch.rs ~L1048 reaches through `source`, and that doctor calls with a target template instead of a locator; have `source` fill `role` from `profile.aws`. The settings surface (`mj-client/src/settings/schema.rs`, profile defaults ~L47, field labels ~L290-L330, and `hidden_key` in `mj-client/src/settings/page.rs` ~L196) gets a nullable `aws` object so a person can edit it; hide it for harness kinds other than Claude and Codex, as `guardian_review_model` is hidden for non-Codex kinds. Inspect `mj-controller/tests/golden/viewer-settings-exchange.txt` and extend it if it renders profile fields.

Milestone 3 adds minting. Create `mj-controller/src/aws_assume_role.rs` (declared in `mj-controller/src/lib.rs`) containing a process-wide `ScopedAwsCredentials` provider shaped like `GithubAppTokenProvider`: `shared()`, a `now` clock field for tests, and a map from `ScopedKey` (session id plus the `AwsRoleConfig` plus the selected `AWS_PROFILE` value) to a tokio mutex around `Option<ScopedEntry>`, where `ScopedEntry` holds the `AwsCredentials`, `minted_at`, and `expires_at`. The method `credentials_for(&self, session_id, source, executor) -> Result<AwsCredentials>` takes the key's mutex, returns the cached entry if `now < minted_at + (expires_at - minted_at) / 2`, and otherwise mints. On every call it first drops entries whose `expires_at` has passed, so the daemon does not hold expired secrets and no entry outlives its usefulness. Minting runs `aws sts assume-role` with `sensitive_output = true`, the source's environment, `AWS_CLI_AUTO_PROMPT=off`, `--profile <AWS_PROFILE>`, `--role-arn`, `--role-session-name mj-<session id>`, `--output json`, plus `--policy <json>`, `--policy-arns arn=<arn>...`, `--duration-seconds <n>`, and `--source-identity mj-<session id>` as configured (the built-in default policy supplies `--policy` when neither policy option is set). It parses the `Credentials` object (`AccessKeyId`, `SecretAccessKey`, `SessionToken`, `Expiration`) into the existing `AwsCredentials` through the same JSON shape `from_export` accepts (add `AwsCredentials::from_assume_role(profile, bytes)` next to `from_export`, with the same no-secret-in-errors discipline). The blocking CLI call runs in `tokio::task::spawn_blocking` with `BoundedProcessExecutor::new(Duration::from_secs(30))`, exactly as `reconcile_profile_guarded` does for export today. Error text never includes CLI output, except one line: when stderr matches the AWS CLI's `An error occurred (<Code>) when calling the AssumeRole operation: <message>` shape, the code and message are included, because they name the problem (`AccessDenied`, `ValidationError`, `ExpiredToken`) and contain no credentials; any other failure reports only the exit status and the standard advice. Add one method on the controller side, `obtain(source, session_id, executor)`, that returns `export(...)` when `source.role` is `None` and `credentials_for(...)` otherwise, and call it from launch.rs ~L208-L212 in place of the direct `export` call. Launch errors gain the context text: "Could not assume AWS role <role_arn> for this session: <code and message>. Check that the controller's AWS login can assume the role and that the role's trust policy allows it; or remove `[profiles.<id>.aws]` to give sessions your AWS credentials unscoped."

Milestone 4 integrates the sync. In `reconcile_profile_guarded` (`credential_sync.rs` ~L303-L320), keep the single per-profile `export` for targets whose `AwsCredentialSource.role` is `None`, and for targets with a role call the provider per target inside the existing per-target loop, within the `gate.run_background` block next to the per-target GitHub token fetch, so the work stays admission-gated and supervised like the GitHub token. A failure becomes that target's `CredentialSyncOutcome { outcome: Err(..) }`, and nothing is pushed for that session, so the worker keeps its current files until they expire. `reconcile_session` and `reconcile_on` need no change: they take an `AwsCredentials` value and compare fingerprints, and the cache returns the identical entry until refresh, so a minute-by-minute pass does not push every time. After a daemon restart the cache is empty, so each live role-configured session receives one fresh mint and one push; that costs one STS call per session and is accepted rather than adding a relay request to read the worker's copy. Verify that the notice text produced for a failed outcome reaches the person (via `CredentialSyncNotices`) and reads clearly for the AWS error line.

Milestone 5 adds doctor checks. In `mj-controller/src/doctor.rs`, next to `aws_harness_authentication_check` (~L1063), add a scope check for each Bedrock profile. With `profile.aws` unset and the shared predicate true for at least one configured target template, return `DoctorCheck::warning` saying that sessions of this profile on isolated targets receive the controller's AWS credentials unscoped, with remediation pointing to `[profiles.<id>.aws]` and docs. With `profile.aws` set, run one real `aws sts assume-role` through the same command builder as Milestone 3 with role session name `mj-doctor` and report Ready ("assumed <role> with the configured policy; credentials last until <Expiration>") or a fixable failure carrying the AWS error line and the remedy. When `AWS_BEARER_TOKEN_BEDROCK` is set in the profile (an API key rather than the credential chain), report that the key is not covered by this feature. Doctor probes run through the existing `executor`, so tests pass a hand-written fake.

Milestone 6 writes documentation, finishes tests, runs validation, and performs the live check described under "Validation and Acceptance".

## Concrete Steps

All commands run from the repository root, `/home/jonathan/Projects/mjolnir-wt/1424-aws-scoped` (or a fresh worktree of it). Run `cargo test` and `cargo clippy` outside the restricted sandbox (AGENTS.md). Do not redirect Cargo's output directory. Test any build of the daemon with `--instance aws-scoped-test`; a binary run from a Cargo target directory refuses the default instance.

Before Milestone 0, claim the issue: `gh issue edit 1424 --add-assignee @me --add-label agent-in-progress` (AGENTS.md "Coordination").

Milestone 0 (needs an AWS account the person controls; if the implementer has none, ask the person to run these and paste the output). Create an IAM role `mj-scoped-test` that trusts the identity of the controller host's AWS profile and has Bedrock invoke permissions. Then:

    aws --profile <host-profile> sts assume-role \
      --role-arn arn:aws:iam::<account>:role/mj-scoped-test \
      --role-session-name mj-0123456789abcdef0123456789abcdef \
      --policy '{"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":["bedrock:InvokeModel"],"Resource":"*"}]}' \
      --policy-arns arn=arn:aws:iam::aws:policy/AmazonBedrockReadOnly \
      --duration-seconds 900 \
      --source-identity mj-0123456789abcdef0123456789abcdef \
      --output json

Expected: a JSON document with `Credentials` (`AccessKeyId`, `SecretAccessKey`, `SessionToken`, `Expiration`), `AssumedRoleUser`, and `SourceIdentity`. Save it with the secrets replaced by obvious placeholders (keep the `Expiration` format and the field set exactly) as `mj-controller/tests/fixtures/aws-sts-assume-role.json`. Record the stderr line of a deliberate failure (an unknown role ARN) in `mj-controller/tests/fixtures/aws-sts-assume-role-access-denied.txt`. Confirm the `--policy-arns arn=...` syntax, and that omitting `--duration-seconds` yields a one-hour `Expiration`. For IMDS: launch with a template that sets `HttpTokens=optional` and `HttpPutResponseHopLimit=2`, adding `--metadata-options HttpTokens=required`, and record `/Instances/0/MetadataOptions` from the response and from a later `describe-instances`; save the response excerpt as `mj-controller/tests/fixtures/aws-run-instances-imdsv2.json`. If the hop limit reverts to 1, decide whether to pass `HttpPutResponseHopLimit` from the template (add a Decision Log entry). For Bedrock: run one Claude (`CLAUDE_CODE_USE_BEDROCK=1`) turn and one Codex Bedrock turn with the role's session policy set to the provisional default in Interfaces, then check CloudTrail (or the harness error output) for `AccessDenied` events; add any action found, or widen to `bedrock:*` as recorded above. Write the results in `Surprises & Discoveries`.

Milestone 1:

    cargo test -p mj-controller ec2_launch
    cargo test -p mj-controller doctor_ec2

Expected after the change: the launch-argument test shows `--metadata-options HttpTokens=required`; the new verification test fails on a recorded response that says `optional` and passes on `required`; the doctor test shows the warning for a template that allows IMDSv1.

Milestones 2 to 5, after each: `cargo test -p mj-core aws`, `cargo test -p mj-controller aws_assume_role`, `cargo test -p mj-controller credential_sync`, `cargo test -p mj-controller doctor`, using the filters that match the tests added (libtest accepts several filters; AGENTS.md says run only what changed per round).

Milestone 6, once: `cargo clippy --all-targets -- -D warnings` for the workspace and the full test suites of `mj-core`, `mj-controller`, `mj-client` and `mj-worker` (the touched crates), on the dev profile.

## Validation and Acceptance

Automated, in the four allowed categories.

Integration with the AWS CLI (a third-party tool): the recorded `aws-sts-assume-role.json` fixture is parsed by `AwsCredentials::from_assume_role`; the test asserts the resulting process-format JSON, that `Expiration` (the CLI emits an offset like `+00:00`) is accepted by `validate`, and that the secrets are absent from every error the function can return. A second test feeds the recorded failure line and asserts the surfaced error contains the AWS error code and message and nothing else from the output. The command-line vector built for a configured role (role, inline policy, two managed policy ARNs, duration, source identity) is compared with a checked-in golden file `mj-controller/tests/golden/aws-assume-role-command.txt`; the same arguments are the ones Milestone 0 ran against real AWS, which is what ties the golden to the real CLI. The tests never call AWS; they use a hand-written fake `CommandExecutor`, like `FailedExport` in `mj-controller/src/aws_credentials.rs` tests.

Tricky conditions, in `mj-controller/src/aws_assume_role.rs` with the injected clock: a cached credential is served before half its granted lifetime and replaced after (shortest 900 s and default 3600 s cases); two sessions with the same role get separate entries and separate session names; expired entries are dropped; two concurrent callers for one key cause exactly one mint; a failed mint returns an error and does not corrupt or evict a still-valid entry. In `mj-controller/src/worker_client/tests.rs`, extending the existing AWS sync test near L1558 (which builds `remote.aws_credentials = Some(AwsCredentialSource {..})`): a role-configured session whose mint fails produces an `Err` outcome and sends no `InstallAwsCredentials` request, and the worker's installed files are unchanged; a role-configured session whose mint succeeds installs once and a second pass pushes nothing. A test with a source that has `role: Some(..)` asserts the unscoped `export` is never invoked for it (a fake executor that fails the test if it sees `configure export-credentials`).

EC2: update the existing `ec2_launch_command` expectations in `mj-controller/src/targets/tests.rs` (~L2587 and ~L2647) to include the flag; a test of the verification function with the recorded response excerpt (`required` passes, `optional` fails, a missing field fails); a Move test that a stored `LaunchPending` argument vector lacking the flag gets it before execution and one that has it is unchanged (this pins the upgrade case); a doctor test with a fake executor returning a template version with `HttpTokens: optional`. If `mj-controller/tests/golden/doctor-external-runtime-checks.txt` renders the EC2 or harness AWS checks, extend that golden for the new check text instead of adding new tests.

Config: extend the existing settings golden if the profile fields appear in it; no separate parse test for defaults or field validation.

Manual, in an isolated instance (never the default instance), after Milestones 1 to 5:

    cargo build
    target/debug/mj --instance aws-scoped-test daemon status

Create a throwaway `MJ_CONFIG_DIR` and `MJ_DATA_DIR` with a Bedrock Claude profile (`CLAUDE_CODE_USE_BEDROCK=1`, `AWS_PROFILE=<host-profile>`), add `[profiles.<id>.aws]` with `role_arn` for the role from Milestone 0, add a Podman or Docker target, and start a session in the TUI driven through tmux (live-test isolation, as recorded in the user's notes). Inside the session run `aws sts get-caller-identity`; expect an ARN ending in `assumed-role/mj-scoped-test/mj-<session id>`. Wait past half the lifetime (use `duration_seconds = 900` and wait about 8 minutes) and rerun it; expect the same role with a new `AccessKeyId` prefix or a later expiry visible in the file `aws-credentials` in the session home, with no restart. Break the trust policy and wait for the next refresh; expect a credential-sync notice naming `AccessDenied`, and the session still working until the old credentials expire. Remove the `[profiles.<id>.aws]` table and run `mj --instance aws-scoped-test doctor`; expect the unscoped warning. Launch an EC2 session on an `aws-ec2` machine and run the `describe-instances` command from the Purpose section; expect `required`.

Acceptance is the behavior above plus: the new tests fail before their change and pass after, workspace `cargo clippy --all-targets -- -D warnings` is clean, and the touched crates' test suites pass.

## Idempotence and Recovery

Every step is additive and repeatable. Configuration is optional; removing `[profiles.<id>.aws]` returns a profile to today's behavior on the next sync pass and the next launch. The cache is rebuildable: a daemon restart empties it and costs one STS call per live role-configured session. Credential files in a session are replaced atomically by the existing `AwsCredentials::install`. Doctor's AssumeRole probe is read-only apart from a CloudTrail event. In Milestone 1, if the IMDS verification rejects a launch, the instance has an owned, persisted id and follows the existing termination path; if Milestone 0 shows the launch-template override does not apply, stop and revisit the decision rather than weakening the verification. If a milestone has to be reverted, each stands alone: the EC2 change does not depend on the credential changes, and Milestones 2 to 5 are guarded by the optional `aws` table.

## Artifacts and Notes

Provisional default session policy, used only when `role_arn` is set and no policy option is configured (confirm in Milestone 0):

    {"Version":"2012-10-17","Statement":[{"Effect":"Allow","Action":[
      "bedrock:InvokeModel","bedrock:InvokeModelWithResponseStream",
      "bedrock:ListInferenceProfiles","bedrock:GetInferenceProfile",
      "bedrock:ListFoundationModels","bedrock:GetFoundationModel"],
      "Resource":"*"}]}

The Bedrock Converse APIs are authorized by `bedrock:InvokeModel`, so no separate action is listed. The resulting permissions are this list intersected with the role's own, so the person's role still bounds the resources (specific model ARNs, regions).

Example configuration after the change:

    [profiles.bedrock-luna]
    kind = "codex"
    home = "/home/ubuntu/.codex-bedrock"

    [profiles.bedrock-luna.environment]
    AWS_REGION = "us-east-1"
    AWS_PROFILE = "work"

    [profiles.bedrock-luna.aws]
    role_arn = "arn:aws:iam::123456789012:role/mj-bedrock-session"
    # session_policy = '{"Version":"2012-10-17","Statement":[...]}'
    # session_policy_arns = ["arn:aws:iam::aws:policy/AmazonBedrockReadOnly"]
    # duration_seconds = 3600
    # source_identity = true

What the role needs. The controller's AWS identity (the `AWS_PROFILE` above) needs `sts:AssumeRole` on that role. The role's trust policy must name that identity. If `source_identity = true`, the trust policy must also allow `sts:SetSourceIdentity`. The role's own permissions should already be limited to what sessions need; the session policy then narrows them further per session.

Honest limits to state in the documentation: this scopes credentials delivered to container and SSH sessions through the credential sync; local bare sessions still read the host's AWS files directly, EC2 sessions use their instance profile (now IMDSv2 only), and a Bedrock API key (`AWS_BEARER_TOKEN_BEDROCK`) is not covered. Scoped credentials limit what stolen credentials can do and for how long; they do not stop code in the session from using them during their lifetime.

Milestone 0 resource log (all times UTC, account 842609633142, caller `arn:aws:iam::842609633142:user/jonathan`, profile `default`; role trust allowed only that user, tagged `purpose=mj-1424-m0`):

    2026-11-07T03:47:52Z CREATED iam role mj-1424-m0-test
    2026-11-07T03:47:59Z CREATED inline policy mj-1424-m0-bedrock on that role
    2026-11-07T03:49:17Z CREATED ec2 instance i-0f59152b3ebf671ba (us-east-1, template hel-runson, m8i-flex.large)
    2026-11-07T03:50:09Z TERMINATED that instance (verified state terminated)
    2026-11-07T03:58:18Z DELETED inline policy mj-1424-m0-bedrock
    2026-11-07T03:58:20Z DELETED iam role mj-1424-m0-test
    2026-11-07T03:58:25Z VERIFIED: get-role returns NoSuchEntity; no instance tagged purpose=mj-1424-m0 is live

Nothing else was created in AWS. AssumeRole calls (about ten, including deliberate failures) and Bedrock invocations (about six one-word turns through isolated `mj` sessions) appear in the account's logs and billing. The first EC2 attempt used `--min-count`/`--max-count`, was rejected by the CLI before any resource existed, and needed no cleanup. Locally, the lab (`~/m0-1424-lab`, with copied binaries and an isolated daemon, `--instance m0-1424`) and `/tmp/m0-1424` were deleted; no real profile home or live config was touched.

## Interfaces and Dependencies

No new crate dependency. The AWS CLI v2 (`aws`), already required, is the only AWS interface.

In `mj-core/src/config/aws.rs`:

    #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct AwsRoleConfig {
        pub role_arn: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub session_policy: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pub session_policy_arns: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub duration_seconds: Option<u32>,
        #[serde(default, skip_serializing_if = "is_false")]
        pub source_identity: bool,
    }

    impl AwsRoleConfig {
        /// role_arn has the form arn:<partition>:iam::<12 digits>:role/<path and name>;
        /// session_policy, when set, parses as a JSON object; each policy ARN starts with
        /// "arn:". Sizes and counts are left to STS to refuse; Mjolnir does not restate its limits.
        pub fn validate(&self) -> Result<()>;
    }

In `mj-core/src/aws_credentials.rs`: `AwsCredentialSource` gains `pub role: Option<AwsRoleConfig>`; `AwsCredentials` gains `pub fn from_assume_role(profile: &str, bytes: &[u8]) -> Result<Self>` (reads the STS `Credentials` object, reuses `validate`, no secret in any error).

In `mj-controller/src/aws_credentials.rs`: one predicate shared by `source`, launch and doctor, `pub(crate) fn receives_synced_credentials(profile: &HarnessProfile, backend: BackendKind, target_environment: &BTreeMap<String, String>) -> bool` (where `BackendKind` is whatever lets both a `TargetLocator` and a `TargetTemplate` be classified as local-bare, EC2 or other; the implementer chooses the smallest shape that both callers can build), and `pub(crate) fn obtain(source: &AwsCredentialSource, session_id: &str, executor: &impl CommandExecutor) -> Result<AwsCredentials>`.

In `mj-controller/src/aws_assume_role.rs`:

    pub(crate) struct ScopedAwsCredentials { /* clock, map of ScopedKey to tokio Mutex<Option<ScopedEntry>> */ }
    impl ScopedAwsCredentials {
        pub(crate) fn shared() -> Arc<Self>;
        pub(crate) async fn credentials_for(&self, session_id: &str, source: &AwsCredentialSource)
            -> Result<AwsCredentials>;
        pub(crate) fn assume_role_command(role: &AwsRoleConfig, selected_profile: &str,
            session_name: &str) -> CommandSpec;   // also used by doctor with "mj-doctor"
    }

In `mj-controller/src/targets/provision.rs`: `pub fn with_required_imds(args: Vec<String>) -> Vec<String>` and `pub fn require_imdsv2(run_instances_response: &serde_json::Value) -> Result<()>`.

Time source: the cache takes `Arc<dyn Fn() -> SystemTime + Send + Sync>`, the same `Clock` type as `github_app.rs`, so tests control it without sleeping.

Documentation to update in the same commits as the behavior: docs/AWS.md (the exact `run-instances` command with `--metadata-options`, the doctor template check, IMDSv2-only instance-profile credentials, and the sentence about hop limit 2 in docs/src/content/docs/profiles.md that must stay accurate for containers on hosts the person manages); docs/src/content/docs/security.md, section "Credential synchronization and exclusion" (what AWS credentials a session gets, scoped versus unscoped, the limits listed under Artifacts and Notes); docs/src/content/docs/profiles.md, section "Codex on Amazon Bedrock" (the `[profiles.<id>.aws]` table, the role's trust and permission requirements, and the same table for Claude on Bedrock).

Revision note (2026-11-06): initial version of the plan, written for parent review before any implementation. Reason: requested design-first deliverable for #1424.

Revision note (2026-11-07): recorded the user's acceptance of the five decisions and the Milestone 0 results (fixtures, CLI error shape, EC2 override left unproven, Move `--min-count` incompatibility, Bedrock action evidence). Reason: Milestone 0 complete; implementation continues from Milestone 1 after parent review.

Revision note (2026-11-07, later): Milestones 1 to 6 implemented; Progress, Decision Log and Outcomes updated with the implementation decisions, deferred items and test results. Reason: implementation complete.

Revision note (2026-11-07, review): seven review findings fixed (Move replay, in-place IMDSv2 enforcement on every adoption path, concurrent mints, cache key, single host predicate, sweep race, sweep from the sync pass). Reason: review of the first implementation.

Revision note (2026-11-07, second review): applied-state wait for IMDSv2, per-session reconcile as mints complete, doctor probe identity of the real shape. Reason: second review.

Revision note (2026-11-07, third review): withdrawal of unscoped credentials on a failed mint (relay protocol 55) and an independent AWS step in the session sync. Reason: third review.

Revision note (2026-11-07, fourth review): independent sync steps, unconditional withdrawal, concurrent per-session dispatch, cancellable IMDS wait, step failures kept beside actions, sweep per pass, non-Bedrock table is a notice. Reason: fourth review.

Revision note (2026-11-07, fifth review): per-session in-flight tracking replaces the per-profile busy flag; the canonical login rewrite has its own lock. Reason: a session that never answers must not hold the profile's other sessions.

Revision note (2026-11-07, sixth review): scope recorded by the worker, pre-55 workers left to the automatic upgrade with an honest message, push and pull decided under the lock, claim before the mint with requeue of skipped triggers. Reason: sixth review.

Revision note (2026-11-07, seventh review): sequential per-profile login pass restored; only the AWS install is per-session; scope carries the policy fingerprint; reinstall on a missing scope record; sweep on schedule; clean pass clears failures; one list of AWS files. Reason: the concurrent design kept producing races.

Revision note (2026-11-07, eighth review): per-session turn-taking before the admission slot, per-target reasons for an inapplicable role, labelled AWS errors, currency check before the mint. Reason: eighth review.

Revision note (2026-11-07, ninth review): per-session AWS renewal tasks, SourceIdentity and an authenticated scope record, merge keeps AWS actions, test markers removed. Reason: ninth review.

Revision note (2026-11-07, tenth review): jobs own their admission, turn and install lock; auth triggers wake the session's renewal task; failures are keyed by step; renewal tasks indexed by session; one half-life function. Reason: tenth review.

Revision note (2026-11-07, eleventh review): one combined AWS failure per step, one creator for the signing key, key errors do not withdraw, lock maps without scans. Reason: eleventh review.

Revision note (2026-11-07, twelfth review): a queued job's hold is released when its awaiter is stopped or its admission cancelled; every renewal cycle reports its outcome to the notices; signing errors keep their message; the signing key is derived once and loaded on the blocking pool. Reason: twelfth review.
