# Ship the local GitHub relevance student

This ExecPlan is maintained according to `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current. Repository root is `/home/jonathan/Projects/mjolnir/.mj/clones/fb267460978a078517f68e60c750a1c2`.

## Purpose / Big Picture


GitHub notifications should reach sessions that need them while removing almost all remote classification calls. The daemon (the background process that owns sessions and the database) will score each new GitHub item against each eligible session locally. Only uncertain scores go to GLiDE, the remote probability classifier. A user sees the existing GitHub mailbox notice; diagnostics explain which route decided it and whether local inference is degraded. The accepted operating point estimates 0.901 pooled recall, with 16.2% of pairs sent to GLiDE. These are estimates, not guarantees.

This plan combines #1505, the required history evidence and GLiDE route, with #1513, the student and three-band policy. Implement #1505 first. This planning assignment does not authorize product edits, commits, publication, or installing a build. Implementation begins after parent review.

## Progress


- [x] (2026-11-12) Read `.agents/PLANS.md`, both issues including comments, the frozen aliases600 specification, checkpoint configuration, Python serialization, and current production route.
- [x] (2026-11-12) Create CPU-only scratch inference prototypes outside the repository; record measurements in Artifacts and Notes.
- [ ] Parent accepts stack and degraded-operation recommendations.
- [x] Milestone 1: port and prove aliases600 evidence parity (2026-11-12; 3f67bdc17, 475affced, 96442a7b3, 21804154f; all 16 fixtures byte-identical, student_segments matches Python json.dumps).
- [x] Milestone 2: deploy and prove the GLiDE GitHub boundary (2026-11-12; 38edf31b0, proxy 49/49; deployed as Cloudflare version aa76f56c, live route answers 400 to an invalid request).
- [x] Milestone 3: implement pinned assets and local scoring (2026-11-12; 0595261, b6b093d; Rust CPU parity 50/50 with 16-way concurrency bit-identical to serial; Rust CoreML on the Mac test host 50/50, max 0.00145, warm 1.76 s vs CPU 13.9 s; HF pin e4e47921).
- [x] Milestone 4: integrate the frozen policy, failure notices and lazy preparation (2026-11-12; 0d4d0c6c5 through c5d89828f; full validation: workspace clippy, core/transcript/controller/client suites, fmt, proxy tests all pass).
- [x] Hardware qualifications (2026-11-12): Rust CUDA on a rented RTX 3070 passed (50/50, max 0.00188); Rust CoreML on the Mac passed; DirectML verified from Python only (Rust run skipped for v1 at Jonathan's direction). See Outcomes.
- [ ] Isolated end-to-end daemon run and installation: Jonathan installs later.
- [ ] Complete final validation, commit and push implementation under repository rules; publish weights only through Jonathan's separate upload.

## Surprises & Discoveries


Candle 0.11 supports ModernBERT but cannot consume this checkpoint correctly without adaptation. Its classifier returns softmax; a single-label softmax is always one, whereas this student requires sigmoid of its single logit. Its layer norms omit learned biases, present in this checkpoint. Its config expects top-level global/local rotary theta and string-valued label2id, while the checkpoint uses rope_parameters and numeric label2id. Scratch code loads the backbone, preserves biases, and supplies the actual head. Do not interpret stock classifier output as parity.

The saved training format has `token-budget: null`, `max_length: 7999`, empty hypothesis prefix, `segment-order: item-session`, and `head-type: rerank`. The balanced-pair branch in common.py is irrelevant to this checkpoint. History's 800-token budget uses the separately pinned Ettin400M tokenizer. Do not substitute a character estimate or confuse that budget with the student's total context limit.

`.github/workflows/release.yml` builds static musl binaries only for the dedicated worker; Linux CLI/daemon uses a glibc baseline through zigbuild. The controller owns inference; adding its native dependencies must not infect mj-core, mj-transcript, or mj-worker.

## Decision Log


- Decision: Preserve the user-frozen thresholds in one controller policy module: notify at s >= 0.2830; reject at s < 0.0716; otherwise call GLiDE and notify at p >= 0.1123. Cite #1513 and `labels/panel/band-point90.json`, accepted 2026-11-12, beside the constants. Do not retune for a different runtime or quantization.
  Rationale: This operating point is already accepted; inference is a parity problem.
  Date/Author: 2026-11-12 / Jonathan; recorded by planning agent.
- Decision: Recommend fp32 assets initially; prefer the runtime with demonstrated parity and reasonable CPU latency. Quantized assets require the same score acceptance gate and cannot justify threshold changes.
  Rationale: Five pairs per minute leaves room for ordinary CPU inference; quantization changes decisions near the bands.
  Date/Author: 2026-11-12 / planning agent; see measured qualification below.
- Decision: On model download/load/inference failure, send each affected pair to GLiDE using the exact same aliases600 evidence and 0.1123 threshold. Report degradation and recovery visibly; never silently revert to Jev or store a false verdict for a failed request.
  Rationale: This is a deliberate available mode of the chosen classifier, keeps notifications working during first-use downloads and repair, and exposes the local failure. If GLiDE also fails, preserve existing durable per-pair retry handling.
  Date/Author: 2026-11-12 / planning agent recommendation for parent acceptance.
- Decision: Accepted: ONNX Runtime through `ort` with fp32 ONNX assets, bundled per supported native platform; int8 stays out (21 of 50 band changes). Accepted: the degraded GLiDE mode above.
  Rationale: Only measured runtime with parity and acceptable latency; Candle was 5-7x slower at these lengths.
  Date/Author: 2026-11-12 / parent.
- Decision: Parallelize across pairs, not within one pair. Each inference uses one intra-op thread, and the owner runs as many pairs concurrently as the host's available parallelism reports (the real limit, read at startup, not a guessed cap). Jonathan asked to "let it parallelize".
  Rationale: Measured intra-op scaling flattens past 4 threads (4.2 s at 4 threads, 4.0 s at 8, 11 s at 1 for a median pair), so one thread per pair gives the most pairs per core-second; a 50-session fan-out drains across all cores instead of queueing behind one 4-thread inference. Sustained cost at about 5 pairs per minute is roughly one core on average.
  Date/Author: 2026-11-12 / Jonathan, recorded by parent.
- Decision: Score parity is judged against fresh fp32 Python scoring of the same checkpoint (score.py on CPU, fp32), with maximum absolute difference <= 1e-4 and identical three-band routing. The stored predictions in vast/student3/results were produced on GPU in bf16 and differ from fp32 by up to about 0.0085; compare against them only for band routing (expected identical on the parity set, with near-threshold cases listed), never for the 1e-4 tolerance. Expected values come from the Python reference, never from the Rust implementation.
  Rationale: The planning benchmark showed Rust ort matches fresh fp32 Python within 2e-7, while stored bf16 scores differ from both.
  Date/Author: 2026-11-12 / parent.
- Decision: CPU assets store fp16 weights and compute in fp32 (cast once at load), halving the download to about 286 MB at fp32 speed; parity gate against fresh fp32 Python is max abs <= 1e-3 with identical band routing. int8 rejected: only 7-10% faster on this host while shifting scores by 0.08 on average.
  Date/Author: 2026-11-12 / Jonathan.
- Decision: Add an fp16 GPU path that is used automatically: CoreML on Apple silicon, and NVIDIA through ONNX Runtime's CUDA provider when that provider loads cleanly. One user setting turns GPU use off (CPU only). If the GPU path fails to load or run, fall back to CPU with a visible notice. Same parity gate. CoreML parity is validated on the Mac test host. Windows DirectML is under evaluation, not yet decided.
  Rationale: Users get the speed without configuration; the off switch covers hosts where GPU use is unwanted or unsafe (Jonathan's WSL dev host has crashed on GPU work).
  Date/Author: 2026-11-12 / Jonathan (superseding the earlier opt-in proposal).
- Decision: The off switch is `[github.watch] use_gpu` (default true; false = CPU only), in mj-core/src/config/github.rs with its configuration reference and Settings entry. The preparation owner decides the device once; callers ask it.
  Date/Author: 2026-11-12 / parent.
- Decision: GPU parity validation, required before the GPU path ships: CoreML on the Apple-silicon Mac test host, and CUDA on a rented NVIDIA host (the local GPU is off limits). Both use the model owner's Phase 1 parity command against fresh fp32 Python, max abs <= 1e-3 and identical routing, and record provider placement (which kernels ran on the GPU). The parent runs both. DirectML, if adopted, is validated on Jonathan's Windows machine (Intel UHD 770 and NVIDIA T1000).
  Date/Author: 2026-11-12 / parent.
- Decision: Score parity gate for every non-fp32 path (fp16 storage on CPU, fp16 compute on GPU): max abs difference <= 5e-3 against fresh fp32 Python, and identical band routing except for pairs whose fp32 reference lies within 5e-3 of a cut (listed, not failing). Replaces the earlier 1e-3 figure: fp16 storage measured 0.00145 on one pair, and the thresholds were tuned on bf16 scores that differ from fp32 by up to 0.0085.
  Date/Author: 2026-11-12 / parent.
- Decision: DirectML is adopted on Windows. Device order there: CUDA if it loads cleanly, then DirectML, then CPU, all through the preparation owner, the use_gpu switch, and the rule that a GPU must beat the CPU's measured per-pair latency on the first real pairs or the owner switches to CPU with a notice. Package a DirectML-enabled onnxruntime.dll and a pinned DirectML.dll with its license notice.
  Rationale: Jonathan's T1000 test showed the code runs; its 30 s per pair came from VRAM already taken by the desktop, which the latency rule handles. Correctness is validated on that T1000 against the parity gate.
  Date/Author: 2026-11-12 / Jonathan (superseding the parent's earlier rejection).
- Decision: Pin ONNX Runtime 1.23.0 on every platform (the DirectML NuGet package exists for 1.23.0 but not 1.23.2; 1.24+ dropped macOS x64). GPU providers in this change: DirectML on Windows (covers NVIDIA, AMD, Intel) and CoreML on macOS. Linux stays CPU-only: the official Linux CPU library is built without CUDA and the CUDA build is a different 240 MB main library needing host CUDA/cuDNN, so Linux CUDA is deferred to a follow-up.
  Date/Author: 2026-11-12 / parent.
- Decision: The leaner graph (sliding-window local attention, 32-query chunked global attention, standard ONNX ops) replaces the first export: p95 transient memory 3.91 GB -> 376 MiB and about 31 s -> 14.5 s single-threaded, same weights file. The GPU graph gets the same structure with mixed precision (fp16 matmuls; softmax, layer norm, GELU and reductions in fp32) after the first fp16 GPU graph failed DirectML parity on the T1000 (max error 0.155, 7 of 50 routing changes, several far from a cut).
  Date/Author: 2026-11-12 / parent.
- Decision: One graph for every device: the rank-3 lean all-fp32 graph (attention tensors kept at <= 4-D for DirectML) with fp16-stored weights. On the T1000 with DirectML (ORT 1.23.0, normal optimizer, memory pattern off, sequential): max abs 0.00143, 50/50 routing, median 2.36 s and max 5.58 s per pair, faster than CPU. The mixed fp16 graph was slower (5.38 s median) and failed (max 0.0075), so it is dropped. Earlier failures: the generic fp16 converter cast RoPE positions to fp16; DirectML rejected 5-D attention MatMuls.
  Date/Author: 2026-11-12 / parent.
- Decision: The ONNX Runtime native library is a pinned downloaded asset, fetched, verified and atomically cached by the preparation owner like the weights (official upstream release archives and NuGet packages, pinned by URL, size and SHA-256; license texts cached beside them). It is not bundled in packages and no system copy is ever loaded. One mechanism covers fresh installs, old updaters that only stage executables, npm/brew installs and source builds; this supersedes the plan's bundling section.
  Date/Author: 2026-11-12 / parent.
- Decision (supersedes the CoreML deferral below): CoreML ships on Apple silicon, because Jonathan requires Mac acceleration. Two static-shape CoreML graphs (6144 and 8192 token buckets; static slices, additive masks, chunk-256 global queries; 3 CoreML partitions instead of 54) share weights.fp16.data, registered as external initializers because ORT 1.23's CoreML EP fails on file-backed external initializers. Mac parity: max 0.00145, 50/50; median 1.6 s (6144) vs 15.0 s CPU single-threaded and 5.5 s with 4 threads. Published as HF revision e4e47921640a6005e5059e5b04e4264b774c6866. Device order: macOS arm64 CoreML then CPU; Windows DirectML then CPU; Linux CPU.
  Date/Author: 2026-11-12 / Jonathan, recorded by parent.
- Decision (supersedes the CUDA deferral): Linux CUDA is in v1, with the documented requirement that users install the CUDA 12 and cuDNN 9 libraries themselves. Before the first ORT load, the owner chooses the official Linux GPU ORT build (pinned, downloaded and verified like the CPU build) only when use_gpu is on, an NVIDIA driver is present and the CUDA provider's required sonames load; otherwise the CPU build. A missing-library case emits one typed notice naming the missing libraries, effective after the next daemon start (the library choice is process-global). The GPU build carries the CPU EP, so latency fallback and set_use_gpu(false) stay in-process.
  Date/Author: 2026-11-12 / Jonathan, recorded by parent.
- Decision: CUDA is verified but still deferred for packaging: model.onnx on an RTX 3060 with onnxruntime-gpu 1.23.0 passes (max 0.00188, 50/50, median 0.38 s per pair).
  Date/Author: 2026-11-12 / parent.
- Decision: Per-pair inference errors do not reload the engine: the pair goes to GLiDE, and the owner reports one typed notice per failing episode and one on recovery. Load and verification failures enter Failed with backoff. `set_use_gpu` is an owner state transition called on configuration reload. The GPU-vs-CPU probe discards one warm-up run before timing.
  Date/Author: 2026-11-12 / parent.
- Decision: CoreML is deferred; Macs run the graph on CPU. On the Mac test host (arm64, macOS 27.0.1, ORT 1.23.0) CoreML MLProgram fails at session creation (error -7) even CPU-only, and NeuralNetwork loads but is killed during the first inference after splitting into hundreds of partitions. Mac CPU passes parity (max 0.0014, 50/50) at about 16 s per pair single-threaded. The only GPU provider in this change is DirectML on Windows.
  Date/Author: 2026-11-12 / parent.
- Decision: Intel Macs: pin one ONNX Runtime release with official assets for every shipped platform if one exists; otherwise Intel Macs use the degraded GLiDE route with a one-time notice. No source builds of ONNX Runtime in CI.
  Date/Author: 2026-11-12 / parent.
- Decision: Comparison against the stored bf16 predictions is informational only: near-threshold pairs may change band between bf16 and fp32 (observed: dev bifrost-dev#11338, 0.07056 vs 0.07173 at the 0.0716 cut). List such flips with their distance to the cut; they do not fail the gate.
  Date/Author: 2026-11-12 / parent.
- Decision: The parent exports and publishes the assets (fp32 ONNX, tokenizer, config) to a public repository in the brokkai Hugging Face organisation before Milestone 3 pins them; the model owner supplies the reproducible export script and hashes.
  Date/Author: 2026-11-12 / parent.

## Outcomes & Retrospective


Shipped on master (2026-11-12): the GitHub item-relevance route no longer asks Jev. A local Ettin 150M student scores every item–session pair, and GLiDE decides only the middle band (notify at s >= 0.2830, reject below 0.0716, otherwise GLiDE at p >= 0.1123). The new proxy route `/v1/github-item-glide` is deployed. Installation is pending Jonathan.

Hardware qualification, all against fresh fp32 Python references on the same 50 real pairs:

| Device | Path | Max error | Routing | Speed |
|---|---|---|---|---|
| Linux CPU | Rust | 0.00145 | 50/50 | 16 concurrent runs bit-identical to serial |
| macOS CoreML (Mac test host) | Rust | 0.00145 | 50/50 | 1.76 s warm vs 13.9 s CPU |
| Linux CUDA (rented RTX 3070, CUDA 12.1, cuDNN 9) | Rust | 0.00188 | 50/50 | 0.32 s warm vs 25.3 s CPU |
| Windows DirectML (T1000) | Python only | 0.00143 | 50/50 | 2.4 s median |

The Rust DirectML run was skipped for v1 at Jonathan's direction; the measured GPU-vs-CPU rule and failure fallback still apply there. The isolated end-to-end daemon run was not performed before the push; Jonathan installs later.

Lessons:
- The first full-attention export used 3.9 GB per long pair; true sliding-window and chunked global attention cut that to about 460 MB with no loss of parity.
- The generic fp16 converter silently cast RoPE position indices to fp16, which broke long inputs on GPU; one fp32 graph with fp16-stored weights turned out simpler and faster than any mixed-precision graph.
- Each GPU provider needed a different accommodation: DirectML rejects attention tensors above 4-D, CoreML needs static shapes and in-memory external initializers, and CUDA on Linux needs a separate ONNX Runtime build that users enable by installing CUDA 12 and cuDNN 9.
- Thresholds were tuned on bf16 GPU scores, so parity is judged against fresh fp32 Python with a 5e-3 tolerance and near-cut exceptions, not against the stored bf16 predictions.

## Context and Orientation


`mj-controller/src/github_watch/service.rs` defines the `GithubClassifier` interface and `JevClassifier` adapter. `run_with_classifier` supervises repository polling. `classify_recorded_items` loads unclassified stored items, builds one digest per session, and runs per-pair classification futures. `session_digest` currently uses spawn_blocking to load only three recent turns via `load_materialized_recent_turns`, then calls `mj_transcript::github_item::session_digest` in `mj-transcript/src/github_item.rs`. That digest extracts conversation, files, references and a private mention index. `classify_for_session` builds item-first `GithubItemEvidence`, applies `SessionContext::for_item` from `mj-core/src/github_item.rs` to select mentions with repository-sensitive bare references, and bypasses classification for the session that created the item. Preserve this bypass and its account-author check.

The adapter calls `mj-controller/src/github_item_verdict.rs::classify`. This bounds evidence, looks up the Jev key on a blocking task, constructs `mj_core::jev::request_body`, and calls either TypeSafe directly or `/v2/github-item-verdict` on the proxy. It writes request/answer diagnostics via DecisionLog, parses a boolean verdict and returns it. Today the hosted proxy in `services/jev-proxy/src/index.ts` and `github-item.ts` validates v2 evidence and calls Jev. GLiDE already has a shared transport in `services/jev-proxy/src/glide.ts` using `https://api.fastino.ai/v1/systemone`, model `fastino/GLiDE`, `store:false`, and bundled server-owned questions.

