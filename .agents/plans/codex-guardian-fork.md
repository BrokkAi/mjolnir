# Ship reliable Codex guardian and yolo modes

This ExecPlan follows `.agents/PLANS.md` and spans `/home/jonathan/Projects/hel2` and `/home/jonathan/Projects/codex-acp`.

## Purpose and Scope

New mj Codex sessions must preserve configured sandbox grants with on-request/auto_review on localhost and guardian hosts, and use unrestricted execution with never approvals in containers and designated yolo hosts. No repair of existing sessions, saved-state migration, general mode redesign, or upstream PR.

## Progress

- [x] Transfer and synchronize the fork (BrokkAi/codex-acp, upstream 51d6247).
- [x] Fix and test adapter permission handling and tag publishing workflow (e4be839, b07ecef).
- [x] Integrate mj and test locally, including image (2fca575f).
- [ ] Release adapter, image, and mj and verify all channels.

## Surprises & Discoveries

The remote fork can fast-forward 86 commits to upstream 1.11.0. The new npm package is absent and local npm authentication is unavailable. The parallel baseline PTY failures were resource-contention symptoms: all six passed in isolation and in the full serial suite. Live mj validation found fresh untrusted worktrees defaulting to read-only; initialize guardian with workspace-write unless a restricted or named profile is configured. Project-local named grants require preserving runtimeWorkspaceRoots for additional directories in pinned Codex 0.153.4.

## Decision Log

Use BrokkAi/codex-acp and @brokkai/codex-acp, retaining the codex-acp executable. Keep origin as upstream and brokkai as the fork push remote. The user chose tag-triggered releases, without automatic previews or registry dispatch. Required transfer, commits, pushes, and releases are authorized. Exact Codex dependency pin: 0.153.4.

## Context and Plan of Work

The adapter's src/CodexAcpClient.ts creates sessions and overwrites their sandbox on every prompt from src/AgentMode.ts. Make guardian preserve resolved configuration and yolo explicitly disable the sandbox, consistently from startup through successive turns. Preserve additional workspace directories. Transfer the fork, fast-forward upstream, update package metadata and release instructions, and replace inherited publishing with a tested v-tag workflow.

Mj selects execution policies in src/hel_config.rs and pins tools in src/hel_harness_runtime.rs. Select agent for configured approvals and agent-full-access for unconstrained targets; remove unconditional bypass injection. Update embedded worker npm manifests/lockfiles, cache identities, controller fallback package/version checks, and containers/Containerfile.agent-dev together. Leave existing saved launches untouched.

## Concrete Steps and Acceptance

Adapter: run npm run typecheck, npm test, npm run build, and npm pack verification. Add behavior coverage for guardian configured roots/networking/project additions across successive turns, and yolo. Follow the run-codex skill for disposable live tests. Mj: run cargo fmt --all -- --check, cargo test outside the sandbox, cargo clippy --all-targets -- -D warnings, and all release validations. Resolve PTY failures at their source. Exercise new local mj guardian and local-container yolo sessions, inspect effective policy, prove permitted writes and escalation routing. Keep evidence under target/ and do not disturb live user sessions.

## Release Milestones

Publish the tested adapter first, expected 1.11.1. Prepare the tarball/workflow before requesting any indispensable npm bootstrap interaction. Verify the public installed artifact, regenerate mj's exact lockfile, then build/test the local image. Publish amd64 and arm64 using the existing image workflow and record its digest. Release the next mj patch, expected 2.6.2, following RELEASING.md: synchronized versions/licenses, clean-commit validation, exact-commit CI, tags, GitHub assets, crates.io, npm, and Homebrew.

## Recovery and Interfaces

Do not overwrite tags or registry versions. Commit only changed files on existing branches; no upstream pushes from the adapter. Preserve ACP compatibility and executable name. Distinguish the fork in cache identity. Keep unrelated harness policy unchanged. Do not publish mj with an unavailable adapter package. Retry failed release workflows after inspecting existing artifacts.

## Outcomes & Retrospective

Adapter: 570 tests passed, 26 skipped; typecheck and build passed. Real Codex tests proved guardian legacy/named/project-local grants, additional directories, repeated prompts, and outside-write guardian escalation. Real mj daemon/worker/TUI test passed two guardian prompts with configured external writes and verified on-request/auto_review/workspace-write rollout contexts. Evidence: target/fork-live/ and target/reliability-artifacts/codex-guardian-fork-seed-1-2981822/.

Mj: cargo test -- --test-threads=1 and cargo clippy --all-targets -- -D warnings passed. Native debug and x86_64 musl worker builds passed. License policy passed. Native optimized mj and worker release builds passed. The final local image built successfully (18209ee3bd3e86325b806d9f5834d6b2ed8241eee98a7c92cb0f19007adbabe9). A real mj local-podman session completed two prompts using the baked fork and recorded danger-full-access/never on both turns; evidence: target/reliability-artifacts/codex-container-fork-seed-1-3311182/permissions.json and target/fork-live/mj-container-final.log. The test container and copied authentication were removed. Fork CI 34526395101 passed on b07ecefcfe6b181e7a22216fb94071d927a4e71b.

Publication dependency: @brokkai/codex-acp has no registry entry and npm whoami returns ENEEDAUTH. User was asked to perform npm login; no credentials requested in chat. Prepared exact tarball target/brokkai-codex-acp-1.11.1.tgz. The production lock uses the intended registry URL and this tarball's SHA512; verify registry integrity and clean npm ci after bootstrap before shipping mj. Do not publish mj or the image while this dependency is unavailable.


## Remaining publication steps

As of 2026-09-10 20:36 UTC, npm whoami still returns ENEEDAUTH. No release tags, npm package, published image, or mj release have been created for this task. Fork changes are pushed; mj changes are committed locally and withheld from origin/master because its container-path push would start image publication against an unavailable npm dependency.

After npm login with @brokkai publishing access, bootstrap the exact tested target/brokkai-codex-acp-1.11.1.tgz. Verify registry integrity equals sha512-eGh2NvM9rsWRNEgqrLXIofxs2+ngOUn5yDsF702MAbG6hV6S4PpYynayWg91xwK/UgqnHsl443Q0oDqDjSu/VQ== and clean managed npm ci works. Set the new npm package's trusted publisher to BrokkAi/codex-acp, publish.yml, environment release; do not alter the existing mj publishers. Tag the tested fork commit v1.11.1 and let the workflow attach the release tarball (its npm step skips an already published exact version). Then complete the mj patch release metadata and all RELEASING.md checks, push the current branch to origin/master, verify image publication and exact-commit CI before the mj tag, and complete registry/Homebrew channels. The user has already authorized these pushes and releases.
