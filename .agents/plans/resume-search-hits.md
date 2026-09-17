# Resume dialog search: fast open, cross-tab hits, transcript hits, loading state


## Purpose


The resume dialog is the TUI window that lists sessions which are not live: the
Mjolnir tab lists Mjolnir's own stopped records, the Import tab lists native
sessions found in each coding tool's home directory, and the Archived tab lists
sessions only SessionWiki still holds. SessionWiki is a session search index
that Mjolnir links as a Rust library (crate `brokk-sessionwiki`), so there is no
external command involved. One search box above the tabs queries that index.

Today the dialog is slow to open, hides where a query's hits are, shows a
transcript summary instead of the matching passage in a six-row pane that
cannot scroll, and gives no sign of whether a search is still running. After
this plan a person opening the dialog a second time sees it at once, sees per
tab how many hits a query has, sees the matching passages with the matched
words highlighted in a scrollable pane, and sees the list say "Searching…"
and then "N matches". A settings field that read "Automatic / default" now
says what an empty value does.


## Orientation


The dialog's state and rendering live in `mj-tui/src/resume.rs` (tests in
`mj-tui/src/resume/tests.rs`). The TUI never touches the filesystem or the
daemon directly: it returns a `DashboardAction`, and `mj-cli/src/dashboard/`
turns actions into background tasks (`actions.rs`, `io/spawn.rs`) whose results
come back as `DashboardIoUpdate` values handled in `io.rs`. The daemon is the
long-running Mjolnir process; the TUI talks to it over a Unix socket through
`mj-client/src/daemon.rs`. Daemon-side SessionWiki code is in
`mj-controller/src/sessionwiki.rs`, reached through `mj-controller/src/daemon/state.rs`
and `daemon/actions.rs`. The native-session scan for the Import tab lives in
`mj-controller/src/import/` and is driven from `mj-cli/src/import.rs` and
`mj-cli/src/dashboard/chat_tasks.rs`.


## Findings that shaped the plan


The "scanning N/M" text at open is the Import scan, not SessionWiki indexing.
`start_resume_discovery` in `mj-cli/src/dashboard/chat_tasks.rs` spawns one
blocking task per enabled profile. The Claude scanner in
`mj-controller/src/import/claude.rs` walks `~/.claude*/projects` and parses
every JSON line of the newest 50 transcripts to find a title record, the working
directory, the branch and filter markers, because a title record can sit on
any line. On the author's machine that is about 100 MB per Claude home, three
homes, on every open, with no cache. The Codex scanner is cheap when Codex's
`state_5.sqlite` exists. Each scanned file also clones the whole growing
profile and sends it down a 32-slot channel (`mj-cli/src/import.rs`,
`discover_import_profile`), which is quadratic in the session count.

`build_resume_rows` in `resume.rs` filters to the current tab after
`merged_resume_rows` has already ranked every hit, so the other tabs' hit
counts are computed and then discarded.

The preview pane shows `brief_markdown` from the SessionWiki crate, a head-and-
tail summary capped at 4 000 characters (`WIKI_BRIEF_CHARS` in
`mj-cli/src/dashboard/io/spawn.rs`), drawn in a fixed six-row `Paragraph` with
no scroll offset, no scrollbar and no mouse surface. Its first line is
`# Previous session: <title>`, which is the crate's own heading, present for
every tool, not written by Codex or Mjolnir.

A search is one request and one reply (`spawn_wiki_search` in `spawn.rs`). The
appearance of results streaming in comes from `next_wiki_refresh` in
`resume.rs` re-running the same query every two seconds while the daemon
reports `topping_up` (a sync is running), at most ten times, replacing all rows
each time.

In Settings, `Archive after (days)` shows "Automatic / default" for an empty
value, but an empty value means never archive (`mj-core/src/config.rs`,
`SessionWikiConfig`; `mj-controller/src/server_runtime/run.rs` runs only the
hourly sync when it is `None`). There is no hidden number.


## Milestone 0: name the real effect of an empty setting


The settings screen (`mj-tui/src/setup.rs` and `mj-tui/src/setup/schema.rs`)
renders every JSON null as "Automatic / default" in two places. Add
`schema::null_label(path) -> &'static str` and call it from both. For
`archive_after_days` return "Never". For other optional fields, return the
effective value when it is known and fixed; keep "Automatic / default" when the
default really is decided elsewhere at runtime. Do not introduce a numeric
archive default: that would start deleting checkpoints on existing installs.
Acceptance: opening Settings, the SessionWiki section shows
`Archive after (days)   Never`; a unit test `empty_archive_after_days_renders_as_never`
covers the label.


## Milestone 1: make the import scan cheap, and show it only on the Import tab


Add a scan cache so a second open parses nothing that has not changed. In
`mj-controller/src/import/native.rs` define a `NativeScanCache`, a map from file
path to (modified time, size, cached metadata), shared as `Arc<Mutex<_>>`. The
dashboard context in `mj-cli/src/dashboard.rs` owns one for the process
lifetime and passes it through `discover_import_profile` into
`scan_native_sessions`. When a candidate's modified time and size match the
cache entry, reuse the cached result of `claude_native_metadata` (including the
"filtered out" verdict) instead of opening the file. This is safe because that
function is a pure function of the file's content. The Codex per-file reader
`codex_session_metadata` can use the same cache; the sqlite fast path needs
none.

