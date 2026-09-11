## Fixes

- Rerunning setup preserves existing profiles, bundles, targets, and preferences while adding newly discovered installations. Conflicting discoveries are made explicit instead of replacing working settings.
- Installed harnesses are detected before their first login creates a configuration directory.
- Missing or incompatible session configuration no longer prevents mj from starting. The terminal and web dashboards show repair guidance while keeping other sessions accessible.
- Setup protects configuration used by active sessions and allows missing dependencies to be restored.

Existing configuration and session history are preserved. No migration is required.
