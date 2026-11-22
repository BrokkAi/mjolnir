# Two-stage Mjolnir guardian, all-harness interception, and utility-model escalation

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It follows `.agents/PLANS.md` in this repository.

## Purpose / Big Picture

Mjolnir already ships a worker-side guardian (`mj-worker/src/acp/guardian.rs`) for Kimi Code, OpenCode, and DSH: when one of those harnesses requests tool permission over ACP (`session/request_permission`), the worker spawns a second read-only session of the same harness and reviews the action using OpenAI Codex's guardian policy. However, harnesses that ship their own built-in reviewers (Codex, Claude Code, Antigravity, Muse Code, Grok Build) currently bypass Mjolnir's guardian completely whenever they emit an ACP permission request, dropping straight to a blocking human permission form. Furthermore, every review currently pays the latency and cost of spawning an ACP reviewer session rather than reusing Mjolnir's configured fast utility model or filtering obvious low-risk actions locally.

After this work completes:

1. Under `ExecutionPolicy::ConfiguredApprovals` (raw targets such as `localhost` and guardian SSH machines), Mjolnir's guardian intercepts every non-plan ACP `session/request_permission` across all eight harnesses (after existing `OwnedMcpApprovals` and literal `gh` auto-approvals), while keeping each harness's own native guardian mode enabled (Option A). Containerized/YOLO targets (`ExecutionPolicy::Unconstrained`) stay untouched and never invoke the Mjolnir guardian or classifier.
2. All guardian reviews flow through one canonical Rust packet builder in `mj-core/src/guardian.rs` (shared identically by production and offline evaluation to prevent train/serve skew) and are sent from `mj-worker` to `mj-daemon` over the authenticated worker relay (`RELAY_GUARDIAN_REVIEW_PROTOCOL = 82`, mirroring `HistoryEndpoint` / `sync_history`).
3. Stage-2 escalation in `mj-daemon` uses `UtilityLlmRuntime` (`mj-import/src/utility_llm.rs`, DRYed with compaction and project-memory merge) whenever a direct utility model is configured, using tool-free structured JSON inference (`infer_structured`) with Codex's policy and conservative missing-context rule. When no direct utility model is configured, the daemon returns `UseAcpFallback` and the worker uses its existing read-only guardian-over-ACP (`ReviewerHost`). During a daemon handoff or restart, the worker holds the pending review under the same `request_id` and the replacement daemon answers it cleanly without splitting deciders by timing.
4. In parallel, an incremental offline evaluation (with a sealed held-out test set and nested coarse ~10% dev slices) audits the 176 Codex guardian denials, evaluates Stage-2 tool-free `UtilityLlmRuntime` on the full denial/high-risk/sampled-approval corpus, and benchmarks zero-shot `karanxa/saroku-guard` (184M DeBERTa-v3) vs `patronus-studio/wolf-defender-jev` (151M mmBERT-small) plus lightweight baselines (never-denied command heads, cross-fitted TF-IDF logistic regression) for both approve-only (Arm 1) and approve+hard-deny (Arm 2) cascades before integrating the winning Stage-1 ONNX classifier into `mj-daemon` in shadow mode first (reusing `mj-github-relevance`'s dynamic `ort` + `tokenizers` runtime).
5. Generalizing `utility-over-acp` across compaction, project-memory merge, and guardian is tracked in a separate follow-up GitHub issue to keep this change focused.

## Progress

- [x] (2026-11-21 19:30Z) Reconnaissance of existing guardian (`mj-worker/src/acp/guardian.rs`), utility inference (`mj-import/src/utility_llm.rs`), ONNX student runtime (`mj-github-relevance/src/github_student.rs`), relay RPC (`mj-core/src/history.rs`, `mj-worker/src/worker_runtime/history.rs`, `mj-session-runtime/src/session_manager/standalone.rs`), and `~/Projects/codex` HEAD (`ea8ccc1d20`).
- [x] (2026-11-21 20:50Z) Plan reviewed by MJ Advisor (`91233b0e`); Option A confirmed by user; incremental coarse-to-fine eval with sealed test set confirmed by user.
- [x] (2026-11-21 21:11Z) Single-pass read-only corpus extraction to `/mnt/optane/tmp/guardian-eval-20261121/data/`: 46,748 decided Codex guardian reviews (46,572 approved, 176 denied across 55 top-level families), 185 Claude auto-mode safety denials across 97 sessions, 171 real non-owned-MCP ACP permission asks (98 Antigravity, 46 Claude, 17 Grok, 10 Codex), approved-twin instability stats, and family-grouped splits (`tune_dev`, `cal_eval` with nested 10%/30%/100% tiers, and `sealed_test`).
- [x] Milestone 1 (Track 1A): Shared guardian policy, packets, and wire types in `mj-core/src/guardian.rs`, protocol 82 negotiation, all-harness profile eligibility, shared structured utility failover, and `mj-import/examples/guardian_eval_tool.rs` for packet export and resumable utility evaluation. The existing worker ACP reviewer uses the shared code and remains limited to Kimi Code, OpenCode, and DSH until Milestone 2 wires daemon review and narrows the remaining ACP fallback launches.
- [x] Milestone 2 (Track 1B): The worker owns pending Guardian reviews, and protocol 82 delivers their wakeups and results. The daemon relay actor runs bounded utility reviews and cancels its work on retirement. Surfaced non-plan ACP permission requests use the Guardian across all eight harnesses under configured approvals. ACP fallback uses a narrowed launch. The profile toggle is available in Settings for every harness, and the user documentation describes both review paths.
- [x] (2026-11-21 21:25Z) Follow-up tracking: Filed `BrokkAi/mjolnir#1634` ("Generalize utility-over-acp fallback for compaction and project-memory merge").
- [x] (2026-11-21 21:50Z) Milestone 3 (Track 2A): Pre-scoring blind audit of all 176 Codex denials (`codex_denials_audited.jsonl`, SHA-256 `008b07c06808c7b12ba27c29cb266c9ec4c0ce255a7a2da63a18a897c4efdf55`: 62 Tag A intrinsic danger, 103 Tag B auth-dependent, 5 Tag C missing evidence, 6 Tag D artifact/dup), isolated ML venv, FP32 ONNX export, and tokenizer overflow / CPU latency / peak RSS measurements (`wolf_compact` 81 ms p50 / 1,427 MB RSS; `saroku_compact` 160 ms p50 / 1,836 MB RSS / 15.1% overflow; `wolf_rich_4k` 557 ms p50 / 2,170 MB RSS; `wolf_rich_12k` 1,890 ms / 4,725 MB RSS ruled out).
- [x] (2026-11-21 23:39Z) Milestone 4 (Track 2B): Completed zero-shot Stage-1 bakeoff across `wolf-defender-jev`, `laya-guard-multilingual`, `saroku-guard`, command-head, and TF-IDF baselines plus 1,015-row Stage-2 coarse eval and 621-row `--with-agents-md` A/B. Selected `wolf-defender-jev` FP32 ONNX with exact Rust `wolf_scene_packet` (1,500-char transcript, `align` question, Arm 1 approve-only, `action_chars <= 1,024`, `case_tokens <= 6,144`, `t_allow = 0.010`) on 28,769-row `cal_100pct` (0.6493 all / 0.6623 cmd / 0.8399 red-team / 0.9450 bare-everyday AUC, 0/42 red-team escapes, 0/10 dangerous bare escapes, 23.4% real ACP cov). One-shot `sealed_test` (7,296 rows, 28 denials): **0/28 denials escaped** (`0/18` high, `0/13` Tag A, `0/13` Tag B) at **17.85% Codex coverage** (`4.80e-06` ONNX FP32 parity).
- [x] (2026-11-21 23:55Z) Milestone 5 (Stage-1 Wolf Integration & Bounded `AGENTS.md` Context): Added 32 KiB bounded `AGENTS.md`/`AGENTS.override.md` collection off-loop inside `GUARDIAN_REVIEW_TIMEOUT` (protocol 83, wire-compatible with protocol-82 workers), canonical Rust `wolf_scene_packet`, and dormant-until-first-use daemon `GuardianClassifier` (`mj-github-relevance/src/guardian_classifier.rs`) reusing `github_student`'s verified HF/SHA-256 pipeline and shared ORT 1.23.0 runtime, wired into `mj-session-runtime` and `mj-daemon` with Patronus MIT license notices and updated user docs. Verified Rust ORT 1.23.0 vs upstream Python `WolfDefenderJevONNX` parity to `1.4e-8` (`git status`) and `7.0e-8` (`rm -rf ~`).

## Surprises & Discoveries

- Observation: `mj-daemon` already ships dynamic ONNX Runtime (`ort = "=2.0.0-rc.15"`, ORT 1.23.0) + HuggingFace `tokenizers` 0.22 in `mj-github-relevance/src/github_student.rs` (CPU, CoreML, CUDA), while `mj-worker` is cross-compiled to static Linux `musl` binaries.
  Evidence: `mj-github-relevance/Cargo.toml:34-43`, `mj-worker-staging/src/worker_binary/process.rs:642`, `containers/Containerfile.agent-dev:72-73`.
- Observation: `~/Projects/codex` HEAD (`ea8ccc1d20`) does give its blocking synchronous guardian read-only inspection commands in a read-only sandbox (`approval_policy = never`, no network), while its fast first-stage classifier (`classifier_instructions.md`) is tool-free. Only 623 of 46,572 Codex approvals (1.34%) and 1 of 176 denials (0.57%) mention local file/target inspection in their rationale.
  Evidence: `/home/jonathan/Projects/codex/codex-rs/core/src/guardian/review.rs:258-262`, `policy_template.md:45-64`, and `/mnt/optane/tmp/guardian-eval-20261121/data/instability.json`.
- Observation: Actual traffic at Mjolnir's ACP insertion point under Option A is ~17 asks/day on Jonathan's host (last 7 days: Antigravity 13.7/day, Grok 2.4/day, Claude 0.6/day, Codex 0.1/day, bare Kimi/OpenCode/DSH 0/day), whereas Codex's own internal guardian runs ~2,546 reviews/day (p50 latency 3.33 s, 0.38% deny rate). Under Option A, Stage 2 automatically answering asks that currently stall for a human is the primary immediate UX win on Jonathan's host, while Stage 1 targets latency/call reduction for higher-volume bare setups (and any future fronting).
  Evidence: Live `mj.sqlite3` read-only audit on 2026-11-21.
- Observation: 27 of 176 Codex denials (15.3%) have an exact approved twin in the same session (and 30/176 anywhere), mostly from post-denial explicit user authorization; 121 of 176 denials carry `user_authorization = low`. This makes pre-scoring audit of intrinsic vs authorization-dependent denials essential before interpreting action-only classifier coverage.
  Evidence: `/mnt/optane/tmp/guardian-eval-20261121/data/instability.json`.
- Observation: `mj.sqlite3` also contains 185 real Claude Code auto-mode safety denials across 97 bare sessions (after excluding 99 `Classifier unavailable` errors), providing an independent cross-harness recall veto corpus alongside the 176 Codex denials and 170 real non-owned-MCP ACP asks.
  Evidence: `/mnt/optane/tmp/guardian-eval-20261121/data/claude_auto_denials.jsonl`.

- Observation: The current `real_acp_asks.jsonl` contains 171 distinct IDs, including 98 Antigravity rows. The earlier extraction summary recorded 170 overall and 97 Antigravity rows.
  Evidence: The Milestone 1 evaluator exported all 171 rows successfully.
- Observation: Zero-shot `wolf-defender-jev` is extremely sensitive to shell/JSON wrappers and missing context: wrapping `git status` in `/bin/bash -lc` flips official `WolfDefenderJev.decide` from `misaligned=0.003 (low)` to `misaligned=0.929 (98% critical)`, and `compact_action_packet`'s 256-char cutoff dropped `[USER GOAL]` on ~79% of real prompts. Stripping `/bin/bash -lc`, dropping synthetic `shell.exec(...)` / `[SYSTEM CONTEXT]` wrappers, and using a clean 3-block scene (`[USER GOAL]` + 2,000-char `[TOOL OUTPUT | history | untrusted]` + `[PROPOSED ACTION | agent]`) lifted `wolf` on `tune_dev` from 0.39–0.58 to 0.75 AUC (`consent`) and on `cal_10pct` OOF to 0.633 all / 0.648 cmd-only / 0.904 red-team AUC (`align`, 0/42 red-team escapes; 17.9% Codex / 23.4% real ACP cov at FNR 5%, vs 16.5% / 5.8% for cross-fitted TF-IDF). By contrast, official `saroku.local_judge.LocalJudge` stays at 0.48–0.55 AUC even with `/bin/bash -lc` stripped, flags `cargo test -p mycrate` at 0.598 UNSAFE, and drops `curl ... @.env` from 1.000 UNSAFE to 0.206 SAFE when prepended with a benign shell comment.
  Evidence: `/mnt/optane/tmp/guardian-eval-20261121/scripts/eval_wolf_clean_cal10.py` and `results_coarse/stage1_zeroshot_findings.md`.

## Decision Log

- Decision: Stay with Option A — keep all harness-native guardians enabled (`agent`, `auto`, `auto_review=on`) and run Mjolnir's guardian on every non-plan ACP `session/request_permission` that reaches Mjolnir across all 8 harnesses under `ExecutionPolicy::ConfiguredApprovals`, leaving `ExecutionPolicy::Unconstrained` (YOLO/containers) untouched.
  Rationale: Confirmed explicitly by the user twice (including after seeing the 2,546/day Codex vs ~17/day ACP numbers); Codex assessments serve as our primary safety-tail fixture corpus, while real ACP asks serve as primary in-distribution coverage.
  Date/Author: 2026-11-21, user.
- Decision: Host Stage-1 ONNX inference and Stage-2 `UtilityLlmRuntime` escalation in `mj-daemon` (`mj-session-runtime`), reached from `mj-worker` over the existing authenticated worker relay (`RELAY_GUARDIAN_REVIEW_PROTOCOL = 82`, mirroring `HistoryEndpoint` / `sync_history` with relay change-wait wakeup and immediate actor completion delivery).
  Rationale: Avoids loading a 150–600 MB model into every worker process, keeps static-musl `mj-worker` binaries free of dynamic `ort`/CUDA/CoreML dependencies, gives Stage 2 direct access to `Config`, all profile credentials, and the SQLite quota cache, and works identically on localhost and bare SSH targets with no extra sockets or SSH tunnels.
  Date/Author: 2026-11-21, parent + Advisor.
- Decision: Never fall back to worker-side ACP guardian due to a daemon disconnect or restart; hold the pending review under its `request_id` on the worker so the replacement daemon answers it. Fall back to worker-side ACP guardian (`ReviewerHost`) ONLY when the daemon explicitly replies `UseAcpFallback` (no direct utility model is configured) or when running in standalone/older-daemon compatibility mode where the daemon never negotiated `RELAY_GUARDIAN_REVIEW_PROTOCOL`.
  Rationale: Advisor Q5 flagged that switching deciders on restart timing creates two different reviewers (different model and tool access) for the same question. Keeping one owner per configuration preserves determinism. (Tradeoff: when a utility model is configured, Kimi/OpenCode/DSH approvals wait briefly across a daemon handoff.)
  Date/Author: 2026-11-21, parent + Advisor.
- Decision: Place the canonical guardian action/evidence packet builder and policy prompt templates in `mj-core/src/guardian.rs` (since `mj-core` already depends on `agent-client-protocol`), and export offline evaluation inputs through that exact Rust builder. Never truncate the planned action; trim context strictly from the oldest end.
  Rationale: Prevents train/serve skew between offline Python evaluation and production Rust inference (Advisor Q5).
  Date/Author: 2026-11-21, parent + Advisor.
- Decision: Hold off on fine-tuning until zero-shot baselines (`saroku-guard` compact, `wolf-defender-jev` compact, `wolf-defender-jev` rich) plus lightweight baselines (never-denied command heads, cross-fitted TF-IDF logistic regression) are evaluated incrementally using nested coarse dev tiers (~10% → 30% → 100%) and a sealed held-out test set (~25% of top-level families). Use FP32 ONNX compute (not INT8 on ORT 1.23) and no monotone Platt/temperature recalibration; threshold the exact shipped artifact's raw logit/score.
  Rationale: Confirmed by user (no premature fine-tuning, incremental coarse-to-fine eval with sealed test set) and Advisor (ORT 1.23 INT8 accuracy warning and monotone-transform invariance).
  Date/Author: 2026-11-21, user + Advisor.
- Decision: Keep `utility-over-acp` generalization out of scope for this change and create a dedicated GitHub issue on `BrokkAi/mjolnir`.
  Rationale: User requested keeping scope focused on guardian escalation while tracking generalized `utility-over-acp` separately.
  Date/Author: 2026-11-21, user.

- Decision: Ship zero-shot `patronus-studio/wolf-defender-jev` (`onnx/onnx_fp32/model.onnx`, MIT) in approve-only mode (Arm 1, `t_allow = 0.010`, `action_chars <= 1,024`, `case_tokens <= 6,144`) alongside bounded 32 KiB `AGENTS.md` context for Stage-2 utility reviews, prioritizing harnesses without native guardians (Kimi Code, OpenCode, DSH) while keeping secondary coverage across all harnesses under `ConfiguredApprovals`.
  Rationale: Confirmed by user after reviewing the head-to-head zero-shot bakeoff (`wolf-defender-jev` vs `laya-guard-multilingual` vs `saroku-guard` vs TF-IDF) and Stage-2 `AGENTS.md` A/B (-13.2 pts false denies on existing-CWD high-risk approvals, +7.1 pts Tag-A recall, +26 ms p50 latency). One-shot `sealed_test` passed with 0/28 denials escaped at 17.85% Codex / 23.4% real ACP coverage and `4.80e-06` ONNX FP32 parity.
  Date/Author: 2026-11-21, user + parent.

## Outcomes & Retrospective

Milestone 1 passed touched-file rustfmt checks, the requested six-crate Clippy check, and a daemon build check. The targeted suites passed after rerunning the corrected OpenCode config fixture. The counts were 6 core guardian, 17 core relay, 147 core config, 10 utility, and 19 worker guardian. Three paid utility tests and one worker test remained ignored. The offline tool exported sample Codex rows and the complete Claude and ACP source files. An isolated Stage-2 run with no provider credentials verified missing-credential results, duplicate-ID handling, and resume without additional writes.

Milestone 2 now leaves pending reviews with the worker during daemon reconnects. The relay change wait reports an enqueue independently of transcript changes. The actor wakes immediately for it, and completed jobs deliver without waiting for a polling tick. Tests cover all four wire outcomes, a relay disconnect and replacement, stale user authorization and withdrawal, configured versus unconstrained requests from Claude, Codex and Antigravity, explicit ACP fallback, and fallback when the daemon has no utility profile. The requested filters passed with 6 core Guardian, 24 worker Guardian, 1 session-runtime Guardian, and 15 TUI profile tests. One worker test remained ignored. The existing ACP plan, Claude permission-form, relay change-wait, and delegation observer regressions also passed. Validation ran locally on Linux.

Milestones 3–5 completed offline evaluation and daemon Stage-1 integration. Bounded 32 KiB `AGENTS.md` collection and canonical Rust `wolf_scene_packet` landed in Slice 1 (`1e462bd54`); dormant-until-first-use Wolf FP32 ONNX inference (`mj-github-relevance/src/guardian_classifier.rs`), cascade wiring in `mj-session-runtime` and `mj-daemon`, bounded instruction-collection timeout, docs, and Patronus MIT attribution landed in Slice 2. Validation passed: core guardian 11/0/0, core relay 17/0/0, relevance guardian 6/0/1 + real local ORT parity 1/0/0 (`1.4e-8` / `7.0e-8` vs Python `WolfDefenderJevONNX`), relevance assets 3/0/0, GitHub student 3/0/5, session-runtime guardian 1/0/0, worker guardian 24/0/1, release packaging 14/0/0, five-crate all-target Clippy `-D warnings`, and touched-file rustfmt. GPU execution was not exercised on this host (CPU fallback and TF32-disabled CUDA config verified at compile/unit level).

## Context and Orientation

Mjolnir splits work between the control plane (`mj-daemon` / `mj-controller` / `mj-session-runtime`, owning `Config`, the SQLite database, profile credentials, `UtilityLlmRuntime`, and on-device ONNX Runtime in `mj-github-relevance`) and the data plane (`mj-worker`, one process per session, local or remote bare SSH/container, owning the harness ACP bridge and durable relay journal).

Key existing files:

- `mj-core/src/config/harness.rs`: defines `HarnessKind` (all 8 harnesses), `ExecutionPolicy` (`ConfiguredApprovals` vs `Unconstrained`), `GuardianReviewer`, `ApprovalChoices`, `HarnessKind::guardian()` (line 1350), `HarnessKind::mjolnir_guardian_reviews()` (line 1363), and profile validation for `mjolnir_guardian` (lines 2223-2228).
- `mj-core/src/worker_launch.rs`: `WorkerLaunchConfig` carries `execution_policy`, `claude_permission_mode`, and `mjolnir_guardian` (lines 284-301).
- `mj-core/src/history.rs`, `mj-core/src/relay.rs`, `mj-core/src/relay/protocol.rs`, `mj-core/src/worker_protocol.rs`: define connection-only worker→daemon relay RPC (`HistoryRequests` / `CompleteHistoryRequest`) and protocol version constants (`RELAY_PROTOCOL_VERSION = 81`).
- `mj-worker/src/worker_runtime/history.rs` and `mj-worker/src/worker_runtime/unix.rs`: worker-side `HistoryEndpoint` (in-memory pending request map + oneshot channel, pruned on drop/expiry) exposed on the live relay socket.
- `mj-worker/src/acp/drive.rs` (lines 429-555), `mj-worker/src/acp/guardian.rs`, `mj-worker/src/acp/guardian/reviewer.rs`, `mj-worker/src/acp/guardian/policy.md`, `mj-worker/src/acp/guardian/policy_template.md`: current worker-side ACP permission handler and guardian-over-ACP reviewer.
- `mj-import/src/utility_llm.rs`: `UtilityLlmRuntime` resolves enabled utility-capable profiles (`Codex` → `gpt-*-luna`, `Muse` → `muse-spark-*`, `Grok` → `grok-*`, `Kimi` → `kimi-*`/`k*`, `Dsh`/DeepSeek → `deepseek-*-flash`), caches authenticated `LlmBackend` instances, ranks by quota class and family precedence, and runs `anvil_client::infer::infer_structured`. Callers today: `mj-import/src/handoff.rs` (compaction) and `mj-session-runtime/src/project_memory_merge.rs`.
- `mj-session-runtime/src/worker_client/relay.rs` and `mj-session-runtime/src/session_manager/standalone.rs` (`sync_history` lines 501-532, `follows_closely` lines 386-400): daemon session actor that polls worker-pending transient requests over the relay, runs them in an owned `JoinSet`, and delivers completions back.
- `mj-tui/src/setup/` and `docs/src/content/docs/{security.md,profiles.md}` and `mj-core/assets/skills/mj/references/configuration.md`: settings UI and user docs describing `mjolnir_guardian`.

## Plan of Work

### Milestone 1 (Track 1A): Canonical packet builder, relay protocol 82, config eligibility, and DRY utility inference

1. Create `mj-core/src/guardian.rs` (exported from `mj-core/src/lib.rs`) as the single source of truth for:
   - Vendored Codex guardian policy & template assets (moved or shared from `mj-worker/src/acp/guardian/{policy.md,policy_template.md,OPENAI_CODEX_NOTICE.txt}` so `mj-core` owns the canonical prompts without duplicating text; keep license attribution test passing!). Provide two instruction renderers: `reviewer_instructions_tool_free()` (for Stage-2 `UtilityLlmRuntime`, omitting read-file tool instructions and reinforcing Codex's conservative missing-context rule) and `reviewer_instructions_acp()` (for worker-side read-only ACP fallback).
   - Canonical action and evidence types (`GuardianReviewRequest`, `GuardianReviewEvidence`, `GuardianReviewResult`, `GuardianReviewOutcome`, `GuardianStage1Shadow`, `GuardianAssessment`, `RiskLevel`, `UserAuthorization`, `Outcome`, `parse_assessment`, `Assessment::enforced_outcome`, `assessment_json_schema`).
   - Canonical packet builder functions: `planned_action(...)` (built from ACP `RequestPermissionRequest` fields, unified diff patches via `mj_core::diff::patch_of`, cwd, harness, native child flag) and `build_compact_packet(...)` / `build_rich_scene(...)` (NEVER truncating the planned action; trimming transcript/user context strictly from the oldest end!). Also expose a deterministic constructor from raw JSON/struct inputs so offline eval fixtures pass through the EXACT same canonical formatter.
   - Constants: `GUARDIAN_REVIEW_TIMEOUT = Duration::from_secs(90)`, `MAX_PENDING_GUARDIAN_REVIEWS = 8`, `GUARDIAN_TRANSCRIPT_BYTES = (20_000 + 10_000) * 4`.
2. Add connection-only relay protocol 82 in `mj-core/src/relay.rs`, `mj-core/src/relay/protocol.rs`, and `mj-core/src/worker_protocol.rs`:
   - `pub const RELAY_GUARDIAN_REVIEW_PROTOCOL: u32 = 82;` and bump `RELAY_PROTOCOL_VERSION = RELAY_GUARDIAN_REVIEW_PROTOCOL`.
   - `RelayRequest::GuardianRequests` and `RelayRequest::CompleteGuardianRequest { result: crate::guardian::GuardianReviewResult }` with `minimum_protocol() => RELAY_GUARDIAN_REVIEW_PROTOCOL`, plus `RelayResponsePayload::GuardianRequests { requests: Vec<crate::guardian::GuardianReviewRequest> }`.
   - Add `"guardian_requests"` and `"complete_guardian_request"` to `is_served_relay_method` in `mj-core/src/worker_protocol.rs`.
3. Update `mj-core/src/config/harness.rs` (Option A — all harnesses under `ConfiguredApprovals`):
   - Keep `HarnessKind::guardian()` as metadata describing whether the harness ships a native reviewer (still useful for docs/diagnostics), OR clearly document its role, while updating `HarnessKind::mjolnir_guardian_reviews(policy, choices)` to return `policy == ExecutionPolicy::ConfiguredApprovals && choices.mjolnir_guardian` for ALL eight `HarnessKind` variants!
   - Update profile validation (`mj-core/src/config/harness.rs:2223-2228`) to allow `mjolnir_guardian = false` on any harness profile (not just Kimi/OpenCode/DSH).
   - Keep `ExecutionPolicy::Unconstrained` returning `false` so YOLO never runs Mjolnir's guardian/classifier.
   - Keep `HarnessKind::execution_enforcement` unchanged for all native guardians (`Codex` `agent`, `Claude` `auto`/`acceptEdits`/`default`, `Antigravity` `auto`, `Muse` `promptUnmatched`+`auto_review=on`, `Grok`, `Kimi`, `OpenCode` hook, `Dsh` `ask`).
4. DRY `mj-import/src/utility_llm.rs`:
   - Extract a shared structured-inference helper `infer_structured_with_failover(candidates, system_prompt, user_prompt, schema_name, schema, cancel) -> Result<(UtilityCandidateInfo, serde_json::Value)>` (or `UtilityLlmRuntime::infer_structured(...)`) that centralizes candidate failover, quota/auth error handling, cancellation, and `anvil_client::infer::infer_structured` invocation.
   - Refactor `infer_text_once` (used by `project_memory_merge.rs`) to sit cleanly on top of the same structured-inference helper (and share error classification cleanly with `UtilityCompactionBackend`).
   - Add `review_guardian_with_utility(config, request, cancel) -> GuardianUtilityOutcome` (returning `Reviewed { assessment, profile_id, model, elapsed_ms }`, `NoUtilityModel`, or `Failed { error }`): resolves candidates via `UtilityLlmRuntime::shared().resolve(config, cancel)`, sends `reviewer_instructions_tool_free()` + `request.request_text_tool_free()` with strict JSON schema matching `AssessmentPayload`, parses and clamps with `enforced_outcome()`, and logs structured `mj_guardian` info (never logging rationale/workspace text).
   - Add an offline binary/test entry point (`#[ignore]` or small tool in `mj-import` / `mj-core`) that (a) canonicalizes JSONL eval records through `mj_core::guardian` so Python Stage-1 eval uses exact Rust packets, and (b) runs `review_guardian_with_utility` against a JSONL eval input for Track 2's Stage-2 offline evaluation!

### Milestone 2 (Track 1B): Worker `GuardianEndpoint`, Daemon `sync_guardian`, all-harness ACP interception, TUI/docs, and follow-up issue

1. Worker-side `GuardianEndpoint` (`mj-worker/src/worker_runtime/guardian_endpoint.rs`, wired into `mj-worker/src/worker_runtime/unix.rs`, `mj-worker/src/relay/requests.rs`, `mj-worker/src/stand_in_relay.rs`):
   - Mirrors `HistoryEndpoint`: in-memory `BTreeMap<String, Pending>` with `request_id`, `expires`, `oneshot::Sender<GuardianReviewResult>`, `Semaphore(MAX_PENDING_GUARDIAN_REVIEWS)`, and drop-guard cleanup when the ACP permission request is cancelled or withdrawn.
   - Enqueueing a `GuardianReviewRequest` signals the worker's relay change-wait notifier (`WaitForChange`) so an attached daemon wakes immediately without polling lag.
   - Exposes `requests()` for `RelayRequest::GuardianRequests` and `complete(result)` for `RelayRequest::CompleteGuardianRequest`.
   - The live worker owns the endpoint, independently of whether a controller is connected. Disconnects leave requests in the queue under the same ID until a replacement controller answers or the deadline expires. Standalone runs with no endpoint use the local ACP reviewer. A newer controller negotiates protocol 81 on old workers and skips the new calls there.
2. Update `mj-worker/src/acp/guardian.rs` and `mj-worker/src/acp/guardian/reviewer.rs`:
   - Re-export canonical types from `mj_core::guardian` instead of duplicating policy/parsing/action logic.
   - Update `Guardian::review(...)` to:
     a) Check the per-turn denial circuit breaker first (`turn_context.guardian_tripped()`).
     b) Build `GuardianReviewRequest` via `mj_core::guardian`.
     c) If `GuardianEndpoint` is wired and daemon supports protocol 82, submit to `GuardianEndpoint` and await `GuardianReviewResult` (handling stale-authorization recheck if the person wrote a new message while waiting!).
        - `Allow` / `Deny { rationale }` / `AskPerson { reason }` settle immediately (recording denials in the per-turn breaker).
        - `UseAcpFallback` (daemon has no configured direct utility model) runs the existing worker-side `ReviewerHost` (read-only ACP session of the same harness) and settles identically.
     d) Notice `Guardian::for_launch(&spec)` now constructs `Some(Arc<Guardian>)` for ALL harnesses when `spec.harness.mjolnir_guardian_reviews(spec.execution_policy, spec.approval_choices)` is true (i.e. `ConfiguredApprovals` + `mjolnir_guardian = true`).
     e) Ensure `reviewer_environment` in `reviewer.rs` safely narrows any harness falling back to ACP (not only OpenCode — e.g., never pass session mailbox plugins/env into a Codex/Claude/Antigravity/Muse/Grok/Kimi/DSH ACP reviewer!).
