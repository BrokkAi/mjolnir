# Bifrost brief runs, fixed binary (PR #13409)

Stage C relation briefs for BrokkAi/bifrost-dev#13150, regenerated with the #13398 fix
(relation_brief no longer drops proved callers). Replaces nothing: the first-run briefs
stay in ../bifrost/.

## EC2 VM (bf-13150-briefs2): 70 diffs
- Instance i-0be29426c7c5573de, type r8id.4xlarge (16 vCPU, 128 GB, local NVMe, caches and build on it),
  us-east-1, Ubuntu 24.04.5, AMI ami-08b275ac6cc5dbefb (ubuntu-noble-24.04-amd64-server-20261108).
- Bifrost commit 2e76c40839a402aeaa33ac92b448f55507b1e7b9 (PR #13409 head), built on the VM with
  `cargo build --profile measure --bin bifrost` (5m16s). `bifrost --version`:
  `bifrost 0.15.3` / `builtin-policy-catalog sha256=f9d3b4efcf20e0ca8e8a976d6524556f114f4a4ccab3f4b0cc761f0b9248d206`
  (the version string is the same as the first run; the commit is what differs).
- Mjolnir checkout: bundle of local master at 9d963be821a19ccbbeccfc0fcdc00244b91073dc; all 140 distinct
  base and intro commits were present (`git cat-file -e`).
- 7 workers, one BIFROST_CACHE_ROOT each (/work/cache1..7), one Bifrost process per cache, a shared
  queue ordered longest first by the first run's meta.json seconds, claimed with `mkdir claims/<diff>`.
  All workers used one mjolnir checkout as --root. Started 2026-11-15T22:15:44Z, last diff finished
  2026-11-16T01:56:50Z: 3 h 41 min wall time. Every driver exited 0; no failed diffs; no OOM.
- Claim log: claim.log (start and end per diff with worker). Diffs per worker: worker1 1
  (diff-c6429e612e, 13266 s, alone for the last ~1.8 h), worker2 11, worker3 11, worker4 12, worker5 9,
  worker6 13, worker7 13. Per-diff table with worker and both runs' items and seconds: summary.tsv.

## Things that went wrong
- The first worker start failed instantly for all diffs: driver.py reads $REVIEW_ARMS_DIR/instance/workspace.json
  at import. I created a stub instance/workspace.json ({"workspace":{"id":"stub"}}) in the VM output dir, cleared
  claims and logs (nothing had run), and restarted. The 70 results come from the second start only.
- The bifrost bundle was first cloned empty because the ref was refs/remotes/origin/...; fetched explicitly.
- The scp upload of the two bundles (480 MB) took a long time on this host's uplink.
- Wall times are from a 16-vCPU VM with 7 parallel Bifrost processes (load about 20 to 25 early on); they are
  not comparable with the first run's 32-vCPU VM.

## Determinism check
Compared brief.md here with after.md of the 32 diffs in /mnt/optane/tmp/13398-rerun-share/share/: 32 of 32 byte-identical.

## Known defect in this binary (#13399), found 2026-11-16
Bifrost 2e76c40839 still labels every symlink in a commit image unreadable (#13399),
and since #12418 that makes the whole-target JS/TS source inventory incomplete on
mjolnir (the crate LICENSE links). JS/TS usage scans then report `analysis_failure`.
479392b9 reproduced it on pending-assessment-hidden-when-idle: 0 items with this
binary, 7 complete items (the 0.15.3 callers) with #13423's head a603fe2f2f.
Briefs with failed JS/TS scans: pending-assessment-hidden-when-idle (7 -> 0 items),
path-completion-lost-on-step-redraw (8 -> 9), diff-0cb5bc6665 (2 -> 2). Stage C
measures this binary as built, so these briefs are kept as generated.
