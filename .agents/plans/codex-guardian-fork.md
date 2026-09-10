# Ship reliable Codex guardian and yolo modes

This completed ExecPlan follows `.agents/PLANS.md` and spans `/home/jonathan/Projects/hel2` and `/home/jonathan/Projects/codex-acp`.

## Purpose and Scope

New mj Codex sessions preserve configured sandbox grants with on-request/auto_review on localhost and guardian hosts. Containers and designated yolo hosts use unrestricted execution with never approvals. No stored-session repair, state migration, general mode redesign, or upstream PR was added.

## Progress

- [x] Transfer jbellis/codex-acp to BrokkAi/codex-acp and fast-forward to upstream 51d6247.
- [x] Fix and test adapter permission handling; replace upstream automation with tag-triggered publication.
- [x] Integrate the fork into mj and validate live guardian and container-yolo sessions.
- [x] Publish adapter 1.11.1 and configure its npm trusted publisher.
- [x] Publish and verify amd64/arm64 agent-dev images.
- [x] Publish mj 2.6.2 archives, all nine crates, all four npm packages, and the Homebrew update.

## Decision Log

Use BrokkAi/codex-acp and @brokkai/codex-acp, retaining the codex-acp executable. Keep origin as the adapter's upstream remote and brokkai as its push remote. The user selected tag-triggered releases with no automatic previews or upstream registry dispatch. Exact Codex dependency: 0.153.4. All transfer, commit, push, and release operations were explicitly authorized.

Guardian preserves resolved permission configuration across turns; new guardian worktrees default to workspace-write unless a restricted or named profile is configured. Yolo explicitly selects unrestricted permissions at startup. Additional directories extend configured roots. Mj selects agent for configured approvals and agent-full-access for unconstrained targets and no longer injects unconditional CODEX_CONFIG bypasses.

## Surprises & Discoveries

The initially installed Codex dependency was stale; npm ci restored the intended exact version. Live mj validation exposed the fresh untrusted-worktree read-only default, which direct pretrusted adapter tests had missed. Project-local named grants require preserving runtimeWorkspaceRoots for additional directories in Codex 0.153.4.

Upstream capacity-retry changes arrived before the mj push. A conflict-free preview and their existing validation record supported merging them in 1fdaf464. Automatic review rejected an initial combined merge-and-push command; local integration followed by complete validation before pushing resolved that rejection.

Existing CI failures required test-only corrections: a Unix-specific preflight test needed a Unix compile guard, and fake-harness fixtures needed fake Node/npm tools for launch preflight. All six PTY tests, the preflight test, three-client reliability with zero leaks, and Clippy passed after those corrections. Production behavior was unchanged by these fixture corrections.

Initial npm publication required the user's login and browser authorization. The new package's trusted publisher was then configured and read back. During mj publication, npm accepted large platform packages but the workflow's one-minute visibility wait expired before registry processing completed. Waiting for public visibility and rerunning failed jobs resumed publication without overwriting any version; the final npm workflow passed.

## Validation and Acceptance

Adapter: 570 tests passed, 26 skipped; typecheck and build passed. Real Codex checks proved legacy/named/project-local guardian grants, additional directories, successive prompts, and guardian escalation for an outside write. Real mj guardian sessions wrote twice to a configured external grant and recorded on-request/auto_review/workspace-write. Real mj local-podman sessions used the baked fork for two prompts and recorded danger-full-access/never. Test containers and copied authentication were removed.

Mj: full serial tests, Clippy, optimized builds, license policy, generated license comparisons, all nine package assemblies, npm packaging tests, and Linux packaging tests passed. Final exact-commit CI passed all nine jobs, including Windows compilation, macOS and Linux tests, and both Linux glibc 2.28 compatibility jobs. All public archive checksums matched. Downloaded Linux CLI and worker binaries report 2.6.2; a fresh public npm installation runs and reports mj 2.6.2.

## Published Results

Adapter source: b07ecefcfe6b181e7a22216fb94071d927a4e71b, tag v1.11.1. GitHub release: https://github.com/BrokkAi/codex-acp/releases/tag/v1.11.1 . Publication workflow 34529362694 passed. The GitHub-built tarball is byte-identical to the locally tested and npm-published tarball. npm integrity: sha512-eGh2NvM9rsWRNEgqrLXIofxs2+ngOUn5yDsF702MAbG6hV6S4PpYynayWg91xwK/UgqnHsl443Q0oDqDjSu/VQ==. Trusted publisher: BrokkAi/codex-acp, publish.yml, environment release; configuration ID 80753811-44fa-4ff4-9b78-7b93ca06812e.

Mj release source: 02070f7ec0865cde49efb647715531aa03514f4e, tag v2.6.2. GitHub release: https://github.com/BrokkAi/mjolnir/releases/tag/v2.6.2 . CI 34531215403, release 34533897199, crates.io publication 34536561920, and npm publication 34536570397 all completed successfully. All nine crates and four npm packages were independently verified public at 2.6.2.

Image workflow 34530627897 passed for amd64 and arm64. Published image ghcr.io/brokkai/mjolnir/agent-dev:latest (also :sha-1fdaf46) has manifest digest sha256:91b2c6112f754a6b65f7e367f722e81e3a14e4f04f02a4b1e8038d1ba36ab80d. Both architecture build logs verify @brokkai/codex-acp 1.11.1 and codex-cli 0.153.4; the pulled amd64 image also passed those version checks. Its source 1fdaf464 differs from the final release only in test fixtures and agent notes.

Homebrew formula update: https://github.com/BrokkAi/homebrew-tap/commit/385f72ed2a31085c384fb29bdcd4c569873e8098 . The existing generator produced only version, URL, and checksum changes; the content-SHA-protected update was read back byte-for-byte and preserves the managed-install wrapper.

Public archive SHA-256 values:

- aarch64 Linux: 7a5662819ee3d5f8b840da7f8e285b24489a5cb959dd8e850505302769b4bf15
- x86_64 Linux: ec604cb27334d92ec8e7f98fd3242e72dae75eefc8ad73375cf1a6e8136e2d27
- universal macOS: 7e5e40b705460a1a43ec8d751d558371ce0d9863c644e1d5f0228856b028bee3

## Evidence and Recovery

Local evidence is under target/fork-live/, target/reliability-artifacts/, target/release-v2.6.2-checks/, and target/release-v2.6.2-*.log. The registry-availability.json readback contains true for every published package. Image manifest and build logs, public checksum files, tested tarballs, Homebrew payload/readback, and fresh-install artifacts are also retained under target/.

Published tags and versions are immutable. Future adapter releases follow its docs/RELEASES.md; mj releases follow RELEASING.md. The established workflows skip already-published versions when rerun. Do not move either published tag.

## Outcomes & Retrospective

Complete. The fork, configured guardian/yolo behavior, multi-architecture image, and mj release are published and verified across every required channel. No user sessions were repaired or restarted. Live tests caught the startup permission issue; registry retries recovered propagation delays without altering released artifacts.