3. Daemon-side `sync_guardian` in `mj-session-runtime`:
   - Add `guardian_requests()` and `complete_guardian_request(result)` to `RelayClient` in `mj-session-runtime/src/worker_client/relay.rs` (gated by `RelayRequest::GuardianRequests.supported_at(self.protocol_version)`).
   - In `StandaloneSession` (`mj-session-runtime/src/session_manager/standalone.rs`):
     - Hold `guardian_jobs: tokio::task::JoinSet<GuardianReviewResult>` and `guardian_active: BTreeSet<String>`, plus access to current `Config` (loaded/passed cleanly as `StandaloneSession` or controller already does for project-memory merge).
     - Add `!self.guardian_active.is_empty()` to `follows_closely()` so active reviews never drop to quiet cadence; also drain/select on `guardian_jobs` completion promptly so responses return immediately upon completion.
     - On `sync_guardian()`, poll `self.client.guardian_requests()`, deduplicate by `request_id`, spawn bounded background evaluation tasks (which run Stage 1 when enabled in Milestone 5, then `mj_import::utility_llm::review_guardian_with_utility`), and deliver `complete_guardian_request(result)`.
     - Because `guardian_jobs` is owned by the connection and cancelled on retire/handoff (without blocking daemon handoff admission), a restarting daemon exits immediately while the worker retains the pending `request_id` for the replacement daemon to complete.
