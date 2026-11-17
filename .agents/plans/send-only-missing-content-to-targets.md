```md
# Send a remote target only the bytes it does not already have

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Mjolnir's controller (the daemon, `mj daemon-run`) often runs on a workstation behind a slow home uplink, around 2.6 Mbps, and drives sessions on remote targets such as EC2 hosts. Today it re-sends large bodies the remote host already holds. Every session launch or restart on an EC2 host uploads a compressed profile archive of about 16 MB, even when an identical session started on the same host a minute earlier. Every recovery copy pulls a whole ZIP back to the controller. Every Move packs the full Git history and can re-send a 61 MiB worker executable the host already has. On 2026-11-17 these transfers saturated the uplink: reviewer sessions could not start within 600 s (#1583), control messages queued behind bulk data (#1580), and lanes lost reviews.

After this work, the controller sends a body only when the receiving host cannot prove it already owns it. A second session launched on a warm host sends only its secrets and the few files that differ, typically kilobytes rather than megabytes. A recovery copy sends only the payload parts that changed since the last copy the controller holds. A Move sends only the Git objects and workspace files the destination lacks. You can see it working in the daemon log, which reports, for every transfer, the bytes offered, the bytes actually sent, and the objects reused, and in launch times on a warm EC2 host.


## Progress

- [x] 2026-11-17 19:15Z: Implemented Milestone 1 profile negotiation, credential bundle, manifest materialization, launch accounting, and Move helper reuse.
- [x] 2026-11-17 19:15Z: Dev-profile receiver tests passed with overlapping offers, a killed receiver, a stream wider than a pipe buffer, identical baseline/replica bytes, and private credentials. The Move helper reuse test passed against a prepublished worker.
- [x] 2026-11-17 19:25Z: Two isolated SSH Podman launches reached idle. The cold launch sent 1,051,436 bytes and two objects. The warm launch sent 1,645 bytes and one new digest, and reused the 1 MiB fixture object.
- [x] 2026-11-17 19:27Z: Exposed the manifest-only prewarm interface through the same receiver.
- [x] 2026-11-17 19:47Z: Milestone 1 validation passed with #1583's byte-progress watchdog merged. The targeted receiver and Move tests passed, and Clippy reported no warnings in the controller or core crates. A new SSH Podman run sent 1,051,437 cold bytes and 1,643 warm bytes.
- [x] 2026-11-17 20:06Z: Replaced credential detection with the coordinator's shareable-kind allowlist. The three targeted receiver tests and the Move reuse regression passed, including unknown and credential-bearing MCP files remaining private. Controller Clippy passed.
- [x] 2026-11-17 20:11Z: The reviewed allowlist build passed the SSH Podman launch comparison. It sent 1,051,304 cold bytes and 1,574 warm bytes. The second launch sent zero shareable objects.
- [x] 2026-11-17 21:53Z: Fixed the deployed descriptor exhaustion regression. The real receiver sent and published 1,152 cold objects with soft NOFILE=1,024, then sent zero warm object bodies. Overlap and concurrent dead-claim takeover regressions passed and controller Clippy was clean.
- [ ] Milestone 2: recovery copies transfer only missing payload parts against the controller's retained object set; legacy ZIP assembled on demand.
- [ ] Milestone 3: Move negotiates Git objects with the destination and sends only missing workspace files.
- [ ] Milestone 4: one receiver-owned content store per host storage and account, with garbage collection from durable roots, replacing the ad hoc profile and worker paths.


## Surprises & Discoveries

- Observation: the profile store already exists and already deduplicates, but only for opted-in shared trees. Project-memory files, settings, and other private entries are always sent whole, so a warm host still receives about 16 MB.
  Evidence: daemon log `logs/mj-daemon-20261117T161634.022Z-2513220.log`, `send profile content archive_bytes=15891162 shared_content_files=0`; the 18:31Z daemon log shows 15,916,921 and 15,920,437 bytes.
- Observation: Move's helper installation bypasses the worker executable store and can stream a full executable (about 61 MiB for x86_64 Linux musl) per session or build helper, even when that build already exists on the host.
  Evidence: `transfer.rs:install_helper` versus `store.rs:store_key,store_gate`.

- Observation: Codex profile configuration can carry an actual bearer token inline. The existing regression for a custom provider named by `experimental_bearer_token` confirms this, in addition to native login files and generated AWS credentials. Review also confirmed that MCP argument arrays can carry credentials. The settings files themselves stay in the private bundle.
- Observation: the old store uses a separate `.x` entry for owner-executable content. The receiver can pin either existing entry by SHA-256 and size, then applies the placement’s mode to a private copy. Identical bytes with different modes therefore transfer once.
- Observation: the shared executor already drains stdout and stderr concurrently, but previously returned stdout only after stdin finished. The negotiated command observes stdout after stripping the SSH start marker and closes that reply stream on accepted command failure, including cancellation.

- Observation: the first deployed receiver retained one locked descriptor per missing object. Release c4831a676 generated 49 cold Create/Unpark failures in roughly 35 minutes on EC2. A replay with 1,152 unique objects and a soft NOFILE limit of 1,024 reproduced the error before peak payload traffic began. The operation-lease receiver published all 1,152 objects under the same limit and reused them on the next offer.


## Decision Log

- Decision: recovery durability stays at the controller. The controller keeps a complete, verified object set for every recovery copy; transfers send only missing parts; the legacy ZIP is assembled on demand for export and for older readers. Keeping bulk only on a remote store is out of scope.
  Rationale: a recovery copy exists to survive loss of the target. A manifest at home whose objects live only on the source host fails on host loss, termination, or Move teardown. Delta transfer gets most of the byte savings without changing the recovery guarantee.
  Date/Author: 2026-11-17, coordinator session 5992a212 (default chosen; the user may revise).
- Decision: reuse authority is the receiving host's actual storage plus Unix account (or guest), identified by an identity the receiver generates, not by target alias. Credentials and tokens never enter a shared store; they travel in a per-session private bundle. Non-secret private content such as project memory may be deduplicated by digest within one store identity, but a session can only materialize objects its own manifest references, and possession of a digest alone never makes content fetchable.
  Rationale: two target aliases that reach the same account and disk should share one cache; separate SSH users, Nitro guests, and ephemeral disks must not.
  Date/Author: 2026-11-17, coordinator session 5992a212.
- Decision: remote-to-remote direct transfer (EC2 host to EC2 host without the controller relay) is out of scope for this plan, except through Git object negotiation in Milestone 3.
  Rationale: it needs an authorized peer route that does not exist today; measure first.
  Date/Author: 2026-11-17, coordinator session 5992a212.
- Decision: the reviewer minimal profile (which entries a reviewer needs at all) is owned by #1580's work, not this plan. This plan owns how entries are transferred; #1580 owns which entries a profile class contains.
  Rationale: one owner per fact; the two changes touch the same staging code and must not both decide profile contents.
  Date/Author: 2026-11-17, coordinator session 5992a212.

- Decision: tie each profile object claim to the receiver process that will verify and publish it. The controller ships a Perl receiver using standard modules in the SSH command and feature-detects its runtime. A short filesystem metadata lock serializes pinning, claim creation, and collection. Operation scratch and pins use the same filesystem as the store, including when it is a separate mount, and the legacy collector reserves their directory. The receiver holds each claim through upload and publication, backed by the operation lease described below. A busy reply can include independent claims, which finish before the sender releases the SSH stream and polls again. Older hosts without the receiver runtime use their existing archive path.
  Rationale: closing the channel or killing the receiver releases the locks without a guessed timeout, PID reuse, cross-controller registry, or gap between negotiation and ownership. Bundling the receiver preserves compatibility with every currently installed worker and lets the progress watchdog count pipe writes through the same executor.
  Date/Author: 2026-11-17, Milestone 1 implementer.
- Decision: Move adopts the existing executable store and its publication/collection ownership. Milestone 4 owns revising the worker store’s lifetime and lock scope.
  Rationale: a cache hit needs zero upload immediately. Changing worker collection and its protected staging together is a separate implementation, as confirmed by the coordinator.
  Date/Author: 2026-11-17, coordinator session 5992a212.

- Decision: Nitro sends its profile with the existing archive path and no shared objects. Milestone 4 owns a store in the guest's trust boundary, along with its durable roots and cleanup.
  Rationale: profile staging reaches Nitro's SSH hypervisor account before engine copy. Reusing that account's store would mix objects from distinct guests.
  Date/Author: 2026-11-17, coordinator session 5992a212.

- Decision: reviewer prewarming will use `ProfileContentManifest::try_transfer` without a staging-home destination. Only missing allowlisted objects travel, using the exact negotiation, claims, and verification used by launch. Session-start and resume prewarm scheduling belongs to #1580 worker 20760f13 after Milestone 1. The daemon schedules one durable intent per session start or resume. The receiving account's per-digest claims resolve all overlap between those intents and live launches.
  Rationale: prewarm should prepare receiver-owned objects before the parent requests a review, while keeping the credentials and materialized reviewer home scoped to that future session.
  Date/Author: 2026-11-17, coordinator session 5992a212.

- Decision: share only explicitly staged plugin, skill, rule, and agent trees, known harness assets, and the memory replica and baseline roots returned by staging. Every other file goes into the private bundle, including harness settings, MCP configuration, instructions, credentials, and any unclassified entry. Launch and cache-only prewarm use this same inventory policy.
  Rationale: a profile setting might pass a credential positionally or under any flag or environment name. The producer's allowlist establishes which kinds may outlive the session. A spelling-based check cannot provide that guarantee.
  Date/Author: 2026-11-17, coordinator session 5992a212 after Milestone 1 review.

- Decision: a receiver owns one locked operation lease for all of its claims. Each claim name is created exclusively as a hard link to that lease, and the existing metadata lock serializes stale-name takeover and publication. A nonblocking probe uses at most one extra descriptor and can still detect active claims from the old receiver. Publication unlinks that object's claim.
  Rationale: the first deployed receiver kept one descriptor per missing object, which exhausted EC2's 1,024-descriptor soft limit during Create and Unpark. A locked inode allows us to keep the same death detection and mixed-version ownership without tying descriptor use to manifest size or raising limits.
  Date/Author: 2026-11-17, Milestone 1 regression follow-up.


## Outcomes & Retrospective

The new receiver pins existing blobs and publishes verified new ones to the established profile store. The scope is the receiving SSH account and storage. A manifest maps private materialized copies into the staged profile, and credential files are transferred in a separately encoded bundle that stays in private staging. Manifest paths are byte-safe and carry the staged kind, mode, length, and digest. The receiving account owns staged files and the existing container install assigns its normal owner. Staging continues to follow source symlinks before offering content. The Move helper is linked from the same verified worker store as a session worker. The reviewed allowlist build reduced compressed profile bytes from 1,051,304 on the first loopback SSH Podman launch to 1,574 on the second, a 99.85% reduction. The warm transfer contained the manifest and private bundle, with zero object bodies. The 1 MiB object was unchanged and stayed cached. Receiver measurements exclude the SSH framing and the manifest offer; the compressed transfer includes the manifest and credential bundle. The test accounts for small generated entries that vary by session. The reviewer cohort's start-time prewarming and minimal profile remain with #1580; its end-to-end cohort validation follows that work.


## Context and Orientation

The controller is the daemon process (`mj-controller`). A worker (`mj-worker`, binary `hel`) runs on each target and hosts the agent harness. A target is a place sessions run: localhost, a Podman host over SSH, an EC2 host created by `mj ec2 create`, and others. The "uplink" is the controller host's outbound network link; on the development workstation it is slow and shared by every session.

Four kinds of bulk transfer matter here.

Profile staging. Before a harness starts, the controller assembles the harness's home directory (its "profile"): plugins, skills, rules, agent trees, project memory (a replica and a baseline copy), settings, credential files, and user instructions. The code is in `mj-controller` under the install path, centered on `install.rs` (search for `PROFILE_CONTENT_STORE`, `QUERY_PROFILE_CONTENT`, `STAGE_PROFILE_CONTENT`, `INSTALL_PROFILE_CONTENT`, `MATERIALIZE_PROFILE_CONTENT`, `profile_archive_entries`, `upload_profile_archive`). A per-host store at `~/.cache/mjolnir/profile-content` keys files by SHA-256 (plus a suffix for owner-executable). The function `shared_profile_paths` opts in rules, plugin and agent trees, and synced skill directories by harness; everything else goes into a per-session archive that is always sent whole. The upload gate keys on the SHA-256 of the sorted whole set of shared keys, so identical sets serialize and overlapping but unequal sets can both send the same missing file. Stage scripts verify hashes before atomic rename, record process identity, and pin needed objects with hard links; private copies then go to the mutable home. Garbage collection runs at most hourly and removes files older than a day; there are no durable per-session manifest roots.

Worker executables. `worker-binaries/<sha256>/hel` beside target worker roots is a per-host store, reused by remote containers. `store.rs:store_key,store_gate` maps daemon data directory, locator command prefix, and store path to an in-process gate. Installation verifies the hash and uses hard link then rename so a running executable's inode is never overwritten. Move's `transfer.rs:install_helper` does not use this store.

Recovery copies. `mj-controller/src/recovery.rs` (`PolicyState::due`, `checkpoint_due`) schedules copies; `controller/checkpoint/latched.rs` can prestage, seal, and pack on the target; `checkpoint_transfer.rs:CheckpointTransfer::execute,transfer_plan` copies the entire ZIP by SCP, checks SHA-256, commits it durably, and only then allows target cleanup. `mj-checkpoint/src/archive.rs` (`prepare_archive_view_with_part_size`, `plan_payload_parts`, `CheckpointPayloadReader`, `verify_archive_streaming`) already declares each payload's SHA-256 and, for compressible payloads, fixed 16 MiB parts with their own SHA-256. Git bundles are stored as Git produced them. On 2026-11-17, surviving recovery ZIPs had a median of 9.71 MiB and a p90 of 39.14 MiB; the slow-transfer subset had a median of 25.59 MiB.

Move. `mj-core/src/move_workspace.rs` assesses bytes (`included_bytes`, with a 1 GB consent threshold) and the Move path packs the full reachable Git history into a bundle, stages files under a per-operation directory, and relays them through the controller with rsync. It cannot reuse objects the destination already has, or another operation's files.

Git mirrors on targets (`controller/git_cache.rs`: `repository_cache_key`, `PREPARE_REPOSITORY_SCRIPT`, `CACHE_GC_SCRIPT`) are already content-addressed by Git itself and use fetch negotiation; keep them. Attachments (`mj-core/src/attachment.rs:AttachmentStore`) already verify SHA-256 and size and are a good model for verification and access scoping.

Relevant rules from `AGENTS.md`: one owner decides each fact; no speculative caps or timeouts; daemon handoff never waits on worker turns or long copies, and long copies must be cancellable and resumable from durable intent; preserve compatibility with older workers, archives, and Move intents; test in isolated named instances (`--instance <name>`); drive streaming code with more than 64 KB of data.


## Plan of Work

Milestone 1 makes launches and restarts cheap and is the most urgent, because it removes the cause of #1580 and #1583. Offer explicitly shareable staged kinds, including both project-memory trees, to the receiving host as a manifest of objects. Keep settings and user instructions private under the review decision above. A manifest describes path, mode, ownership, type (file, directory, symlink), size, and SHA-256. Replace the whole-set upload gate with per-object negotiation: the receiver answers, for each object, present (and pins it), missing (and claims it for this operation), or in flight (claimed by another operation); the controller streams only the objects it claimed, in batches, over the bulk connection, and other operations wanting the same object wait for that claim to finish or fail and then ask again. The receiver verifies length and hash on a private temporary file and publishes atomically, exactly as the store does today. Credentials and tokens are never offered to the shared store; they go in a small private bundle for that session only. The memory replica and baseline are offered as objects referenced twice in the manifest, so identical content is sent once. Finish by materializing the session home from the manifest: hard links or copies from the immutable store into the mutable home, with private copies for files the harness may modify. Change Move's `install_helper` to install through the worker executable store, so a build already present on the host is never re-sent. Add one log line per transfer giving bytes offered, bytes sent, objects reused, objects sent, and time spent waiting on another operation's claim separately from time spent sending. This milestone deliberately reuses the existing profile store layout and scripts; Milestone 4 generalizes them.

Milestone 2 makes recovery copies send only what changed. Keep the controller's guarantee: after a copy completes, the controller holds every byte needed to restore. Store retained recovery content at the controller as objects keyed by the payload and part SHA-256 values that the archive format already computes, and have the target offer the new copy's manifest of parts. The controller requests only parts it lacks, verifies them, and records the new copy durably before target cleanup is allowed, exactly as today. The legacy ZIP is assembled on demand from objects for export and for any reader that needs a ZIP. Do not treat compressed ZIP bytes as deduplication boundaries; use the declared payload parts. Existing ZIPs remain valid and are imported lazily. Measure, from adjacent sealed checkpoints, how much fixed 16 MiB parts deduplicate before considering content-defined chunks.

Milestone 3 makes Move send only what the destination lacks. For Git, negotiate objects with the destination's actual repository (as `git fetch` does) instead of packing full reachable history; never rely on destination ancestry that the destination cannot prove it holds, and preserve the history cutoff that review points need. For non-Git workspace files, offer a manifest and send only missing files, using the same per-object negotiation as Milestone 1.

Milestone 4 generalizes the profile and worker stores into one receiver-owned content store per host storage and account, with a small versioned host-side helper that answers negotiation and owns claims, verification, publication, and pins. Garbage collection removes only objects unreachable from durable roots: active workers, retained or parked sessions, pending Move intents, and installed restorable archives. Attachments, fetch staging, and resource uploads move onto the same protocol where it saves bytes. Older workers that lack the helper keep using the existing paths, detected by capability, never by replacing a live worker just to probe it.


## Concrete Steps

Work from a detached worktree, for example `~/Projects/mjolnir-wt/send-only-missing`, created from `origin/master`. Build and test on the dev profile.

For Milestone 1, read `install.rs` around the profile functions named above and `shared_profile_paths`, then write the manifest and negotiation types beside them. Run the affected tests with `cargo test -p brokk-mj-controller <filter>` and `cargo clippy -p brokk-mj-controller -p brokk-mj-core --all-targets -- -D warnings`. Then, in an isolated instance, start a daemon and launch two sessions in a row on a Podman target over SSH (an EC2 host created for the test, or a local SSH Podman target), and compare the per-transfer log lines:

    python3 tests/e2e/profile_transfer.py --mj target/debug/mj --worker target/debug/mj-worker

The fixture starts a loopback SSH server with a private home and builds a disposable target image from a preloaded agent-dev image. It drives an isolated `--instance send-only-missing` and stops that daemon during cleanup. The first launch sends the manifest, private bundle, and missing content. The second launch sends the manifest, private bundle, and any changed content. Its log lines give compressed profile-archive bytes sent and unique uncompressed object bytes offered, excluding SSH framing.


## Validation and Acceptance

Milestone 1 is accepted when, on a warm host, a second session launch sends no more than its private bundle plus the files that actually differ, the log line shows the reuse, and a reviewer group of three starts without any profile upload larger than its private bundles. Add a tricky-condition test where two operations offer overlapping manifests at once: each object is sent once, the second operation waits for the first's claim and then pins the published object, and a claim whose sender dies is recoverable. Add a test driving more than 64 KB through the object stream. Add an end-to-end test in an isolated instance comparing first and second launch byte counts. Move's helper must not upload an executable whose hash is already in the worker store.

Milestone 2 is accepted when a recovery copy following an unchanged turn transfers only manifest-sized data, a restore from objects is byte-identical to the legacy ZIP's contents, export still produces a valid legacy ZIP, and target cleanup still waits for durable proof. Milestone 3 is accepted when a Move to a host that already has the repository sends only new objects and changed files. Milestone 4 is accepted when garbage collection under concurrent uploads never removes a pinned or in-flight object, including across process death and daemon handoff.


## Idempotence and Recovery

Every receiver step is atomic: temporary file, verify, rename. A lost acknowledgement is retried by operation ID and the receiver's durable claim status, never by replaying an arbitrary mutation. Re-running a launch or copy after a crash re-negotiates and sends only what is still missing. Legacy archives, Move intents, and worker relay formats stay readable throughout.


## Interfaces and Dependencies

Define a manifest entry type carrying path, kind, mode, ownership, size, and SHA-256; a negotiation request and response per batch of objects with outcomes present-and-pinned, missing-and-claimed-by-you, and in-flight-elsewhere; and a publish step that atomically records a manifest and its pins before materialization. Keep secrets out of these types entirely; the private bundle is a separate per-session transfer. Reuse the existing SHA-256 implementation and the store scripts' verification and hard-link pinning. Any new persisted format or database change must advance the revision and be classified compatible or breaking per `AGENTS.md`.
```