On a cache miss, make the parse cheaper: in `claude_native_metadata` test each
line for the byte substrings of the keys the function actually reads before
calling `serde_json::from_str`, and skip lines that contain none. Read the
function body for the exact key list and keep behaviour identical.

Stop the quadratic publishing: in `discover_import_profile`, publish the growing
profile at most about every 100 ms plus once at the end, not after every file.

Move the scan notice into the Import tab. Remove the `scanning N/M` suffix from
the dialog title and the `Scanning…` spinner drawn over the footer in
`render_resume_dialog`. While `is_scanning()` is true, the Import tab label
reads ` Import · scanning N/M ` and the Import list panel title reads
` Importable sessions · scanning N/M ` with the spinner. `needs_fast_tick`
animates for the scan only while the Import tab is showing. The Import-only
empty message and the Import-only scan error line stay as they are.

Acceptance: open the dialog twice in one TUI process; the second open shows no
scan text beyond a flicker. Unit tests prove that a second scan over an
unchanged directory opens no files and that a changed modified time re-parses,
using a multi-line fixture with the title record on the last line.


## Milestone 2: show hits on the other tabs


When the search box is non-empty, count merged rows that have a `wiki_rank`
per tab before the tab filter in `build_resume_rows`, store the three counts on
`DashboardState` beside `resume_rows`, and render them in the tab labels:
` Mjolnir · 3 `, ` Import · 12 `, ` Archived · 0 `. When the current tab has no
hits but another does, the empty-list message reads
`No matches here · 12 on Import, 3 on Archived`. Never switch tabs
automatically. Acceptance: a test `search_counts_hits_on_every_tab` over
`merged_resume_rows` fixtures, and the labels visible in the TUI.


## Milestone 3: a preview pane that scrolls


Add `preview_scroll: usize` and `preview_key: Option<String>` to `ResumeDialog`;
reset the scroll to zero when the previewed SessionWiki id changes. Replace the
fixed `PREVIEW_HEIGHT` with `max(6, inner.height * 2 / 5)` in `resume_bands`,
keeping `resume_sessions_pane` in step because pointer hit-tests use it. Add
`SurfaceId::ResumePreview` in `mj-chat/src/selection.rs` and register a
scrollable surface for the pane body after the list surface. In
`handle_resume_dialog_event`, before the form sees the event, hit-test mouse
wheel events against the pane and move `preview_scroll` by the shared
`MOUSE_SCROLL_ROWS` step, clamped to the wrapped line count minus the viewport.
Add PgUp and PgDn for the pane while focus is on the list. Wrap the text to the
pane width yourself (look for an existing wrap helper in `mj-chat` first) so the
clamp and scrollbar are exact, and render with `Paragraph::scroll`. Paint the
scrollbar with `render_session_scrollbar` from `mj-tui/src/render/sessions.rs`.
Acceptance: wheel over the pane scrolls it and stops at the end; wheel over the
list still moves the list; a scrollbar appears when the text overflows.


## Milestone 4: show the matching passages, not a summary


When a query is active, the pane shows every message that matches, with one
message of context on each side, `*[… N messages omitted …]*` separators between
groups, matched text highlighted, and a title ` Transcript · hit 1/7 `. Keys
`n` and `N` (and `]`, `[`) move between hits. With no query the pane keeps the
brief but drops its first line, which is the crate's `# Previous session:`
heading and duplicates the selected row's title.

Daemon side, in `mj-controller/src/sessionwiki.rs` beside `brief`, add
`transcript_hits(id, query, context_messages, per_message_chars)`. Load
messages with `sessionwiki::index::session_from_index`, redact them the way
`brief_markdown` does, and find matches with a case-insensitive substring
search on NFC-normalised text. That reproduces the index's matches because the
index's full-text table uses a trigram tokenizer, which is substring matching
for queries of three or more characters, and shorter queries already go
through a `LIKE` search. Cap each message at `per_message_chars`, keeping the
window around its first hit. Return `WikiHitTranscript { blocks, omitted_before }`
where each block is `{ role, text, hits: Vec<(start, end)> }` with byte offsets
into `text`.

Wire it exactly like `wiki_brief`: a `DaemonAction` and `DaemonReply` variant
in `mj-client/src/daemon.rs`, a `RuntimeState` method in `daemon/state.rs` that
runs it through `blocking`, the trait method in `server_runtime/api.rs` with an
unavailable default (and the `subagent_backend` stub), a client method, a
`spawn_wiki_hits` task and `DashboardIoUpdate::WikiHits` in the CLI, and
`apply_wiki_hits` on the dialog, cached by (wiki id, query) beside `previews`.
`next_wiki_brief` becomes `next_wiki_preview` and asks for hits when the query
is non-empty and a brief otherwise. Render blocks as lines of spans with the
matched ranges in the accent colour and bold; on first render of a (wiki id,
query) pair set `preview_scroll` to the first hit's line.

