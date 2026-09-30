# Unify projects, repository identity, and directory discovery


This is a living ExecPlan maintained according to `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log, and Outcomes & Retrospective current. Implementation is on the existing `hel4` branch; do not change branches or push.

## Purpose / Big Picture


The user chooses Projects for managed targets and directories for raw targets. Both selections must identify the same project under the hood: a bundle of repositories, with a single repository for raw targets. Choosing a repository subdirectory, a symlink, a linked worktree, or its GitHub URL should find the same saved project. The selected raw checkout remains the actual execution directory. New users should see their recent native project directories immediately, and later picker openings should discover projects from new Mjolnir sessions without repeatedly scanning histories or probing unchanged repositories.

Repository identity is separate from fetch/push settings and the checkout location. A bundle's identity includes all member repositories, its primary repository, and destination layout. History and project memory must agree for raw and bundle-of-one sessions. Equivalent saved projects are merged, while running workers, stored checkpoints, and source settings remain usable.

## Progress


- [x] (2026-09-30 16:21Z) Grounded the approved design in the current branch and checked the build layout. `target` is an existing mbx-managed symlink; leave it untouched. The only preexisting working-tree change is untracked `scripts/__pycache__/`.
- [x] Implement one repository resolver and exact bundle reuse; core identity tests pass.
- [x] Add schema 67, the durable catalog, incremental discovery, accepted session definitions, and resumable consolidation. Alias and edited-layout regressions pass.
- [x] Seed native profile directories and connect both terminal/web pickers to shared refresh. Web validation: 70 unit tests and 42 browser tests pass; isolated discovery and terminal regressions are in the full Rust run.
- [x] Unify raw session project identity, prompt history, and memory; memory conflict/late-worker and raw UI regressions pass.
- [x] (2026-09-30) Validate isolated upgrade, persistence, UI, and default-workspace regressions; prepare the validated current-branch checkpoint.

## Surprises & Discoveries


New bundle creation already calls `mj_core::local_git::canonical_repository`, which maps subdirectories and linked worktrees to the main repository. Existing bundle matching only canonicalizes filesystem paths, and quick creation can match a larger bundle by its primary alone. Import instead uses GitHub origin identity. These independently defined rules cause inconsistent reuse.

`gh` is already required for the GitHub browser and optionally supplies credentials. Core resolution must use Git's advertised symbolic HEAD rather than introducing a mandatory `gh repo view` call. Raw launches and local discovery must work with `gh` absent. Existing authentication integration stays supported.

First-run setup currently configures Codex only. Native seed tracking must be per configured local profile home, so each home receives its own initial ten-session seed when first available. Ten means sessions, not ten distinct directories. Native conversations are not imported.

Prompt history is associated through `session_contexts.bundle_id`. Project memory currently gives a raw repository and a bundle of one different keys. Consolidation must preserve both histories and memory and account for workers that still use an old key.

## Decision Log


Decision: Raw targets retain a directory-oriented picker and single-directory launch. Internally the directory resolves to a bundle of one; `project_directory` remains execution context. Rationale: the user explicitly rejected a multi-repository raw checkout workflow. Date/Author: 2026-09-30, user/Codex.

Decision: Native seeding examines the newest ten sessions per local profile home once. Later picker openings refresh from Mjolnir session records only. Rationale: the user chose these limits to avoid redundant work. Date/Author: 2026-09-30, user/Codex.

Decision: Repository identity follows the remote tracked by the repository's default branch, independent of the current feature branch. Discover the default branch through cached symbolic remote HEAD or a bounded Git `ls-remote --symref ... HEAD` request. Use origin, or the sole remote, as the initial server; report unresolved ambiguity. No network remote means host-qualified canonical local identity. Rationale: user-directed semantics with general non-GitHub support and no new CLI dependency. Date/Author: 2026-09-30, user/Codex.

Decision: Preserve source checkout settings separately from repository identity. Rationale: fetch repository identity must not discard separate push destinations or overwrite raw checkout choice when equivalent sources reuse a project. Date/Author: 2026-09-30, Codex.

Decision: Merge equivalent bundle definitions, preserving primary selection and destination layout. Select the existing ID with most session references and lexicographic ties; persist the result and retain compatibility aliases. Rationale: user chose merging, and stable aliases protect checkpoints and older workers. Date/Author: 2026-09-30, user/Codex.

## Outcomes & Retrospective


Repository resolution, durable catalog storage, accepted session definitions, picker refresh transport, and memory redirects are implemented and validated. Raw launches preserve the chosen directory, while managed launches preserve accepted repository IDs, layout, and fetch/push settings. Equivalent project aliases share history and memory without changing active workers or checkpoint meaning. Discovery seeds ten native sessions per local profile home, then processes durable Mjolnir changes with explicit retries. No mandatory `gh` dependency was added.

The final dev-profile default-workspace test run and Clippy pass. Web validation passes 70 unit tests and 42 deterministic browser tests. The optional desktop package was excluded from the all-workspace check because this host lacks its GTK/GLib/Cairo development libraries; no build paths or host libraries were changed. Delivery is the validated checkpoint on `hel4`, with no push and the unrelated Python cache left untouched.

## Context and Orientation


`mj-core/src/local_git.rs` and `mj-core/src/remote_git.rs` contain Git interpretation, endpoint resolution, subprocess helpers, and advertised-HEAD parsing. Extend these rather than inventing a second parser. `mj-controller/src/controller.rs` creates bundles; `mj-controller/src/import/bundles.rs` matches imports. Configuration types are in `mj-core/src/config/targets.rs`. The controller daemon is the process that owns persisted session and lifecycle decisions; its runtime owner and serialized config mutation live in `mj-controller/src/daemon.rs` and `mj-controller/src/daemon/state.rs`.

`mj-controller/src/database/` owns SQLite migrations, the serialized writer, session records, and prompt history. Add durable catalog storage there and shared controller behavior in a `project_catalog` module. `mj-core/src/project_picker.rs` defines discovery transport types; `mj-controller/src/project_picker.rs` performs bounded repository discovery. Native metadata readers for all five harnesses live in `mj-controller/src/import/`; reuse their parsers and newest-first indexes, adding an early-stop metadata-only seed API rather than reading full transcripts or computing directory sizes.

The terminal picker lives in `mj-tui/src/wizards/projects.rs`, with background transport in `mj-cli/src/dashboard/io/`. The web picker lives in `mj-controller/src/web/viewer.js` and uses controller HTTP handlers. Raw directory suggestions currently use `State::project_directories`, whose history stores only twenty paths. Both UI surfaces must receive catalog discovery through the daemon, including explicit loading/error state and cancellation.

Worker startup memory settings are built in `mj-controller/src/controller/worker_binary/project_memory.rs`; canonical reconciliation is in `mj-core/src/project_memory.rs` and the session manager. Accepted work belongs to workers and survives daemon replacement. New scan tasks are cancellable, resumable work and must not delay upgrades for minutes. Short catalog commits participate in upgrade admission with their own label.

## Plan of Work


### Milestone 1: One repository resolver and exact reuse


Introduce shared resolved repository identity and location data near existing core Git helpers. Preserve the local selected worktree root separately from the main canonical repository root. Resolve a default branch from cached remote HEAD or bounded advertised HEAD; use `branch.<default>.remote` rather than current branch tracking, retaining current endpoint resolution separately for launch settings. Normalize GitHub source forms together, preserve case-sensitive paths for other hosts, and remove credentials from identity/display values. No-remote identity is local, while metadata failures remain visible instead of inventing an unrelated project.

Route quick creation through the full source-list implementation. Match configured local repositories through the same resolver and require exact member set, primary, and layout. Tests must show nested paths, symlinks, worktrees, URL aliases, fork push settings, feature-branch switching, and selecting one member from a larger bundle behave correctly.

### Milestone 2: Durable catalog and consolidation


Add storage for repository identity, host-qualified checkout locations, canonical bundle mapping, aliases, seed markers, and incremental session discovery progress. The daemon is the single serialized owner of catalog decisions. TOML remains an input for configured bundles; resolved identity/reuse belongs to the catalog. Record relevant session-directory changes durably in the same transaction as the session update, and refresh only new candidates. Successful commits advance the cursor; failed candidates remain retryable.

Consolidation retains a durable intent until config, session references, prompt contexts, and memory mappings have been reconciled. It is idempotent across crashes. Retain accepted per-session source settings and repository IDs so active workers and checkpoint archives do not change meaning. Merge equivalent projects only; custom layouts or primary choices remain separate. New database migrations must be forward-only, classified beside the migration, and conservatively breaking when old reads/writes cannot safely preserve the semantics.

### Milestone 3: Shared discovery and UI integration


Seed each local harness home from its newest ten native sessions using existing metadata readers. Skip unreadable/deleted/non-Git directories with useful diagnostics and continue other profiles. Persist seed completion after successful publication; missing or failed homes retry without repeating completed homes. A new configured home receives its own seed. At daemon startup and either picker entry, coalesce refresh requests and run scans in supervised cancellable tasks with progress published immediately.

Expose catalog reads and refresh through daemon transport and HTTP; route old `source`/`sources` creation requests through the unified service. Terminal/web Projects lists show readable names and canonical entries. Raw target directory lists use locations on the selected host and preserve browse/isolated-checkout controls. Each raw selection associates a bundle of one while retaining the actual chosen checkout. Opening screens later refreshes only Mjolnir session records.

### Milestone 4: Unified session identity, history, and memory


Replace synthetic raw-only project identity with resolved bundle identity at creation, import, move, resume, and recovery boundaries. Existing raw records are reconciled through aliases without disturbing workers. Persist accepted resolved bundle/source snapshots so catalog edits cannot change an existing launch. Project history resolves aliases consistently. Single-repository bundle memory uses repository identity, and old raw/bundle keys reconcile into the same canonical store with conflict preservation. Running worker replicas using old keys continue to reconcile through aliases.

### Milestone 5: Validation and delivery


Complete behavior tests and isolated end-to-end checks. Inspect the final diff, update human documentation for the actual behavior, and run dev-profile workspace `cargo test` outside the restricted sandbox plus `cargo clippy --all-targets -- -D warnings`. Run web unit tests and relevant isolated browser tests. Validate migration/daemon restart with active and suspended fixtures, pending questions, old IDs, history, and memory. Commit each coherent validated checkpoint on `hel4`, staging only task files. Do not push.

## Concrete Steps


Run commands from `/home/jonathan/Projects/mjolnir4`. Use ordinary Cargo through the existing mbx-managed setup; do not redirect target/cache paths. Every `cargo test` runs with elevated permissions because socket tests require loopback and Unix sockets. Focused checks begin with `cargo test -p brokk-mj-core` and `cargo test -p brokk-mj-controller`, followed by terminal/client tests as their interfaces change. Final commands are `cargo test` and `cargo clippy --all-targets -- -D warnings` on the dev profile. Record actual commands and outcomes below as milestones pass.

For manual or end-to-end binary invocations use `--instance project-catalog-test` or another unique named instance for every invocation, with isolated config/data. Never start this build against the default instance. Preserve the preexisting untracked Python cache. Plan-only edits need diff review; product Rust checkpoints require the appropriate tests before commit.

## Validation and Acceptance


The observable result is that the same repository selected through a subdirectory, worktree, or remote URL appears once in Projects, and both a raw launch and managed bundle-of-one use the same project history and memory. Raw launch runs in the selected checkout rather than jumping to the main checkout. A managed launch retains fetch/push intent. Multi-repository managed projects remain exact bundles; selecting one repository does not silently add others.

A fresh isolated profile set with twelve native sessions per harness contributes directories only from the ten newest sessions of each profile, deduplicates shared repositories, and shows the same local locations in raw suggestions. Reopening either screen reparses no native transcripts and probes no unchanged Mjolnir candidate. A newly created Mjolnir local session contributes its project on the next screen entry. Deleting a historical directory cannot prevent other projects from appearing.

With `gh` absent, local discovery, normalization, raw launches, and Git default-branch lookup still work. Fork and non-GitHub fixtures verify identity is stable across feature branch switches, push destinations remain intact, and ambiguous/no-remote configurations have correct visible outcomes.

Consolidation tests restart after each durable step and verify one canonical entry, stable aliases, intact histories, preserved checkpoint IDs, conflict-preserving memory, and active worker continuity. Upgrade tests use isolated instances and never require manual store reset or daemon restart. UI tests verify immediate loading state, responsive cancellation, coalesced refresh, and visible partial failures.

## Idempotence and Recovery


Catalog upserts, seed markers, aliases, and migration steps are repeatable. Never remove a legacy definition until its durable alias and per-session settings are available. Failures retain resumable intent; startup finishes accepted consolidation before publishing dependent client state. Scan cancellation does not advance an uncommitted cursor. Failed metadata probes remain retryable and do not overwrite a known identity. Never delete or rewrite native transcripts or the user's Git configuration.

## Artifacts and Notes


Initial branch: `hel4`; initial commit: `02290eed`. Existing build output points to `/mnt/optane/mbx-targets/`; use it as configured. No live instance has been modified.

## Interfaces and Dependencies


Use `Path`/`PathBuf` internally and convert to text at transport/render boundaries. Use the shared `CommandExecutor`, cancellable executor, and subprocess helpers. Extend shared project discovery transport types and daemon/HTTP clients with catalog read/refresh results and status rather than doing Git work in either UI. Keep existing bundle creation inputs compatible. Do not add a workspace crate, a mandatory `gh` dependency, or a worker wire-format requirement just to reorganize project identity.

Revision note (2026-09-30): Initial execution plan transcribes the approved design and the subsequent correction that Git default-branch lookup must remain independent of `gh`.

Execution evidence (2026-09-30): `cargo check --workspace --all-targets` cannot build the optional desktop package because this host lacks GTK/GLib/Cairo development libraries. Use the normal default workspace packages for required `cargo test` and `cargo clippy --all-targets -- -D warnings`. The initial default-package check found terminal imports to correct. Elevated web validation passed all 70 unit tests and 42 relevant deterministic Playwright tests.

Decision: Persist an accepted `ProjectBundleSnapshot` with each session. This contains repository IDs, destinations, resolved identity and launch network settings. Catalog aliases can therefore rebind history without changing checkpoints or later provisioning. Date/Author: 2026-09-30, Codex.

Decision: Memory consolidation publishes a durable redirect while holding the source and destination locks and the shared decision registry. Stale worker sync targets resolve that redirect before acquiring their write lock; they cannot continue writing the retired canonical store. Date/Author: 2026-09-30, Codex.

Revision note (2026-09-30): Implementation is complete and final validation is in progress. The first full Rust run passed 2,048 controller tests and exposed migration fixture assumptions, an alias-unaware full-session save, and fixtures that predated accepted project definitions. Migration 67 now tolerates retained new tables when historical fixtures replay the ladder, full-session saves consult the same session-scoped alias owner as prompt writes, and the tests use the simulated executor for SSH registration. Discovery cleanup is bounded to two seconds; abandoned restartable scans report their eventual errors. Terminal discovery retries are explicit, and refreshed locations update an already-open picker while preserving its selected source.

Revision note (2026-09-30): Final review found that canonicalizing a selected linked worktree all the way to the main checkout would discard worktree-specific Git source settings. Repository identity still uses the common repository and default-branch fetch provenance, while the configured source now keeps the selected checkout root. The real Git regression `selecting_a_linked_checkout_reuses_identity_and_retains_its_push_settings` passes. A separate isolated regression verifies that an explicitly repaired accepted source survives reload without changing its repository IDs, layout, or logical identity.

Validation evidence (2026-09-30): The complete default-workspace run passed all 2,069 controller and 586 core tests, including the historical upgrade ladder, seeded/incremental discovery, catalog aliases, edited layouts, source repair, and memory conflict reconciliation. Its remaining failure was one old terminal test expecting no background action on Recent entry; that expectation now requires `LoadMountHistory`. The final tree adds the verified worktree source separation and uses the existing picker generation to discard replies from an older opening or retry. Full dev-profile validation and Clippy are running on this stable tree before committing.

Final validation evidence (2026-09-30): Elevated `cargo test --quiet -- --test-threads=16` exits successfully on the final tree, including 2,070 controller tests, 586 core tests, 898 terminal tests, 693 worker tests, the CLI and isolated daemon upgrade suites, and doctests. Existing ignored tests remain ignored. `cargo clippy --all-targets -- -D warnings` also exits successfully. Web commands `node --test *.unit.test.mjs` and `./node_modules/.bin/playwright test --project=deterministic new-session.spec.js project-groups.spec.js`, run from `tests/e2e/web`, pass 70 and 42 tests respectively. The final staged diff passes whitespace checks.