4. Update TUI settings, tests, and docs:
   - `mj-tui/src/setup/` (make `mjolnir_guardian` toggle visible and editable on all harness profiles, updating profile setup tests).
   - Update `mj-core/src/config/tests.rs`, `mj-worker/src/acp/guardian_tests.rs`, `mj-worker/src/acp/plan_tests.rs`, and add relay round-trip + restart-preservation + utility-escalation + `UseAcpFallback` integration tests.
   - Update `docs/src/content/docs/{security.md,profiles.md}` and `mj-core/assets/skills/mj/references/configuration.md` to document all-harness interception under `ConfiguredApprovals`, utility-model Stage-2 escalation, and ACP fallback.
5. File a concise GitHub issue on `BrokkAi/mjolnir` describing generalizing `utility-over-acp` so compaction and project-memory merge can also fall back to ACP when no direct utility model is configured.

### Milestone 3 (Track 2A): Pre-scoring 176-denial audit + ML runtime & tokenizer/latency/RSS probe

1. Spawn a `subtle` audit child to blindly tag every row in `/mnt/optane/tmp/guardian-eval-20261121/data/codex_denials_for_audit.jsonl` (all 176 Codex denials) into `(a)` intrinsic danger, `(b)` authorization-dependent, `(c)` missing evidence, `(d)` artifact/duplicate, saving to `codex_denials_audited.jsonl` BEFORE any model is scored.
2. Create an isolated `uv` virtual environment at `/mnt/optane/tmp/guardian-eval-20261121/venv` (Python + `torch` cu128 for B5000 GPU scoring, `transformers`, `onnxruntime`, `optimum`, `scikit-learn`, `huggingface_hub`, `saroku==0.6.3`) without modifying system Python or the repo.
3. Inspect `saroku` 0.6.3's exact `LocalJudge` serializer and `patronus-studio/wolf-defender-jev`'s ONNX files/tokenizer.
4. Run a fast probe on `tune_10pct` canonical packets measuring:
   - Exact real-tokenizer overflow rate for `saroku-guard` (512 tok cap) and `wolf-defender-jev` (6,144 tok cap).
   - Warm CPU latency (p50/p95 at 1 thread and 4 threads) and peak process RSS for FP32 ONNX `saroku-guard` compact, `wolf-defender-jev` compact, and `wolf-defender-jev` rich.