`classify_for_session` prepares a deduplicated `MailboxEventBody::NewGithubItem` only for interested sessions, then calls `mj-controller/src/database/github_watch.rs::commit_github_item_classification` on a blocking task. That transaction inserts `github_watch_classifications`, removes retry failures, establishes the relevant watch and enqueues the mailbox event atomically. Continue storing the final boolean using this transaction. Numeric scores and model identity belong in diagnostics for this change; no schema migration is needed. Failed inference plus failed GLiDE must return an error before this commit, allowing existing `github_watch_classification_failures` backoff to retry.

Existing tests include `watcher_records_pages_and_retries_items_without_blocking_other_work`, `watcher_classification_backoff_does_not_starve_healthy_sessions`, `classification_uses_opening_tool_calls_and_the_credential_author`, and `watcher_stops_classification_and_requests_when_mailboxes_are_disabled` in service.rs. Existing database integration covers transcript windows and creation output. `mj-transcript/src/github_item.rs` tests UTF8 truncation, turn boundaries and peer relationships. `mj-core/src/github_item.rs` tests bounded serialization and typed v2 answers. Proxy fixtures `services/jev-proxy/tests/fixtures/github-item-controls{,-v2}.json` and `tests/github-item.test.ts` cover the current HTTP contract; these are not an existing final-score golden. Do not describe the old helper tests as proof of the new route.

