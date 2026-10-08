# Expose durable session recovery controls

This ExecPlan follows `.agents/PLANS.md` and supports the approved MergeMarshall lookahead plan.

## Purpose / Big Picture

An invalidated speculative merge reuses its session and checkout, clears obsolete queued input, stops work, clears the native conversation, and receives a generated restart brief. CLI/API callers need stable prompt command IDs so a lost reply cannot create a second clear or restart turn.

## Progress

- [x] Inspect deployed clear-context and background-stop behavior.
- [x] Add optional prompt command IDs and queue cancellation through the authenticated API and CLI.
- [x] Validate routes and CLI request wiring, touched crate suites and workspace Clippy.
- [x] Commit, publish and deploy before enabling monitor lookahead.

## Surprises & Discoveries

The worker already supports typed `/clear`, queue cancellation, and idempotent command IDs. The HTTP prompt request did not expose producer-chosen IDs, and queue cancellation was only available in the viewer.

Full validation exposed upstream historical migration fixtures retaining the newer mailbox failure column. Corrected their old schema shapes without changing production migration logic. The CLI fixture also needed the API version header, and new flag help descriptions are required by the existing help test.

## Decision Log

Expose these existing primitives; keep old prompt requests wire-compatible. Do not change worker protocol, storage, harnesses, build layout, or session provisioning.

## Milestones

First expose optional `command_id` on prompt requests and `mj prompt --command-id`. Replies retain the original relay acceptance ordinal when the worker sees a retry. Then expose POST `/sessions/{id}/queued-prompts/clear` and `mj clear-queue`; this cancels queued input, preserving the active turn and environment. Finally validate fake authenticated routes and request shapes, full touched crate suites, and `cargo clippy --all-targets -- -D warnings`, then deploy ahead of the supervisor feature.

## Outcomes & Retrospective

The API and CLI changes are validated, including an isolated daemon capability probe. Controller and CLI full suites were run; failing historical fixtures and CLI tests passed after focused corrections. Workspace Clippy passed. Matching release binaries were deployed to the MergeMarshall CI host after checksum verification. The read-only recovery capability probe passed and all 45 existing CI session IDs were preserved across the daemon handoff. No live session was reset during validation.
