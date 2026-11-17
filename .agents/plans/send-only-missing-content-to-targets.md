```md
# Send a remote target only the bytes it does not already have

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.


## Purpose / Big Picture

Mjolnir's controller (the daemon, `mj daemon-run`) often runs on a workstation behind a slow home uplink, around 2.6 Mbps, and drives sessions on remote targets such as EC2 hosts. Today it re-sends large bodies the remote host already holds. Every session launch or restart on an EC2 host uploads a compressed profile archive of about 16 MB, even when an identical session started on the same host a minute earlier. Every recovery copy pulls a whole ZIP back to the controller. Every Move packs the full Git history and can re-send a 61 MiB worker executable the host already has. On 2026-11-17 these transfers saturated the uplink: reviewer sessions could not start within 600 s (#1583), control messages queued behind bulk data (#1580), and lanes lost reviews.

After this work, the controller sends a body only when the receiving host cannot prove it already owns it. A second session launched on a warm host sends only its secrets and the few files that differ, typically kilobytes rather than megabytes. A recovery copy sends only the payload parts that changed since the last copy the controller holds. A Move sends only the Git objects and workspace files the destination lacks. You can see it working in the daemon log, which reports, for every transfer, the bytes offered, the bytes actually sent, and the objects reused, and in launch times on a warm EC2 host.


## Progress

- [ ] Milestone 1: per-object negotiation for every non-secret profile file; secrets in a small private bundle; project memory sent once; Move helper uses the worker store; byte accounting in logs.
- [ ] Milestone 2: recovery copies transfer only missing payload parts against the controller's retained object set; legacy ZIP assembled on demand.
- [ ] Milestone 3: Move negotiates Git objects with the destination and sends only missing workspace files.
- [ ] Milestone 4: one receiver-owned content store per host storage and account, with garbage collection from durable roots, replacing the ad hoc profile and worker paths.


## Surprises & Discoveries

- Observation: the profile store already exists and already deduplicates, but only for opted-in shared trees. Project-memory files, settings, and other private entries are always sent whole, so a warm host still receives about 16 MB.
  Evidence: daemon log `logs/mj-daemon-20261117T161634.022Z-2513220.log`, `send profile content archive_bytes=15891162 shared_content_files=0`; the 18:31Z daemon log shows 15,916,921 and 15,920,437 bytes.
- Observation: Move's helper installation bypasses the worker executable store and can stream a full executable (about 61 MiB for x86_64 Linux musl) per session or build helper, even when that build already exists on the host.
  Evidence: `transfer.rs:install_helper` versus `store.rs:store_key,store_gate`.


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


## Outcomes & Retrospective

Not started.


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

Milestone 1 makes launches and restarts cheap and is the most urgent, because it removes the cause of #1580 and #1583. Change profile staging so that every non-secret entry, including project memory, settings, and user instructions, is offered to the receiving host as a manifest of objects: path, mode, ownership, type (file, directory, symlink), size, and SHA-256. Replace the whole-set upload gate with per-object negotiation: the receiver answers, for each object, present (and pins it), missing (and claims it for this operation), or in flight (claimed by another operation); the controller streams only the objects it claimed, in batches, over the bulk connection, and other operations wanting the same object wait for that claim to finish or fail and then ask again. The receiver verifies length and hash on a private temporary file and publishes atomically, exactly as the store does today. Credentials and tokens are never offered to the shared store; they go in a small private bundle for that session only. The memory replica and baseline are offered as objects referenced twice in the manifest, so identical content is sent once. Finish by materializing the session home from the manifest: hard links or copies from the immutable store into the mutable home, with private copies for files the harness may modify. Change Move's `install_helper` to install through the worker executable store, so a build already present on the host is never re-sent. Add one log line per transfer giving bytes offered, bytes sent, objects reused, objects sent, and time spent waiting on another operation's claim separately from time spent sending. This milestone deliberately reuses the existing profile store layout and scripts; Milestone 4 generalizes them.

Milestone 2 makes recovery copies send only what changed. Keep the controller's guarantee: after a copy completes, the controller holds every byte needed to restore. Store retained recovery content at the controller as objects keyed by the payload and part SHA-256 values that the archive format already computes, and have the target offer the new copy's manifest of parts. The controller requests only parts it lacks, verifies them, and records the new copy durably before target cleanup is allowed, exactly as today. The legacy ZIP is assembled on demand from objects for export and for any reader that needs a ZIP. Do not treat compressed ZIP bytes as deduplication boundaries; use the declared payload parts. Existing ZIPs remain valid and are imported lazily. Measure, from adjacent sealed checkpoints, how much fixed 16 MiB parts deduplicate before considering content-defined chunks.

Milestone 3 makes Move send only what the destination lacks. For Git, negotiate objects with the destination's actual repository (as `git fetch` does) instead of packing full reachable history; never rely on destination ancestry that the destination cannot prove it holds, and preserve the history cutoff that review points need. For non-Git workspace files, offer a manifest and send only missing files, using the same per-object negotiation as Milestone 1.

Milestone 4 generalizes the profile and worker stores into one receiver-owned content store per host storage and account, with a small versioned host-side helper that answers negotiation and owns claims, verification, publication, and pins. Garbage collection removes only objects unreachable from durable roots: active workers, retained or parked sessions, pending Move intents, and installed restorable archives. Attachments, fetch staging, and resource uploads move onto the same protocol where it saves bytes. Older workers that lack the helper keep using the existing paths, detected by capability, never by replacing a live worker just to probe it.


## Concrete Steps

Work from a detached worktree, for example `~/Projects/mjolnir-wt/send-only-missing`, created from `origin/master`. Build and test on the dev profile.

For Milestone 1, read `install.rs` around the profile functions named above and `shared_profile_paths`, then write the manifest and negotiation types beside them. Run the affected tests with `cargo test -p mj-controller <filter>` and `cargo clippy -p mj-controller --all-targets -- -D warnings`. Then, in an isolated instance, start a daemon and launch two sessions in a row on a Podman target over SSH (an EC2 host created for the test, or a local SSH Podman target), and compare the per-transfer log lines:

    mj --instance send-only-missing daemon status
    grep 'send profile content' <instance log dir>/mj-daemon-*.log

The first launch should send roughly today's bytes; the second should send only the private bundle and changed files. Stop the test daemon afterwards with `mj --instance send-only-missing daemon stop`.


## Validation and Acceptance

Milestone 1 is accepted when, on a warm host, a second session launch sends no more than its private bundle plus the files that actually differ, the log line shows the reuse, and a reviewer group of three starts without any profile upload larger than its private bundles. Add a tricky-condition test where two operations offer overlapping manifests at once: each object is sent once, the second operation waits for the first's claim and then pins the published object, and a claim whose sender dies is recoverable. Add a test driving more than 64 KB through the object stream. Add an end-to-end test in an isolated instance comparing first and second launch byte counts. Move's helper must not upload an executable whose hash is already in the worker store.

Milestone 2 is accepted when a recovery copy following an unchanged turn transfers only manifest-sized data, a restore from objects is byte-identical to the legacy ZIP's contents, export still produces a valid legacy ZIP, and target cleanup still waits for durable proof. Milestone 3 is accepted when a Move to a host that already has the repository sends only new objects and changed files. Milestone 4 is accepted when garbage collection under concurrent uploads never removes a pinned or in-flight object, including across process death and daemon handoff.


## Idempotence and Recovery

Every receiver step is atomic: temporary file, verify, rename. A lost acknowledgement is retried by operation ID and the receiver's durable claim status, never by replaying an arbitrary mutation. Re-running a launch or copy after a crash re-negotiates and sends only what is still missing. Legacy archives, Move intents, and worker relay formats stay readable throughout.


## Interfaces and Dependencies

Define a manifest entry type carrying path, kind, mode, ownership, size, and SHA-256; a negotiation request and response per batch of objects with outcomes present-and-pinned, missing-and-claimed-by-you, and in-flight-elsewhere; and a publish step that atomically records a manifest and its pins before materialization. Keep secrets out of these types entirely; the private bundle is a separate per-session transfer. Reuse the existing SHA-256 implementation and the store scripts' verification and hard-link pinning. Any new persisted format or database change must advance the revision and be classified compatible or breaking per `AGENTS.md`.
```
