These are evaluation fixtures, not training data. Never upload them with model
weights. `parity.jsonl` preserves50 real identity-matched dev/new pairs (25 each),
compact Python input segments, token IDs, masks and fresh CPU fp32 probabilities.
47 contain history; three have empty/no history. Unicode, median/p95 lengths and
observations nearest both sides of each frozen cut are included. `overflow.json`
is derived from a real pair by repeating its conversation and adding Unicode;
Python truncates only the session to7999 tokens and provides a fresh fp32 score.

Reference code: frozen vast/student3/ce/score.py, common.py and jev_shape.py in
/mnt/optane/jev-github-firstcut-20261110/. Checkpoint150m-mixed.tar; model repository
brokkai/mj-github-relevance-ettin-150m, revision e4e47921640a6005e5059e5b04e4264b774c6866.
Python3.14, torch2.15.0+cpu, transformers5.22.2, tokenizers0.23.3; score.py CPU,
--batch-size1, no --bf16. Original stored GPU-bf16 predictions are informational;
the fresh Python values here are the numerical expectations. They are never
regenerated using Rust. Export/provenance evidence is in the #1513 model-owner
report and deploy-bench/{export,lean} outside the repository.

The explicit ignored integration tests fail when assets are missing. Point
MJ_GITHUB_STUDENT_TEST_CACHE at an isolated data directory containing the owner's
verified github-student/ and github-student-native/ caches, then run outside the
restricted sandbox:

    cargo test -p brokk-mj-controller github_student::tests::python_parity_and_sixteen_concurrent_calls_share_one_loaded_session -- --ignored

    cargo test -p brokk-mj-controller github_student::tests::owner_reuses_loaded_cpu_across_health_and_preference_changes -- --ignored

To populate a new isolated cache through the SAME preparation owner, explicitly
permit downloads and select the parity test first (requires public HF/GitHub/NuGet
network access, about300MB plus the platform's pinned native archives):

    MJ_GITHUB_STUDENT_PREPARE=1 MJ_GITHUB_STUDENT_TEST_CACHE=/path/to/isolated/github-student-parity cargo test -p brokk-mj-controller github_student::tests::python_parity_and_sixteen_concurrent_calls_share_one_loaded_session -- --ignored

This launches no daemon and reads no live/default store. The normal suite uses
local HTTP fixtures for interruption, corruption and atomic publication. The
ignored lifecycle test additionally uses a local server with real model assets,
one recorded loader failure and actual ORT recovery, proving concurrent callers
observe one preparation and retries reuse verified content. The parity test
checks input token IDs/masks before scores, then compares16 concurrent calls with
the same serial calls bit for bit on one loaded CPU session.


GPU qualification must run in its own Cargo test process because ORT's main
library is immutable within a process. On a Linux x64 NVIDIA host, install CUDA12
and cuDNN9 through the system loader, then run (no Python/pip preload):

    MJ_GITHUB_STUDENT_PREPARE=1 MJ_GITHUB_STUDENT_TEST_CACHE=/path/to/isolated/gpu-cache cargo test -p brokk-mj-controller github_student::tests::actual_gpu_python_parity_and_warm_latency -- --ignored --nocapture

For a source tarball without `.git`, additionally set
`MJ_BUILD_REVISION` to the full40-character commit SHA that was exported.
The same command tests real CoreML placement on an Apple-silicon Mac or DirectML
on Windows. It requires actual EP nodes in the ORT profile, compares50 Python
probabilities and routing, discards one warmup, and reports latency against five
CPU controls. It fails explicitly when a GPU or its libraries are unavailable.
The optional coreml_external_initializers_padding_and_python_parity test checks
both6144/8192 static buckets and shared fp16 external initializer registration;
on Linux it uses CPU, on Apple-silicon macOS it also requires CoreML placement.
Use an isolated cache and delete it, CoreML compiler caches and any ORT temp
entries afterwards when running on a borrowed Mac.
