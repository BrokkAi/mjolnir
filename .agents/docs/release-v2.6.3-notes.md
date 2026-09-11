Mjolnir 2.6.3 makes terminal dialogs predictable and fixes tool-result expansion clicks.

- **Unify terminal dialogs.** Lists select on a single click and activate on double-click, exactly like Enter; command menus still run on one click. Escape dismisses the innermost dialog first, and dialogs with unsaved edits ask to keep editing or discard instead of silently dropping drafts. Applies to setup, workspaces, palettes, wizards, resume, and review settings.
- **Fix tool expansion clicks.** Clicking a tool name now expands the correct call after wrapped reasoning text and when repeated tool names are partially scrolled.
