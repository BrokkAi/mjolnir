# Make Mjolnir configuration discoverable to localhost agents

This ExecPlan follows `.agents/PLANS.md` and must be kept current as implementation proceeds.

## Purpose / Big Picture


A user can open a localhost session and ask its model to configure Mjolnir. The installed `mj` skill describes editing the user's TOML configuration and carries the maintained configuration reference. Container and SSH/EC2 sessions do not receive this host CLI skill; their existing worker MCP delegation tools keep their own instructions.

## Progress


- [x] (2026-09-30) Inspect skill staging, credential reconciliation, configuration documentation, and instance environment propagation.
- [x] (2026-09-30) Implement skill guidance, reference packaging, and one target-derived skill scope used at launch and synchronization.
- [x] (2026-09-30) Validate skill frontmatter, published-package inclusion, and documentation build/link checks; preserve reference content exactly during relocation.
- [x] (2026-09-30) Pass clippy with warnings denied, all new behavior regressions, and individual reruns of the five existing worker tests that failed in the broad run.
- [x] (2026-09-30) Complete all remaining integration/doctest targets, verify relative host-directory overrides in a dedicated named instance, and pass clippy on the final source.
- [x] (2026-09-30) Review the final diff and prepare the required commit on the existing `hel3` branch.

## Surprises & Discoveries


Launch adds managed skills, but subsequent credential reconciliation currently uses a single archive for every session of a profile. A profile can have both localhost and isolated sessions, so synchronization must select the appropriate archive per target. Worker harnesses clear their environment; locality alone does not preserve the daemon's named-instance configuration paths.

## Decision Log


- Decision: Keep TOML and existing commands; deliver guidance through the installed skill.
  Rationale: The user explicitly chose discoverable instructions over a configuration API.
  Date/Author: 2026-09-30 / Codex.
- Decision: Keep one configuration-reference source inside the publishable core crate and generate its website page through the existing guide synchronization script.
  Rationale: Published Cargo crates cannot embed files outside their package, and the website already copies embedded guides.
  Date/Author: 2026-09-30 / Codex.
- Decision: Derive localhost versus isolated skill scope from the recorded session target. Exclude the reserved `mj` skill directory for isolated sessions, including any profile copy, at staging and sync.
  Rationale: The CLI connects to the host daemon; containers and remote machines cannot use it. One decision must control both delivery paths.
  Date/Author: 2026-09-30 / Codex.

## Outcomes & Retrospective


Implementation and validation are complete. The skill links to its packaged configuration reference, website generation uses that same source, and isolated staging/sync exclude the reserved CLI skill directory. Localhost launch environments preserve the daemon's absolute instance directories, including relative overrides supplied to the daemon. Skill validation, package-file inspection, documentation checks, final clippy, all new regressions, CLI and worker integration targets, and doctests passed. Five worker tests and one CLI viewer timeout fixture failed during broad runs; all six passed individually on their identical compiled binaries. The full broad runs therefore returned nonzero and must not be described as clean passes. The required commit is prepared for `hel3`; no push is authorized. No host configuration or live store has been edited.

## Context and Orientation


`mj-core/assets/skills/mj/SKILL.md` is embedded by `mj-core/src/skills/managed.rs`. `mj-controller/src/controller/worker_binary/launch.rs` stages it into the session-owned harness home. `mj-controller/src/worker_client/credential_sync.rs` later sends the canonical skills archive to workers. A worker is the per-session process running the model; the daemon is the user's controlling process. `mj-core/src/state.rs` records each session's target. `docs/scripts/sync-podman.mjs` already generates website pages from embedded runtime guides. The existing configuration reference is `docs/src/content/docs/configuration.md`.

## Plan of Work


First move the configuration reference into the skill resources and include it in the core Cargo package. Extend the guide-generation script and ignore the generated website copy. Broaden the skill description and add a short configuration workflow, including correct configuration paths, preserving unrelated edits, version handling, and doctor diagnostics.

