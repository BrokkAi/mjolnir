## Fixes

- Rerunning setup preserves existing profiles, bundles, targets, and preferences while adding newly discovered installations. Conflicting discoveries are made explicit instead of replacing working settings.
- Installed harnesses are detected before their first login creates a configuration directory.
- Missing or incompatible session configuration no longer prevents mj from starting. The terminal and web dashboards show repair guidance while keeping other sessions accessible.
- Setup protects configuration used by active sessions and allows missing dependencies to be restored.
- The new-session review no longer gets stuck with Create disabled. It checks prerequisites as soon as it opens, shows "Checking prerequisites…" while the check runs, and enables Create once the check passes.

## Improvements

- The terminal composer's microphone button is now in the upper-left corner, before Model and Effort.
- The terminal session list and transcripts use less CPU, because unchanged session ordering and transcript data are reused.

Existing configuration and session history are preserved. No migration is required.
