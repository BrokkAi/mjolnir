# Add an Auto theme, colorblind-safe and high-contrast light palettes, and Catppuccin

This ExecPlan is maintained according to `.agents/PLANS.md`. It builds on `.agents/plans/ui-themes.md`, which introduced saved terminal themes.

## Purpose / Big Picture

The terminal dashboard has twelve palettes, chosen in Settings (F7) → Interface → Theme and saved to `theme` in `config.toml`. Users must still pick a dark or light palette by hand to match their terminal, every palette marks failure in red beside green success and amber activity, which the most common color-vision deficiencies cannot tell apart, the only high-contrast palette is dark, and users of the most widely used community scheme, Catppuccin, see a dashboard whose background does not meet their terminal's padding.

After this change Theme offers six more choices. Auto asks the terminal for its background color when mj starts and draws Midnight on a dark background and Light on a light one; Settings says which it chose and why. Colorblind Dark and Colorblind Light carry status in blue, yellow, and orange and keep failure distinct from success, waiting, and activity under simulated protanopia, deuteranopia, and tritanopia. High Contrast Light is black on white. Catppuccin Mocha and Catppuccin Latte paint the scheme's own base color.

To see it work, put `theme = "auto"` in an isolated instance's `config.toml`, start `mj --instance <name>` in a light terminal, and observe the Light palette; in a dark terminal, Midnight.

## Progress

- [x] (2026-10-20) Surveyed palettes, config, Setup, terminal entry, goldens, and the PTY harness.
- [x] (2026-10-20) Designed and measured the five palettes with a contrast and color-vision script.
- [x] (2026-10-20) Added `ThemeChoice` and the new `UiTheme` variants to the configuration.
- [x] (2026-10-20) Added the palettes, the terminal background fact, and Auto resolution in `mj-chat`.
- [x] (2026-10-20) Probed the terminal background once in `TerminalGuard::enter`.
- [x] (2026-10-20) Offered Auto in Settings, reported its resolution, and scoped theme labels to the theme field.
- [x] (2026-10-20) Extended goldens (macOS variants derived from Linux), the contrast floor, the PTY end-to-end test, and docs.
- [x] (2026-10-20) Validated: full suites of brokk-mj-core, brokk-mj-chat, brokk-mj-tui, and brokk-mjolnir, workspace Clippy with warnings denied, and formatting pass. Committed on the current branch.

## Surprises & Discoveries

The dashboard reads terminal input through crossterm's `EventStream`. crossterm 0.29 parses an unrecognized `ESC ]` sequence as Alt plus the following characters, so a background reply reaching it would type `]11;rgb:…` into the composer. The probe therefore reads the terminal itself before crossterm's reader exists, and stops only after the terminal's answer to a Primary Device Attributes query (DA1, `ESC [ c`), which every VT-compatible terminal answers after any earlier query. `TerminalGuard::enter` already sends crossterm's keyboard-enhancement query the same way, and is entered once per process.

Settings labels any string value that parses as a theme with that theme's label. `auto` is also an image pull policy, so the label lookup must be scoped to the `theme` field.

The PTY fixture terminal now answers the background query with white. At its 80-column size the panels cover the canvas, so the end-to-end case checks Light's panel surface (251, 250, 247) rather than its canvas color.

Settings reports in test renders always read `Monochrome (NO_COLOR; …)`, because `mj-tui/src/render.rs::render` pins a NO_COLOR override for tests, so the Settings golden cannot show Auto's own wording; the `theme_report` assertions in `setup_reports_monochrome_while_no_color_overrides_the_theme` cover it.

The macOS goldens `active-chat-render-macos.txt` and `config-selection-render-macos.txt` differ from the Linux files only by `Cmd-V paste` for `Ctrl-V paste`, so they were derived from the regenerated Linux files with that substitution; both pairs changed by the same pure additions.

