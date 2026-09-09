Mjolnir 2.6.0 adds easier updates, more control over background work, and a more compact terminal interface.

- **Install-aware updates:** interactive startup checks for updates at most once a day and asks before upgrading. Release-installer updates verify checksums and replace the full binary bundle; npm and Homebrew use their own upgrade commands. Cargo and ephemeral npx installs show the appropriate command. Set `MJOLNIR_NO_UPDATE_CHECK` to opt out.
- **Stop individual background tasks:** the conversation TUI and web viewer can stop supported Claude background tasks and Mjolnir-hosted shell tasks without stopping the session or sibling tasks. Pending requests and failures remain visible. Codex background tasks remain read-only until its adapter supports targeted cancellation.
- **Protect Kimi background agents:** detached agents remain visible and keep automatic worker upgrades, routine checkpoints, recovery copies, and session moves from interrupting live work. Uncertain task state also defers those operations.
- **Compact Setup and consistent controls:** inline choices and shared comboboxes reduce nested dialogs. Terminal dialogs have more consistent dismissal behavior, and agent questions leave more of the conversation visible.
- **Conversation polish and themes:** refined prompt-composer spacing, model and reasoning-effort controls above the web composer, and new IntelliJ-inspired Darcula and high contrast themes.
- **Restored session details:** quota-reset columns and advanced stopped-session visibility are back.

Internally, shared client contracts now live in `brokk-mj-client`, separating terminal and chat code from controller implementation dependencies.

[Full changes since v2.5.0](https://github.com/BrokkAi/mjolnir/compare/v2.5.0...v2.6.0)