## Plan of Work


### Milestone 1: exact aliases600 evidence (#1505)


Assign one evidence owner exclusive edits to `mj-core/src/github_item.rs`, new `mj-core/src/github_item/history_unicode.rs` and the pinned evidence tokenizer asset under that directory, `mj-transcript/src/github_item.rs`, new `mj-transcript/src/github_history.rs`, `mj-transcript/src/lib.rs`, new transcript fixtures under `mj-transcript/tests/fixtures/github-history/`, and `mj-controller/src/database/github_watch.rs`. Reserve all service.rs integration edits for Milestone 4. The evidence owner supplies callable APIs without changing the polling route yet.

Copy the 16 frozen fixtures from `/mnt/optane/jev-github-firstcut-20261110/labels/history-snippets/aliases600/out/rust-port-fixtures.jsonl.gz`, tokenizer and Python Unicode property ranges with their provenance hashes. Port the entire `RUST-PORT.md` contract, not an approximation of snippets. The port accepts the ORIGINAL untruncated title/body and a snapshot of materialized transcript rows. Cut history at item creation, retaining only rows whose creation AND last change are at or before cutoff. Order by position then stable_id, omit the same-item mailbox notice, and render the exact dossier header, counts, UTC millisecond timestamps, role text and full tool call JSON. Normalize CRLF and lone CR. Keep the existing base digest and item-specific mentions.