Midnight's activity and attention colors are only 4.4 apart in OKLab distance (scaled by 100) even for normal vision, and Light's success and error fall to 5.3 under simulated deuteranopia; each session state also has its own glyph, so color is a second cue there. The colorblind palettes keep every status and session pair at least 9.1 apart under all three simulations.

## Decision Log

- Decision: Store the setting as a new `ThemeChoice` (`Auto` or `Fixed(UiTheme)`) and keep `UiTheme` as the identity of a drawable palette.
  Rationale: Render scopes, palette lookup, and transcript caches take a `UiTheme`. Keeping Auto out of that type makes it impossible to render "Auto" or cache rows under it; only `mj_chat::theme::effective_theme` turns a choice into a palette.
  Date/Author: 2026-10-20 / Claude.
- Decision: Probe once per process, at the first `TerminalGuard::enter`, whether or not Auto is configured, and record the answer in a `OnceLock` owned by `mj_chat::theme`.
  Rationale: Settings can switch to Auto mid-session, when crossterm owns the input and no probe is possible. One owner records the fact; every resolution reads it.
  Date/Author: 2026-10-20 / Claude.
- Decision: Auto picks between Midnight and Light by whichever palette's text contrasts more with the reported background, and uses Midnight when the terminal does not answer; Settings names the reason.
  Rationale: No invented threshold, and the fallback is the existing default and is reported rather than hidden.
  Date/Author: 2026-10-20 / Claude.
- Decision: Windows does not probe.
  Rationale: crossterm reads the Windows console as input records, not as a byte stream that could carry the reply. Auto there reports that the terminal did not answer and draws Midnight.
  Date/Author: 2026-10-20 / Claude.
- Decision: Assert in the theme golden that every painted palette's foreground roles reach 4.5:1 on all four surfaces.
  Rationale: The golden records each contrast, but a regenerated golden would accept a regression silently.
  Date/Author: 2026-10-20 / Claude.

## Outcomes & Retrospective

Settings offers Auto and five new palettes. Auto resolves through one function, `mj_chat::theme::resolve`, from a background the first terminal entry records once; render scopes and caches never see Auto because the setting and the palette are different types. The real `mj` binary in a pseudo-terminal draws Light when the terminal reports white, and the reply never reaches the input. Every painted palette now must keep 4.5:1 on all surfaces, enforced in the theme golden.

Remaining limits: Auto does not follow a terminal that changes scheme while mj runs; Windows outside WSL never probes and draws Midnight; keys typed in the instant between the query and its reply are consumed with the reply; and, as with the themes added before, an older mj cannot read a configuration saved with one of the new names. The probe was not exercised on macOS or Windows here; their CI lanes run on this change.

## Context and Orientation

`mj-core/src/config/ui.rs` defines `UiTheme`, serialized kebab-case (`midnight`, `light`, …) with `ALL` and `label`. `mj-core/src/config.rs` holds `Config.theme` and its on-disk twin `StoredConfig.theme`, which skips serializing the default. `mj-chat/src/theme.rs` defines `Palette` (seventeen color roles, eight pin colors, and `reverse_video`), one `const` per palette, `palette_for`, the thread-local `with_theme` render scope, `effective_theme` (which applies `NO_COLOR`), and the `golden_theme_rendering` test that records every foreground on every surface with its WCAG contrast ratio. `mj-tui/src/combined.rs::render_combined` and `mj-cli/src/dashboard.rs::DashboardStartup::background` resolve the configured theme. Settings is `mj-tui/src/setup.rs` with `setup/schema.rs` supplying choices (`choices`), labels (`choice_label`), the theme report (`theme_report`), and help. `mj-cli/src/main.rs::TerminalGuard::enter` enables raw mode and starts terminal modes; `DashboardScreen::enter` calls it once and then creates crossterm's `EventStream`. `mj-cli/tests/termination_pty.rs` runs the real `mj` binary in a pseudo-terminal and answers the device-attributes query.

## Plan of Work