### Milestone 4 (Track 2B): Incremental Stage-2 & Stage-1 Zero-Shot Evaluation

1. Export all corpus records (`codex_guardian.jsonl`, `claude_auto_denials.jsonl`, `real_acp_asks.jsonl`, and a hand-crafted zero-tolerance red-team set of benign-comment wrappers, standard destructive commands, and length-cap padding) through Milestone 1's canonical Rust packet builder so every offline input matches production byte-for-byte.
2. **Stage-2 tool-free `UtilityLlmRuntime` evaluation:**
   - First run on coarse slice (`tune_10pct` + `cal_10pct` denials/high-risk + ~300 approvals) to validate prompt wording and catch any false-deny spikes early.
   - Once wording is clean, run across all 176 Codex denials, all 602 high-risk approvals, ≥3,000 stratified other approvals, and all 170 real ACP asks. Report agreement overall and split by `mentions_local_inspection` (`./script.sh` / `python foo.py` false-deny check).
3. **Stage-1 zero-shot bakeoff (incremental coarse → fine → sealed test):**
   - Wording/format tuning strictly on `tune_dev` (never reported in final numbers).
   - Coarse comparison on `cal_10pct` (all 117 `cal_eval` denials + 3,046 stratified approvals) + 170 real ACP asks + 185 Claude auto-mode denials + red-team set across 5 arms:
     1. `saroku-guard` compact (512-tok cap; overflow auto-escalates)
     2. `wolf-defender-jev` compact (exact same compact packet)
     3. `wolf-defender-jev` rich (with retained user instructions + transcript tail, if CPU latency passed Milestone 3)
     4. Baseline A: Command-head allowlist (command heads never denied in training folds)
     5. Baseline B: Cross-fitted TF-IDF + logistic regression (grouped by root family)
   - Expand to `cal_30pct` / `cal_100pct` only for surviving finalists that need finer discernment, using family-grouped K-fold out-of-fold threshold calibration, chronological drift check (`2026-11-14` cutoff), and leave-one-project-out check.
   - Evaluate BOTH **Arm 1 (approve-only)** and **Arm 2 (approve + hard-deny at ≥90% precision AND absolute false-deny count)**, Stage-1 vs Stage-2 2×2 on denials, Claude safety-category recall veto, primary coverage on real ACP asks (and separately labeled "coverage if Stage 1 fronted Codex's guardian"), red-team zero-tolerance gate, and hand-read the top-100 highest-scoring approvals.
   - Freeze winning model, exact ONNX precision/tokenizer/template, and numeric thresholds in writing; open `sealed_test` ONCE and report final numbers to the Advisor and user.