Acceptance: daemon tests `transcript_hits_locates_case_insensitive_matches`
and `transcript_hits_keeps_context_and_marks_omissions`; a TUI test that `n`
advances `preview_scroll` to the next hit; in the TUI, selecting a hit opens the
pane on the first highlighted match.


## Milestone 5: say when a search is running and when it is done


Add `wiki_pending: bool` to `ResumeDialog`, set in `next_wiki_search` and
`next_wiki_refresh`, cleared in `apply_wiki_search` when the request id
matches, and included in `needs_fast_tick`. While a query is active the list
panel title reads ` Searching… ` with the spinner when pending and no rows have
arrived, ` 12 matches · index syncing, more may arrive ` when rows are present
and the daemon reports `topping_up`, ` 12 matches ` when final, and
` Index building… ` while the index state is `Indexing`. Replace the fixed ten
polls at two seconds with a backoff of 2, 4, 8 and then 10 seconds that
continues while `topping_up`, so a long sync no longer leaves the pane
promising more results with no further polls. Acceptance: a test
`search_reply_clears_pending_for_matching_request_only`; in the TUI the title
moves from Searching… to N matches.


## Validation


From the repository root run `cargo test` outside the sandbox and
`cargo clippy --all-targets -- -D warnings`, both on the dev profile, after each
milestone. Then, with the local daemon running, open the resume dialog twice
and confirm the second open is immediate; type a query with hits on more than
one tab and confirm the tab counts and the empty-tab message; select a hit and
confirm the pane scrolls with the wheel, shows a scrollbar, opens on the first
highlighted hit and moves to the next on `n`; watch the panel title move from
Searching… to N matches. Commit each milestone on the current branch once its
checks pass.


## Progress


- [x] Milestone 0: null label (commit c7a43e16)
- [x] Milestone 1: scan cache, cheaper parse, throttled publish (c90b0226); notice on Import tab (2644a5f4)
- [x] Milestone 2: per-tab hit counts (2644a5f4)
- [x] Milestone 3: scrollable preview (2644a5f4)
- [x] Milestone 4: daemon and client call (26fd74ee)
- [x] Milestone 4: CLI and TUI wiring
- [x] Milestone 5: search pending/complete state (2644a5f4)


## Surprises & Discoveries


The "# Previous session:" heading is generated by the SessionWiki crate's
brief renderer (`commands.rs` in `brokk-sessionwiki-0.28.0`). A search of every
Codex home, Codex's session index, and the SessionWiki database on the author's
machine found zero stored occurrences.

The Claude scanner's line prefilter works because the position keys (`cwd`,
`gitBranch`, `entrypoint`) drop out of the test once their value is known,
usually after the first line; nearly every transcript line carries `"cwd"`,
so testing it unconditionally would skip nothing. The `isSidechain` marker
is tested for the value `true`, since `"isSidechain":false` is on every line.

mj-chat already had a grapheme-aware `wrap_styled_line`; it was `pub(super)`.
Making it public replaced a 90-line word-wrap helper first written in the
dialog. The pane wraps its own rows and renders with `Paragraph::scroll` so
the scroll clamp, the scrollbar and the drawn rows count the same thing.

Adding the daemon call moved the daemon protocol version from 21 to 22, as
an earlier action-shape change did; an older running daemon is treated as
incompatible until restarted.


## Decision Log


The Import list keeps its own scan rather than being rebuilt from SessionWiki
rows: the index has no branch, size, availability reason or modified-time
ordering, has no adapter for the Kimi, Grok or Muse profiles, and lists at most
50 sessions by start time. Caching the scan gets the speed without those gaps.

The transcript preview shows a hit-centred excerpt rather than the whole
transcript, so the reply stays bounded and every hit is reachable.

An empty `archive_after_days` is labelled "Never" rather than given a numeric
default, because a default would begin deleting checkpoints on existing
installs.


## Outcomes & Retrospective


All six milestones landed on branch hel2 in five commits. A second open of
the resume dialog in the same TUI process reopens no unchanged transcript;
the first open parses only lines that can carry a key the scanner reads.
A query shows its hit count on every tab label, an empty tab says where the
hits are, and the list title moves from Searching… through "N matches ·
index syncing" to "N matches". The preview pane takes two fifths of the
dialog, scrolls with the wheel and page keys, paints a scrollbar, and with a
query shows the matching passages with the matched text highlighted and
`n`/`N` moving between hits. Settings says "Never" for an empty archive age.

Lesson: the slow part of the dialog was a scan unrelated to the search index
it appeared to be reporting on. Measuring what actually ran (about 100 MB of
JSON per Claude home per open) settled the design faster than reasoning
about the index. Reusing mj-chat's wrap helper instead of a local one kept
the scroll clamp, scrollbar and drawn rows in one definition.

Remaining: hit navigation uses the pane geometry from the last rendered
frame, which is correct because a resize redraws before the next keystroke.
An older daemon must restart before the new hits call is available, because
the daemon protocol version moved to 22.
