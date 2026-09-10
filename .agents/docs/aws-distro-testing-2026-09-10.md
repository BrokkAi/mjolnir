# AWS Spot distro testing — 2026-09-10

Tested committed HEAD `350854488a66b18108759294c607a6c79b3f7f8e` on six real x86_64 Linux VMs through tmux. Filed two confirmed product bugs, retained reproductions and terminal captures, and stopped after the second batch because further distro variants would largely repeat the tested assumptions. No product fixes were made.

## Confirmed findings

[Issue #984](https://github.com/BrokkAi/mjolnir/issues/984): the Linux release installer reports success on hosts where its installed CLI cannot execute. Debian 12 and Amazon Linux 2023 lack the required `GLIBC_2.38`/`GLIBC_2.39` symbols. Alpine lacks the GNU dynamic loader. NixOS rejects the generic dynamically linked executable. All four reproductions used the actual published v2.6.0 release, installed by the script from the tested HEAD; these are not merely failures of a locally compiled binary. The current installation guide does not state the needed libc floor. The diagnostic static-musl build started and ran sessions on all four.

[Issue #985](https://github.com/BrokkAi/mjolnir/issues/985): an inline ACP form can hide the current field title and position, leaving an unlabeled boolean control such as `☐ No`. Reproduced through tmux on Ubuntu and Fedora with the existing deterministic component form. A minimal Ubuntu probe confirmed that `Enabled` remains absent at 140×40, 200×60, and after resizing back. The compact question pane allocates too little space to show the field label together with its focused control.

## Environment and coverage

All machines were one-time Spot `m6a.large` instances in `us-east-2c`, with publisher-verified images, encrypted disposable root disks, and at most three concurrent VMs. Distro security settings were not disabled. NixOS used the image's root SSH account; the other tests used the default non-root cloud account.

| Distribution | libc | tmux | Tested HEAD binary | Verified core tmux probe |
| --- | --- | --- | --- | --- |
| Ubuntu 24.04 | glibc 2.39 | 3.4 | GNU | Passed; spaces, CJK, and emoji in project path |
| Debian 12 | glibc 2.36 | 3.3a | Static musl after GNU loader failure | Passed |
| Fedora 44 | glibc 2.43 | 3.7c | GNU | Passed with `LC_ALL=C` |
| Alpine 3.24.1 / OpenRC | musl 1.2.6 | 3.7c | Static musl after GNU loader failure | Passed on retry; one unconfirmed detach timeout |
| NixOS 26.05 | glibc 2.42, Nix store layout | 3.6a | Static musl after GNU loader failure | Passed as root |
| Amazon Linux 2023 | glibc 2.34 | 3.6a | Static musl after GNU loader failure | Passed |

The core probe creates a real local-bare session with the repository's deterministic fake ACP agent and displays it in tmux. It resizes through 1×1, 40×5, 79×18, 80×18, 200×60, 80×10, and 140×40; verifies survival and restoration of the dashboard; sends a 70,034-byte Unicode prompt; verifies the reply in an **agent** conversation entry and its tail on screen; opens and closes Help; and detaches while another prompt is in flight. Verified replies arrived in approximately 1.7–1.9 seconds, including the fixture's 1.5-second delay. Detach plus the harness's own socket cleanup completed in under three seconds. SQLite integrity checks and fixture process cleanup passed on each final successful run.

Real `mj doctor --json --smoke` Podman checks passed on Ubuntu (Podman 4.9.3) and Fedora (5.8.4), including rootless UID-map validation and disposable run/exec/remove of `docker.io/library/alpine:3.24`. The overall doctor command correctly exited nonzero because the deliberately empty doctor configuration had no signed-in harness profiles; its Podman-specific results were ready. This does not prove a complete container-backed ACP session.

The standard `tui_components_tmux.py` harness exercised startup, invalid and valid project input, session creation, rename, ordinary replies, and model/effort controls before encountering the form-label bug. It did **not** complete end to end. A continuation run that explicitly skipped the blocked chat-control group reached another stale target-dialog cancellation expectation on Amazon Linux. The NixOS component fixture also failed worker startup, while the independent core probe passed; the component fake contains a hardcoded `/usr/bin/python3` shebang that does not match NixOS's layout. Neither continuation run is counted as a full baseline pass.

## Harness adjustments and inconclusive observations

Only disposable copies were adjusted; repository product and test sources were left unchanged.

- The relative-path error assertion expected `Project directory must be an absolute`; HEAD renders `Enter an absolute path or a path starting with ~/.`.
- The composer is now titled with model/effort rather than `Prompt`. The original chat helper's focus locator is stale. A footer-based replacement was also focus-dependent, so the independent probes avoided relying on it.
- Python's default JSON escaping produces invalid TOML surrogate escapes for emoji paths. The custom fixture uses `ensure_ascii=False` when writing TOML string values.
- An initial 216,034-byte prompt exceeded mj's documented-in-code 65,536-character API limit and was correctly rejected. The final 70,034-byte prompt remains under that character limit while crossing a 64 KiB pipe boundary.
- Conversation responses deliberately omit older lines in very long entries. The final reply assertion checks the agent entry's tail; it does not mistake the immediately echoed user prompt for an agent response.
- Amazon Linux's default Python 3.9 lacks `tomllib`; Python 3.11 was installed for the harness. Its existing `curl-minimal` was retained. Alpine provisioning used `doas`, not `sudo`. NixOS packages were installed with `nix-env`, and extracted test files were assigned to the test account rather than disabling Git's ownership protection.
- One Alpine run missed Alt-Q after Help dismissal and a new prompt. A full rerun passed, and six focused Escape/Alt-Q trials all detached successfully. Captures and logs are preserved; there is insufficient evidence to file a reliable product defect for this observation.
- A Fedora loopback SSH-bare probe successfully reached remote directory/Git validation but the synthetic repository lacked a supported network remote. Its subsequent `file://` remote was correctly rejected. Full SSH provisioning and checkpoint/resume over SSH remain unverified.

No live model credentials were copied to the VMs. ACP behavior used deterministic fixtures. ARM64, Rocky/AlmaLinux, openSUSE, real-provider authentication, full checkpoint/resume coverage, and the entire component-harness workflow remain outside the completed coverage. Stopping after six distros was intentional: both libc families, an older glibc baseline, OpenRC, NixOS's unusual layout, multiple Git/tmux generations, and SELinux-enabled environments had been exercised, while the final packaging failures repeated #984.

## Reproduction artifacts

Local artifacts are under `target/distro-spot-20260910/` (ignored build storage). `resources.json` records exact resource IDs and launch/termination-request times; `images-*.json` records AMI ownership and versions. `build.json` and `spot-prices.json` record binary and pricing evidence. Each distro directory contains bootstrap/environment logs, release installation results where tested, probe logs, and `evidence-small.tar.gz`. `evidence-omitted.json` identifies files over 10 MiB excluded from the compact archive, principally copied executables; the tested binaries remain available in the local payload/portable directories. Earlier larger archives are retained for the first batch.

The final successful probe directories inside the respective compact archives are:

| Distro | Evidence directory |
| --- | --- |
| Ubuntu | `distro-unicode-seed-91004-7160` |
| Fedora | `distro-c-locale-seed-91004-6004` |
| Debian | `distro-normal-seed-91004-3662` |
| Alpine | `distro-normal-seed-91004-2966` |
| NixOS | `distro-normal-seed-91004-1194` |
| Amazon Linux | `distro-normal-seed-91004-26906` |

Each includes `probe.json`, terminal captures, the final large conversation response, logs, process evidence, and SQLite integrity output. The form reproduction is `ubuntu/extracted/distro-form-seed-91004-5962/`; its three `form-boolean-*.txt` captures show the missing label. Alpine's focused key trials are in `distro-keys-seed-91004-3426`.

The task-local `probe.py`, `formprobe.py`, `keyprobe.py`, `patch_harness.py`, and `baseline_remaining.py` preserve the exact exploratory drivers. Run a copied repository with matching `target/debug/mj` and `mj-worker`, then `python3 ../probe.py normal` (or `unicode`, `c-locale`); use Python 3.11 on Amazon Linux. The drivers use isolated configuration and stop their own processes before removing runtime files. Cloud operations require a new run identity and key; do not reuse the completed resource ledger to launch machines.

Binary SHA-256 values after stripping debug sections:

| Build | Binary | SHA-256 |
| --- | --- | --- |
| GNU | mj | `9cb4b1f278c444d3dae6a03724a34a862e48c9c173e07bddff0aa9115426832b` |
| GNU | mj-worker | `210c8df5e3be1302e099a744efdfcb7f31ab5148c17bc5b9a4ad5eaeae40d51d` |
| Static musl | mj | `e90240dd7b375ece99534983caf08d613c723edbf7468ee225d51e5bff028b3a` |
| Static musl | mj-worker | `7428717b6c4bb3d165f5aacc33d9f87263ee8bb538582753d57e5eed6e65e723` |

## Cost, cleanup, and validation

The six VMs accumulated about 1.14 VM-hours before termination requests, with individual lifetimes of roughly 8–15 minutes. AWS reported Spot rates of $0.0297–$0.0300/hour, approximately $0.034 in compute before short termination tails. Including temporary disks, public IPv4 addresses, and evidence downloads, estimated total spend is below $1; this is an estimate, not a settled bill. The user-authorized ceiling was $25.

Cleanup verification is recorded in `target/distro-spot-20260910/cleanup-complete.json`: all six instances terminated, no launchable Spot requests, and no remaining run-owned volumes, security groups, or EC2 key pairs. The local temporary SSH private/public keys were removed. Credentials from `~/.secrets` stayed on the controller. An independent watchdog was active during the run; guest expiry was a secondary precaution rather than the cleanup mechanism relied on at completion.

Built both GNU and static-musl CLI/worker pairs with the pinned Rust 1.96.0 toolchain and `--locked`. Validated task-local Python syntax and reviewed the documentation diff. No Rust or dependency changes were made, so no repository-wide Cargo test/clippy run was required for this documentation-only commit. The pre-existing untracked workspace plan and `mj.sqlite3` were left untouched.