### Milestone 5: Daemon-hosted Stage-1 ONNX Integration (Shadow Mode First)

1. Conditional on Milestone 4 clearing the safety, coverage, latency, and RSS gates (and Advisor/user sign-off):
   - Extract/share the dynamic `ort` 1.23.0 + `tokenizers` 0.22 engine from `mj-github-relevance/src/github_student/` so `mj-daemon` hosts the Stage-1 guardian ONNX model beside the GitHub relevance student with shared runtime initialization and clean failure isolation (any Stage-1 error/loading/overflow escalates to Stage 2).
   - Ship first in **shadow mode**: run Stage 1 on every daemon guardian review, log its score, latency, overflow flag, and hypothetical decision alongside Stage 2's verdict under `mj_guardian`, without acting on Stage 1 yet.
   - Once shadow telemetry confirms calibration on live ACP traffic, enable active Stage-1 gating with a small random background audit sample (sending e.g. 5–10% of local allows to Stage 2 asynchronously and logging disagreements).

## Concrete Steps

All repository commands run from `/home/jonathan/Projects/mjolnir/.mj/clones/90a97922992fb436fa0b2e7df5b4ab9c`. Always run `cargo test` outside the sandbox and on the dev profile (`mbx` or `cargo`).

    cargo check -p brokk-mj-core -p brokk-mj-import -p brokk-mj-worker-runtime -p brokk-mj-session-runtime
    cargo test -p brokk-mj-core guardian
    cargo test -p brokk-mj-import utility_llm
    cargo test -p brokk-mj-worker-runtime guardian
    cargo test -p brokk-mj-session-runtime guardian
    cargo clippy -p brokk-mj-core -p brokk-mj-import -p brokk-mj-worker-runtime -p brokk-mj-session-runtime -p brokk-mj-tui --all-targets -- -D warnings