Next add an explicit skills scope derived from the session target and use it in staging and reconciliation. Preserve unrelated user skills. Cache the two scoped archives per wire format so a profile shared by local and remote sessions cannot send the wrong tree. Carry the daemon's absolute configuration/data directories to localhost harnesses so commands and file edits address the same instance. Use `std::path::absolute` without requiring the directories to exist yet.

## Concrete Steps


Run from `/home/jonathan/Projects/mjolnir3`: inspect the diff, run the skill validator, `cargo fmt --all -- --check`, elevated `cargo test`, and `cargo clippy --all-targets -- -D warnings`. Run `npm run check` and `npm run build` from `docs/`. Keep existing mbx build storage and all automated isolated directories. The completed broad run encountered the six timing failures noted below, so its failed tests were run individually; CLI integration targets, worker binary/recovery targets, and `cargo test --doc` completed the targets it skipped. The localhost-path test was additionally run with relative configuration/data overrides resolving under a dedicated temporary directory and `MJ_INSTANCE=mj-configuration-skill`. Stage only changed files and commit to the existing branch.

## Validation and Acceptance


Behavior tests must show that localhost staging includes a readable configuration reference and unrelated skills; isolated staging excludes the entire `mj` directory while retaining unrelated skills. Reconciliation must make the same selection for mixed local/isolated sessions of one profile, including removing an old remote copy. All supported skill archive formats must preserve these results. The website must still render `/configuration/` with valid links. Any manual executable invocation uses `--instance mj-configuration-skill`.

## Idempotence and Recovery


Guide generation and skill staging can be repeated. Tests use temporary profile homes and existing isolated instances. No migrations, daemon replacement, publication, or push are part of this task. If a check fails, fix failures introduced by these edits and record unrelated failures accurately.

## Artifacts and Notes


The skill validator reported `Skill is valid!`. `cargo package --list --allow-dirty --offline -p brokk-mj-core` includes both `assets/skills/mj/SKILL.md` and `assets/skills/mj/references/configuration.md`. The final docs build checked 2154 internal links across 26 HTML files. A content comparison with the original page confirms the configuration reference body is unchanged. Final `cargo clippy --all-targets -- -D warnings` exited zero on the dev profile. Controller tests passed (2084), core tests passed (590), and all new staged-file and shared-profile sync tests passed. The five worker failures passed when invoked individually with `--exact --test-threads=1` on `target/debug/deps/mj_worker-7b20c5e49b0f788d`; each rerun took under three seconds. The CLI main tests passed 253 and failed one viewer fixture with a broken pipe after its timeout; that test passed individually on `target/debug/deps/mj-2e25c8990694a6de` in 0.19 seconds. All seven CLI integration targets passed (36 tests, four existing ignored import cases), worker binary/recovery targets passed (8 and 4), and all doctests passed. The focused relative-path test passed on the final source in its named instance. Logs are under `/mnt/optane/mj-configuration-skill-*.log`. The repository's `target` remains the existing mbx-managed symlink.

## Interfaces and Dependencies


Use existing `SkillsArchive`, `SkillsArchiveFormat`, `HarnessKind`, and session `TargetLocator` types. Introduce a two-variant `SkillsScope` for localhost and isolated sessions and a target method returning it. Pass this scope into session skill construction and managed staging; add it to the copied credential-sync target. Reuse existing TOML loading and doctor commands without a new CLI command or configuration transport.

Plan created 2026-09-30 after inspecting delivery paths and finding that cleared worker environments require explicit host-instance propagation.

Plan updated 2026-09-30 after implementation and initial documentation validation. Rust compilation exposed an extra argument in the new regression's call to the existing test wrapper; corrected before rerunning the suite.

Plan updated 2026-09-30 after clippy and focused regressions passed. The broad worker suite encountered five existing deadline-sensitive failures; direct single-test reruns of its identical binary all passed. Complete the remaining CLI/doctest coverage rather than repeat already-passing controller/core suites.

Plan updated 2026-09-30 after final validation. Resolve relative host-directory overrides before passing them to a session with another working directory. Final clippy, the relative-path regression, all remaining integration targets, and doctests passed. Record the broad-run timing failures accurately rather than claim those invocations passed.