Identifier extraction preserves Python insertion and overwrite order; numeric/path/symbol/test/acronym passes use the frozen Unicode categories. Windows use Unicode character offsets, a 250-character grid, 500-character width, rarity-weighted matches and a slight recency boost. Apply the frozen ASCII-run redactor, literal novelty checks against Python default-spaced digest JSON, deterministic score ordering, non-overlap and the 2,100-character selection budget. Retain raw span offsets and f64 score bits for fixture diagnostics. Select timestamps with the permissive reference regex; redact timestamp strings too. These details intentionally include reference quirks.

Canonical session key order is conversation, files, references, mentions when present, other keys in Unicode order, history last. History is chronological objects with time then text. Use compact UTF8 JSON without ASCII escaping for final provider/student input. Enforce history-array <=800 tokens and full pair <=7999 using the pinned Ettin400M tokenizer without padding/truncation. Remove the lowest-scoring selected snippet, earliest on ties, until both fit. Never refill or trim base evidence to satisfy history's budget. Empty-history overflow must be reported; preserve the base so the student loader applies its saved session-only truncation rule. Avoid accidentally bounding the item body twice.

Expose a transcript rendering/retrieval operation and a database snapshot loader usable from spawn_blocking. Do not cache an unrestricted entire history indefinitely in daemon memory; load one snapshot per session per poll and share it among the pairs being processed, releasing it after those pairs finish. Acceptance: all 16 fixtures match lexical snippets, budgeted history bytes and final evidence bytes; timestamp cutoff and Unicode adversarial cases fail before the port and pass after. These are integration tests between the Python-trained format and Rust, not hand-built happy-path unit examples. Run focused `cargo test -p brokk-mj-transcript github_history` and affected database transcript tests outside the restricted sandbox.

