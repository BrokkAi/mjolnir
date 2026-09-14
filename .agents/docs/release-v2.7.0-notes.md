## Highlights

- Drive sessions through the new `/api/v1` interface: create sessions with an initial prompt, page transcripts, wait for turn outcomes, upload files, and export completed work.
- Run and observe Mjolnir-managed subagents with durable event streaming, CLI replay, activity reporting, model selection, and native goal controls.
- Preserve Codex goals and managed task state across worker restarts and checkpoint recovery.

## Improvements

- Run fully isolated Mjolnir worlds with the new global `--instance` / `-i` flag, separating configuration, databases, daemons, and logs.
- Session creation now makes configuration and managed-worktree choices explicit, while discovery and provisioning remain cancellable.
- Turn diagnostics record provider usage and quota exhaustion more accurately across Codex, Claude, Grok, and Kimi integrations.
- Checkpoint exports are isolated, checksum evidence is reported, and compatible newer database schemas remain usable by older builds.
- Local installation and development scripts build the native worker and dictation helper alongside portable workers.

## Fixes

- Worker recovery no longer races checkpointed session destruction.
- Runtime launchers and session Git authentication survive installation relocation and branch export.
- Daemon startup failures are reported directly to the launching client instead of timing out without the underlying error.
- Imported sessions reconcile native task identities and preserve configured model pins.
- Daemon startup continues to work after the client executable is replaced.

Existing configuration and session history are preserved. No breaking database migration is required.
