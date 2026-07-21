---
title: Thor, Eitri, and Loki
description: The responsibilities and interaction model of Mjolnir's coding Council.
---

## Thor

Thor owns the user turn. It coordinates the work, uses tools directly for small edits, and delegates bounded implementation tasks when a fresh context is valuable.

## Eitri

Eitri implements delegated work or explores a codebase. During a coding handoff, its fresh ACP session streams through the normal Mjolnir interface and returns its final response and diff to Thor.

## Loki

Loki is a long-lived, read-only reviewer. It observes transcript checkpoints asynchronously and returns advice at natural turn boundaries without interrupting an active agent.

The exact selection rules, review timing, and fallback behavior will be documented here as the reference grows.