### Milestone 2: GLiDE HTTP boundary (#1505)


A second owner exclusively edits `services/jev-proxy/src/github-item.ts`, `services/jev-proxy/src/index.ts`, new `services/jev-proxy/tests/fixtures/github-item-aliases600.json`, `services/jev-proxy/tests/github-item.test.ts`, and a new bundled question file `mj-core/src/github_item/questions_glide.json`. Use the frozen `aliases600/question.json` exactly, not the earlier Jev question text. Add an explicitly versioned GitHub GLiDE route accepting history; retain deployed older routes for older clients. Validate state and return the finite probability, not a 0.5-thresholded boolean. Reuse glide.ts. No Jev request should occur on the new route.

The controller boundary owner exclusively edits `mj-controller/src/github_item_verdict.rs` and new `mj-controller/src/github_item_glide.rs`. Preserve existing interfaces until Milestone 4. Supply `classify_probability(&GithubItemEvidence) -> Result<f64>` asynchronously, using hosted GLiDE and the existing bounded HTTP transport. Direct Jev credentials must not choose another relevance algorithm. Match the proxy's authentication and error handling; never require users to provision a new key to retain hosted notifications.

Extend existing proxy HTTP integration fixtures with real aliases600 evidence, malformed probabilities and provider errors; assert full emitted provider request and returned response against checked-in expected JSON. From `services/jev-proxy`, inspect package.json and run its existing test script (`npm test`). Add a Rust HTTP boundary integration test for probability decoding, including malformed/nonfinite/out-of-range results and exact evidence serialization. No external provider credentials belong in fixtures. Deploy the additive proxy route through the authorized normal workflow before shipping a daemon that requires it. A live optional recorded request can prove provider compatibility; do not declare a local fake-server test end-to-end.

### Milestone 3: assets and inference (#1513)


The model owner exclusively edits new `mj-controller/src/github_student.rs`, `mj-controller/src/github_student/assets.rs`, `mj-controller/src/github_student/inference.rs`, `mj-controller/src/github_student/manifest.json`, real-input score fixtures under `mj-controller/tests/fixtures/github-student/`, and controller Cargo.toml. A designated integrator owns root Cargo.lock and `mj-controller/src/lib.rs` to avoid concurrent edits. Keep inference dependencies confined to the controller; do not introduce a workspace crate for organization.