Offline evaluation scripts run under `/mnt/optane/tmp/guardian-eval-20261121/` using `/mnt/optane/tmp/guardian-eval-20261121/venv/bin/python`. GPU scoring on the two RTX PRO 5000 Blackwell cards uses:

    CUDA_DEVICE_ORDER=PCI_BUS_ID CUDA_VISIBLE_DEVICES=1,3 LD_LIBRARY_PATH=/usr/lib/wsl/lib

## Validation and Acceptance

- **Milestone 1–2 acceptance:**
  - Scripted end-to-end tests prove: (1) a Codex, Claude, Antigravity, Muse, Grok, Kimi, OpenCode, or DSH session under `ConfiguredApprovals` routes non-plan ACP `request_permission` through `GuardianEndpoint` → daemon `sync_guardian` → `UtilityLlmRuntime`, returning allow-once or reject-once + transcript notice without popping a human form; (2) `Unconstrained` (YOLO) sessions never invoke Mjolnir's guardian; (3) when no utility model is configured, daemon returns `UseAcpFallback` and worker executes its read-only ACP `ReviewerHost`; (4) when the daemon disconnects mid-review and a replacement daemon connects, the pending review keeps its `request_id` and is answered by the replacement daemon without falling back to ACP; (5) legacy pre-82 daemons/workers remain wire-compatible; (6) `mjolnir_guardian = false` disables interception on any harness profile.
  - All targeted crate tests and `cargo clippy --all-targets -- -D warnings` pass cleanly.
