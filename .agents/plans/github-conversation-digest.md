# Preserve relevance relationships in GitHub classification

This ExecPlan follows .agents/PLANS.md. The daemon classifies new GitHub items for live sessions. The earlier v2 digest lost review and ownership relationships by keeping only short prompts and one reply; the user authorized a replacement conversation field and a replay before choosing its byte limits.

## Progress

- [x] Inspected the current digest, shared wire type, proxy validator, fixtures and prior replay evidence.
- [x] Implemented conversation text, mailbox rendering, shared contract, proxy, fixtures and documentation updates.
- [x] Replayed 332 requests on the same 83 pairs and timestamps; chose 8 KiB total / 2 KiB per message.
- [x] All requested checks passed: 24 Rust tests, zero failures (one pre-existing private-capture ignore); Clippy with warnings denied; TypeScript, proxy tests, 12 fixture validations, rustfmt and whitespace checks.

## Surprises & Discoveries

The baseline sample has 44 positives and 39 negatives. All timestamps are item creation plus 60 seconds because no selected item has a surviving decision log. Twenty-two states contained later-mutated materialized rows. This uncertainty remains; compare variants on identical reconstructed input. Typed mailbox messages contain important peer relationships and now use the existing mailbox renderer. All 332 request envelopes reconstruct with matching SHA-256 hashes. Most three-turn windows exceed the byte budgets; none of the 18 target misses exhausts its available conversation before even the 16 KiB budget.

## Decision Log

The user's design fixes the shape: session.conversation, files and references, no tools in conversation; load the same three recent turns, remove automatic continuation and context commands, select newest messages first while rendering selected messages oldest first. Conversation and message limits include truncation markers; the conversation limit includes role labels and newlines. Keep existing context boundaries, files and reference handling. Reuse continuation::PROMPT and acp::context_command_text for filtering, preserving peer messages and substantive user requests. The measured selection is 8192 total bytes and 2048 message bytes, maximizing recovery (15/18) at the unchanged .5 threshold with no new positives. Numeric variants are 4096/1024, 8192/1024, 16384/1024 and 8192/2048 bytes. Select the smallest meeting >=16/18 adjudicated misses and <=2 new positives at .5, otherwise the best measured variant.

## Outcomes & Retrospective

Conversation replaces requests/latest_reply throughout the undeployed v2 contract. None of four variants meets 16/18 recovery at .5. The chosen 8 KiB/2 KiB variant is best at 15/18, retains 32/44 v1 positives and adds 0/39 positives on v1 negatives. All requested validation passed: 24 Rust tests, one pre-existing private-capture ignore; Clippy, TypeScript, proxy tests, fixture validation and formatting checks.

## Context and Orientation

mj-core/src/github_item.rs owns the serializable evidence types, budgets and request validation; its questions_v2.json is shared by direct and hosted calls. mj-transcript/src/github_item.rs computes the digest. mj-controller/src/github_watch/service.rs loads three materialized turns and includes test classifiers using the old fields. services/jev-proxy/src/github-item.ts validates hosted v2 evidence and imports the frozen questions. Proxy tests and fixtures must change together. `/mnt/optane/jev-github-v2-20261108/replay/` holds the previous sample and the 18 provisional relevance adjudications.

## Plan of Work and Milestones

First replace requests/latest_reply with conversation in Rust and TypeScript, convert fixtures and documentation, and extend existing tricky-condition tests for newest-message selection, context boundaries, filtering and Unicode. Second copy working changes into a detached probe worktree and use the real Rust digest, parameterized there only for the four numeric variants, to read the live SQLite store with SQLITE_OPEN_READ_ONLY at the saved timestamps. Call TypeSafe sequentially, preserving raw scores and byte counts. Report thresholds .3/.4/.5 and whether three turns bound the available conversation. Choose constants and mirror the wire limit in the proxy. Finally run the specified checks, save logs, remove the probe worktree, and leave the product changes uncommitted for parent review.

## Concrete Steps and Validation

Run cargo test -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-controller --lib -- github_item github_watch github_reference reference_boundaries creation_output outside the sandbox. Run cargo clippy -p brokk-mj-core -p brokk-mj-transcript -p brokk-mj-controller --all-targets -- -D warnings outside the sandbox. In services/jev-proxy run npm run check and npm test. Run python3 scripts/jev-github-item-eval.py without --run. Tests must show UTF-8-safe cuts with labels/markers inside the budget and maximal request-envelope fit (including JSON escaping). Report measured byte maxima and limitations.

## Idempotence and Recovery

Resume API replay by variant/pair key without repeating accepted responses. Store only read-only queries; never start a daemon. Probe sources and outputs live outside product code; remove the detached worktree after saving evidence. Existing report inputs stay available. No new dependency or public budget-parameter API is required in product code.

## Artifacts and Interfaces

SessionContext changes to conversation: String, files: Vec<String>, references: Vec<String>. session_digest keeps its existing public signature; numeric arguments exist only in the disposable probe. Store a per-variant threshold/bytes table, chosen constants, fit evidence and check logs under the report directory. The parent owns final acceptance.

Plan created for the user's explicit conversation-digest assignment.

Plan updated after all four replay variants completed; 8 KiB/2 KiB is the best measured fallback, one recovery below the requested cutoff. Rust checks are complete and passing.

Final update: validation completed, evidence preserved under the report directory, detached worktree removed, changes left uncommitted as requested.