The manifest pins Jonathan's public BrokkAi Hugging Face repository, immutable revision, artifact sizes for diagnostics, SHA256 of config/tokenizer/weights and any ONNX external data. Download to unique temporary siblings in the mj data directory, hash before use, then atomically publish the verified revision directory. Existing verified content is reused. Interrupted downloads cannot become valid cache entries. Serialize preparation through one model owner; all callers observe that owner's Loading, Ready or Failed(next retry) state rather than independently checking files. Asset fetching is asynchronous supervised work; hashing, tokenization, loading and inference use supervised blocking work. Downloading must not block pair classification: Loading selects GLiDE. Do not hold daemon handoff admission across model download or uninterruptible inference. Preparation can restart under the next daemon, and uncommitted pairs remain durably discoverable. Observe/report task failures and cancel owned downloads at shutdown.

Expose `score(&GithubItemEvidence) -> Result<f64>` behind the owner. Serialize bounded item and canonical session compactly exactly as common.py; use the checkpoint tokenizer pair postprocessor, item first, session second, padding only as required, truncation only_second, maximum 7999. The checkpoint has no balanced token-budget branch and no hypothesis prefix. Evaluate CLS pooling, dense head, GELU, layer norm including biases, linear classifier and sigmoid. Reject nonfinite output as failure. Retain fp32 until quantization passes the same gate. Keep the model loaded across pairs.

Before promotion, run a real-format integration parity test on at least 50 identity-matched pairs spanning both predictions-150m-mixed-dev.jsonl and predictions-150m-mixed-new.jsonl. Include lengths near median and p95, history removal, Unicode and overflow. Compare input bytes, token IDs, masks and scores in that order, diagnosing the first divergence. Require maximum absolute probability difference <=1e-4 for fp32 and identical three-band routing for these pairs; inspect all near-threshold cases explicitly. Quantized results must meet these same requirements or remain experimental. Store expected scores from the frozen files, never regenerate expectations using the Rust implementation. Large weights stay in the pinned cache; the explicit model integration test reports missing assets rather than silently passing. Routine tests use checked-in recorded provider responses where appropriate.

Add asset integration cases against a local HTTP server: interrupted download followed by restart, bad digest, concurrent callers observing a single preparation, and loader failure followed by recovery. These protect partial failure and ownership. Run focused controller github_student tests outside the sandbox.

### Milestone 4: policy and user-visible degraded operation


One integrator exclusively owns `mj-controller/src/github_watch/service.rs`, new `mj-controller/src/github_relevance.rs`, `mj-controller/src/lib.rs`, root Cargo.lock, and final validation artifacts. This is the only milestone that switches production routing. Replace the Jev adapter with the student/GLiDE classifier, integrate the evidence snapshot from Milestone 1, and preserve the creator bypass, PollControl checks and atomic database/mailbox transaction.

The policy module owns the three thresholds and explicit routes StudentNotify, StudentReject, GlideBand and GlideDegraded. It returns the existing boolean verdict to the watcher; diagnostics additionally include student probability, GLiDE probability when used, manifest revision, route, elapsed time and failure context. Reuse existing diagnostic logging conventions without claiming the model is Jev. No provider-dependent recomputation of the threshold is allowed.

If the model cannot download or load, immediately classify affected pairs through GLiDE. Report one visible degradation notice per failure episode using the existing daemon notice surface, with the cause and automatic-retry status; report recovery once. Reuse existing retry scheduling conventions for preparation with increasing delay and retry-after when supplied, and record next retry in the owner. A persistent failure must not trigger a new download on every pair. Do not add an invented throughput cap or admission queue; benchmark thread count is a performance setting, not a queue timeout. If GLiDE fails too, return the error to durable watcher retry handling. Never commit an uninterested result as an error fallback. Do not reclassify already committed pairs when the model later recovers.

Extend existing watcher integration tests for atomic notification/retry behavior and the creator bypass. Add recorded model/provider integration cases covering each route and exact boundaries, concurrent first use, malformed output, model unavailable with GLiDE succeeding, both unavailable, cancellation while downloading, and shutdown/handoff while work is outstanding. Use real recorded scores where possible; boundary cases protect numerical ordering rather than ordinary happy paths. Add a whole rendered diagnostic/notice golden through the daemon API or existing user-facing diagnostics, not a golden of an internal helper string. Keep tests isolated with their own MJ_DATA_DIR and configuration.

For a final end-to-end demonstration, run a real newly built binary and daemon under `--instance github-student-e2e`, a real worker with a recorded harness, pinned model assets, recorded GitHub HTTP inputs and recorded GLiDE responses. Observe the existing mailbox notification for positive items, none for negatives/creator, a GLiDE request only for band/degraded routes, a visible download-failure notice and subsequent recovery. Exercise daemon replacement during preparation; it must return promptly and resume pending classification after restart. Stop only this test instance and remove its owned files after its processes terminate.

## Concrete Steps


All repository commands run from the root named above unless explicitly stated otherwise. Run every cargo test outside the restricted sandbox. Use ordinary mbx-managed Cargo storage; do not change target directories. During implementation run focused tests for changed modules after each round, fixing and rerunning only failures. After the last edits run once:

    cargo fmt --all -- --check
    cargo test -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-controller
    cargo clippy --all-targets -- -D warnings