Configuration: add `HighContrastLight`, `ColorblindDark`, `ColorblindLight`, `CatppuccinMocha`, and `CatppuccinLatte` to `UiTheme` and `ALL`. Add `ThemeChoice` with `ALL` (Auto first, then each palette), `label`, `is_default`, and string serialization: `auto` or the palette's own name, so every existing file and the `dracula` alias load unchanged. `Config.theme` and `StoredConfig.theme` become `ThemeChoice`.

Palettes: add the five constants with the measured values, map them in `palette_for`, and add the terminal background record: `record_terminal_background(Option<Rgb>)` sets a `OnceLock`, `auto_theme_for(Option<Rgb>)` chooses by text contrast, and `resolve(ThemeChoice, no_color, background) -> Resolution` returns the theme and why. `effective_theme(ThemeChoice)` resolves with the process facts.

Probe: a new `mj-cli/src/terminal_background.rs` writes `ESC ] 11 ; ? ESC \` then `ESC [ c` to standard output and reads the terminal (standard input when it is a terminal, otherwise `/dev/tty`, the same descriptor crossterm reads) with `poll` until the DA1 reply arrives or two seconds pass, then parses `rgb:` or `rgba:` with one to four hex digits per channel. `TerminalGuard::enter` calls it after raw mode and before crossterm's keyboard query, and records the answer.

Settings: choices come from `ThemeChoice::ALL`; `choice_label` interprets theme names only for the `theme` path; `theme_report` takes the resolution and says, for Auto, which palette is on screen and why.

Tests and docs: the per-theme goldens regenerate with the new palettes (the macOS variants are extended by hand from the Linux sections, as `1fd2e78e0` did); the theme golden asserts the 4.5:1 floor and records Auto's resolution for a dark, a light, and an unreported background; the PTY harness answers the background query, and an end-to-end case starts `mj` with `theme = "auto"` against a light reply and finds Light's painted background. Update `docs/src/content/docs/terminal-surface.mdx` and `mj-core/assets/skills/mj/references/configuration.md`.

## Concrete Steps

Work in the repository root. Run, outside the sandbox:

    cargo test -p brokk-mj-core config
    cargo test -p brokk-mj-chat theme
    cargo test -p brokk-mj-tui setup
    cargo test -p brokk-mjolnir --test termination_pty auto_theme
    cargo test
    cargo clippy --all-targets -- -D warnings
    cargo fmt --all -- --check

Regenerate goldens with the repository's golden update variable, then review every diff.

## Validation and Acceptance

An existing configuration loads with the same theme and is not rewritten. Every choice, Auto included, survives a save and load. In the PTY end-to-end case, `mj` with `theme = "auto"`, answered with a white background, paints Light's panel surface, never Midnight's, and the reply never reaches the screen. The theme golden records Auto's choice for white, Solarized Light, Solarized Dark, Catppuccin Mocha, mid gray, and no answer. Settings shows `Auto: Light (terminal background #fdf6e3)` or `Auto: Midnight (terminal did not report its background)`. Every painted palette passes the 4.5:1 assertion.

## Idempotence and Recovery

Tests use isolated data and configuration directories and `--instance` names; nothing touches the default instance. Re-running the steps is safe.

## Artifacts and Notes

Measured with the scratch script (OKLab distance ×100 between the closest pair; status is success, warning, error; session is error, activity, attention, idle):

    Midnight          deutan status 7.7, session 4.3 (4.4 for normal vision)
    Light             deutan status 5.3 (success/error)
    Colorblind Dark   every simulation: status >= 20.7, session >= 9.1
    Colorblind Light  every simulation: status >= 12.1, session >= 11.8

## Interfaces and Dependencies

No new dependencies; the probe uses `libc`. At the end:

    mj_core::config::ThemeChoice { Auto, Fixed(UiTheme) }
    mj_chat::theme::record_terminal_background(Option<(u8, u8, u8)>)
    mj_chat::theme::effective_theme(ThemeChoice) -> UiTheme
    mj_chat::theme::resolve(ThemeChoice, no_color: bool, background: Option<Option<(u8, u8, u8)>>) -> Resolution
