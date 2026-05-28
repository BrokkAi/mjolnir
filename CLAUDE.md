# Repository Guidelines

Follow the same repository guidance as `AGENTS.md`.

## UI Safety Requirements

Permission dialogs must never truncate requested permission content. Long commands, titles, descriptions, and option labels must remain fully readable through wrapping, scrolling, paging, resizing, or an equivalent explicit expansion path. This applies to both inline and fullscreen UI modes.

Do not recover from inline UI failures by falling back to the fullscreen TUI. That is a jarring mode switch and a poor user experience; inline terminal problems should be retried, degraded, or surfaced within inline mode instead.