- **Milestone 3–4 acceptance:**
  - All 176 Codex denials audited before any model scoring; exact tokenizer overflow, FP32 CPU latency, and peak RSS reported; Stage-2 tool-free `UtilityLlmRuntime` agreement measured across denials, high-risk approvals, sampled approvals, and `mentions_local_inspection` slices; Stage-1 zero-shot models and baselines compared across Arm 1 and Arm 2 with frozen sealed-test evaluation and Advisor review.
- **Milestone 5 acceptance:**
  - Daemon shadow-mode logs confirm live Stage-1 scores and Stage-2 agreement with zero regressions in daemon startup, handoff admission, or worker tests; active mode preserves Stage-2 background audit sampling and escalates on any Stage-1 unavailability/overflow.

## Idempotence and Recovery

- All offline evaluation artifacts live under `/mnt/optane/tmp/guardian-eval-20261121/` and open `mj.sqlite3` strictly read-only (`mode=ro`, `PRAGMA query_only=on`). Re-running extraction or scoring scripts is idempotent.
- Relay protocol additions are strictly additive (`RELAY_GUARDIAN_REVIEW_PROTOCOL = 82`), keeping backward compatibility with older workers and daemons.
- Stage-1 activation is gated behind shadow mode first, so any calibration surprise can be reverted or re-thresholded without losing Stage-2 safety coverage.