Model integration tests additionally require the verified cache and should be explicitly selected if ignored to avoid network downloads in ordinary unit suites. Record their exact command and cache manifest here after implementation. All new binary/daemon invocations must include `--instance github-student-e2e`; never use the live default instance. Dispatch `nightly.yml` on master after the platform-sensitive implementation lands, checking macOS and Windows. Verify the worker musl build retains its existing dependency boundary. Do not run release validation or install during planning.

## Validation and Acceptance


Success means the real evidence fixtures match byte for byte, the model scores match reference within the stated tolerance, the exact frozen bands select the correct route, and the existing atomic notification transaction is unchanged. First use and failed downloads must leave polling and shutdown responsive. Existing committed classifications must remain readable without migration. The new proxy route must call GLiDE only. Final diagnostics must visibly identify degraded mode and model recovery. Treat a missing publication revision, checksum, model integration run, or supported-platform runtime as incomplete work, not a passing result.

## Idempotence and Recovery


Repeated startup reuses verified revision content and restarts interrupted preparation safely. Never modify the published manifest in place to disguise a different artifact. A bad cache is quarantined or removed by the model owner before retry; no reader can mmap it until verification completes. Retrying failed pairs uses the existing durable watcher keys and transaction, avoiding duplicate mailbox delivery. A daemon crash loses only rebuildable inference memory; workers and pending accepted turns remain independent. Leave existing classifications untouched on rollback; stop routing to the new classifier by reverting the implementation, not by resetting user data.

## Interfaces and Dependencies


The evidence owner supplies `build_history(original_item, bounded_session, transcript_snapshot)` with ordered history plus parity diagnostics, and a database snapshot loader. The GLiDE owner supplies async `classify_probability(&GithubItemEvidence) -> anyhow::Result<f64>`. The model owner supplies a daemon-owned Student handle with async preparation/status and background scoring. The policy integrator supplies async `classify(&GithubItemEvidence) -> anyhow::Result<GithubItemVerdict>` implementing the existing GithubClassifier adapter. Exact ownership names may follow nearby conventions, but one owner must decide readiness and retries.

Use the Rust tokenizers library to reproduce the published tokenizer. If choosing ONNX Runtime, bind it only in the controller through ort and explicitly package the pinned native runtime for Linux glibc, macOS and Windows; ort is a binding, not a pure Rust inference engine. Dynamic loading reduces binary link impact but does not remove distribution, integrity and license obligations. Do not silently use an arbitrary system library. If choosing Candle, preserve the checkpoint's architecture differences above and upstream or narrowly maintain the required correction; stock ModernBERT classifier is insufficient.

## Artifacts and Notes


Benchmark scratch root: `/mnt/optane/jev-github-firstcut-20261110/deploy-bench/`. It contains bench.py, short.py, rest.py, src/main.rs, the inspected modernbert.rs, generated token IDs, timing outputs and build logs. Source references: Candle ModernBERT at https://github.com/huggingface/candle/blob/0.11.0/candle-transformers/src/models/modernbert.rs and ort API at https://docs.rs/ort/latest/ort/ (2.0.0-rc.15 at inspection). CPU: AMD Ryzen Threadripper PRO 9985WX under WSL, 120 exposed logical CPUs. Runs use nice 10 with 1/4/8 configured inference threads; single observations are not stable latency percentiles. Timing inputs are actual pairs with 3,415 and 5,002 tokens. The scratch release build is for measuring inference and is not product correctness validation.

Measurement results and final stack recommendation are appended below after the bounded experiment completes.

Plan authored 2026-11-12; records the frozen user design, researched ownership boundaries and concrete acceptance criteria. No product implementation or commit performed.

### Distribution work reserved to the model owner


If ONNX is selected, the model owner also owns `.github/workflows/release.yml`, `scripts/install.sh`, and the existing platform package/installer inputs reached from those files. Inventory those inputs before editing and list the resolved paths here; tell the integrator before crossing into another assigned file. Bundle a pinned, checksum-verified CPU ONNX Runtime alongside the daemon on each supported native platform and ensure installation/upgrades retain it. Include its supplemental license notice through the existing release-notice mechanism. Select an exact compatible ort version and runtime API version together. Linux worker musl artifacts continue to contain only the dedicated worker and need no runtime. Check macOS signing/package paths and Windows DLL discovery through actual nightly builds. The measured Linux .so is not proof of those platforms. A local runtime loaded through an explicit verified install path must work without LD_LIBRARY_PATH or a user-installed Python environment. These packaging edits and smoke tests are part of Milestone 3 acceptance, not optional follow-up work.

### Important parity distinction


The first fresh CPU Python fp32 reference score was 0.2006157935, while the supplied stored prediction for that identity was 0.1980641782. Therefore a 1e-4 test directly against every stored prediction is not yet justified. Keep the frozen stored values and report their differences, but establish the reference execution precision before choosing the stored-score tolerance. The implementation gate remains <=1e-4 against Python with the SAME fp32 execution semantics, plus a separate comparison to stored predictions and explicit inspection of band changes. Do not loosen a test until the cause is demonstrated; training's sweep invokes bf16, which is a lead to investigate, not proof of why every stored difference occurs. Parent acceptance of this distinction is required before claiming production parity. The thresholds remain frozen.

### Final inference recommendation and parity evidence


