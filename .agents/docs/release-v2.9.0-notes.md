## UI-first settings

Manage agent profiles, projects, SSH and EC2 connections, runtime overrides, and interface preferences through **F7 Settings**. The command palette also opens **Manage agent profiles** and **Manage machines and runtimes** directly, and target actions link to the selected target's settings. The CLI `mj setup` command remains optional.

## Live target availability

Standard local targets are supplied automatically without running setup. New, resume, and move target pickers check every target in the background. Unavailable or pending targets cannot advance, failures show a reason, and **F5** rechecks after the service or connection is repaired. Independent checks do not block each other or the UI. Saved target overrides remain authoritative, and implicit defaults survive configuration reloads and edits.

## Fixes

- Show Apple's container-service diagnostic from stdout as well as stderr, with the command to start the service.
- Stop claiming that launch cleanup failed merely because a failed session remains in the list.
- Allow adding Zcode profiles from Settings.
- Keep ZCode sessions lazy until the first prompt.
- Make model and effort controls clickable in the prompt pane.
- Put New bundle in the wizard action row and show worktree options only where applicable.
- Correct Settings shortcuts, guidance, and documentation screenshots.
- Update installer test fixtures for grouped Cargo builds.

No database migration is required; existing sessions and saved configuration remain compatible.