## Artifacts and Notes

- Run the tool from the checkout with `mbx run -p brokk-mj-import --example guardian_eval_tool -- canonicalize <in.jsonl> <out.jsonl>` for packet export. Use `stage2-utility <in.jsonl> <out.jsonl> [--concurrency N]` with the original corpus rows for structured reviews through the configured utility profiles. Completed output IDs are skipped on resume.
- Corpus root: `/mnt/optane/tmp/guardian-eval-20261121/`
  - `scripts/extract_v2.py` — single-pass read-only extractor from `mj.sqlite3`
  - `data/codex_guardian.jsonl` — 46,748 decided Codex guardian reviews (46,572 approved, 176 denied) with preceding user instructions and transcript tail
  - `data/codex_denials_for_audit.jsonl` — all 176 Codex denials enriched with approved-twin flags
  - `data/claude_auto_denials.jsonl` — 185 Claude auto-mode safety denials across 97 sessions
  - `data/real_acp_asks.jsonl` — 170 non-owned-MCP real ACP permission asks (97 Antigravity, 46 Claude, 17 Grok, 10 Codex)
  - `data/instability.json` — approved-twin and local-inspection counts
  - `data/splits.json` — deterministic root-family splits (`tune_dev`, `cal_eval` with nested 10%/30%/100% tiers, `sealed_test`, plus chronological cutoff)

## Interfaces and Dependencies

In `mj-core/src/guardian.rs`:

    pub const GUARDIAN_REVIEW_TIMEOUT: Duration = Duration::from_secs(90);
    pub const MAX_PENDING_GUARDIAN_REVIEWS: usize = 8;

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct GuardianReviewRequest {
        pub request_id: String,
        pub harness: HarnessKind,
        pub session_id: String,
        pub tool_call_id: String,
        pub action: serde_json::Value,
        pub user_instructions: Vec<String>,
        pub user_instructions_omitted: bool,
        pub transcript: String,
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
    pub enum GuardianReviewOutcome {
        Allow { stage: String, stage1_score: Option<f64> },
        Deny { rationale: String, risk_level: RiskLevel, user_authorization: UserAuthorization, stage: String, stage1_score: Option<f64> },
        AskPerson { reason: String },
        UseAcpFallback { stage1_score: Option<f64> },
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct GuardianReviewResult {
        pub request_id: String,
        pub decision: GuardianReviewOutcome,
    }

    pub fn planned_action(...) -> serde_json::Value
    pub fn reviewer_instructions_tool_free() -> String
    pub fn reviewer_instructions_acp() -> String
    pub fn assessment_json_schema() -> serde_json::Value
    pub fn parse_assessment(text: &str) -> anyhow::Result<Assessment>

In `mj-core/src/relay.rs` and `mj-core/src/relay/protocol.rs`:

    pub const RELAY_GUARDIAN_REVIEW_PROTOCOL: u32 = 82;
    // RelayRequest::GuardianRequests
    // RelayRequest::CompleteGuardianRequest { result: crate::guardian::GuardianReviewResult }
    // RelayResponsePayload::GuardianRequests { requests: Vec<crate::guardian::GuardianReviewRequest> }

In `mj-import/src/utility_llm.rs`:

    pub async fn infer_structured_with_failover(
        candidates: &[UtilityCandidate],
        system_prompt: &str,
        user_prompt: String,
        schema_name: &str,
        schema: serde_json::Value,
        cancel: CancellationToken,
    ) -> Result<(String, String, serde_json::Value)>

    pub async fn review_guardian_with_utility(
        config: &Config,
        request: &mj_core::guardian::GuardianReviewRequest,
        cancel: CancellationToken,
    ) -> GuardianUtilityOutcome