Recommend ort 2.0.0-rc.15 with pinned CPU ONNX Runtime and fp32 ONNX weights, initially four intra-op threads and one inter-op thread. Four threads give useful headroom at roughly five pairs/minute without using eight CPU threads per pair. This is a measured performance choice, not a speculative concurrency limit. Use the verified runtime version as a distribution contract and ensure the chosen ort API feature matches it. The scratch runtime was ONNX Runtime 1.31.1, loaded dynamically through ort's api-24 interface. The fp32 ONNX export is 572 MiB on disk; int8 is 145 MiB. The standalone Rust ort prototype is 900 KiB plus a 28 MiB native .so; corrected Candle prototype is 3.5 MiB with no native inference library. These sizes are scratch binaries, not measured changes to mj's linked release binary. Initial Candle dependency build took 94 seconds at four build jobs; adding ort in the same scratch package rebuilt shared dependencies, so that wall time is not an isolated ort build comparison. Cross-platform packaging remains acceptance work.

On the first 50 real dev pairs (2,961–5,649 tokens), fp32 ONNX versus supplied stored Python predictions has max absolute probability difference 0.00852518, mean 0.00112254, zero band changes. Int8 ONNX has max 0.31728945, mean 0.08083058, and 21/50 band changes. Reject this dynamic int8 export for shipment. It provides little latency gain and violates score parity.

On 50 additional real short pairs (387–1,746 tokens; 28 new and 22 heldout), fresh CPU Python fp32 versus corrected Candle has max difference 1.40071e-6, mean 6.22636e-8. Rust ort versus that same fresh Python reference has max 1.93715e-7, mean 2.28174e-8. Candle versus the stored predictions on these 50 pairs has max difference 0.00192377 and zero band changes. These short pairs make a bounded architecture parity check possible without hammering this host. They do not replace the implementation milestone's representative long-input parity fixtures. Additionally, fp32 ONNX matches fresh Python on four long inputs with max difference 4.43e-7. This isolates the observed stored-score drift from the ONNX export itself, but does not establish the stored scoring precision on every pair.

No bf16 Candle runtime benchmark was performed; fp32 is the measured safetensors candidate. Do not infer that bf16 preserves scores or improves CPU latency. That optional experiment is unnecessary for the recommended initial stack. The comparison measures corrected Candle, not its invalid stock single-logit softmax head. ONNX main timing/parity runs used Python's binding to the same native engine; the separate Rust ort run verified loading, execution and 50-pair score parity. Some scratch checks overlapped, so latency observations carry local contention and are not a rigorous isolated performance comparison. All heavy scratch work ran at nice 10; no GPU was used.

Single-observation elapsed inference seconds, real 3,415-token / 5,002-token pairs (the latter represents the requested p95-size input, not a latency percentile):

| Runtime / format | Threads | 3,415 tokens | 5,002 tokens |
| --- | ---: | ---: | ---: |
| Corrected Candle / fp32 safetensors | 1 | 41.011 | 97.037 |
| Corrected Candle / fp32 safetensors | 4 | 28.519 | 69.450 |
| Corrected Candle / fp32 safetensors | 8 | 20.134 | 36.268 |
| ONNX Runtime / fp32 ONNX | 1 | 11.062 | 22.919 |
| ONNX Runtime / fp32 ONNX | 4 | 4.161 | 11.281 |
| ONNX Runtime / fp32 ONNX | 8 | 3.961 | 6.965 |
| ONNX Runtime / dynamic int8 ONNX | 1 | 10.194 | 21.776 |
| ONNX Runtime / dynamic int8 ONNX | 4 | 3.855 | 10.250 |
| ONNX Runtime / dynamic int8 ONNX | 8 | 3.567 | 7.469 |

The independent Rust ort four-thread smoke run measured 6.529 / 16.487 seconds, under different concurrent scratch work, with the same scores. This variation is why the plan makes no precise service-latency promise. All runs were single observations with no warmup repetitions; first inference includes runtime kernel setup. Measured experiment work, including dependency preparation and compilation, finished within approximately twenty minutes. Candle's dense attention implementation makes it a poor fit for these long sequences even after correcting the head and biases. ONNX fp32 is the recommendation; bf16 and int8 are not prerequisites to shipping.

Reproduction uses the scratch root as working directory. `bench.py prepare` extracts the first 50 real dev pairs through frozen common.py, writes timing-pairs.jsonl, exports an eager-attention dynamic-length opset-17 graph, and creates per-channel dynamic QInt8 weights. `bench.py fp32 --threads 4` and `bench.py int8 --threads 4` score those 50 pairs; thread counts 1 and 8 select only the two timing pairs. `short.py` selects the 50 shortest real dev/new inputs for the bounded independent Python/Candle/Rust-ort check. `reference-short.py` runs fresh Python fp32. `summarize.py` joins by owner/repo/number/session_id to the frozen predictions. All were launched with `nice -n 10`. Preserve the logs and scripts rather than rerunning this local benchmark during each implementation round.

Revision note, 2026-11-12: completed measured runtime recommendation, rejected the measured int8 export, added explicit native-runtime distribution ownership and documented the stored-versus-fresh reference precision discrepancy. This is the reason production parity remains an implementation acceptance gate rather than being claimed complete by this plan.
