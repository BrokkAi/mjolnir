# Split the conversation area into tiled panes


This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained according to `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture


Today the terminal dashboard (`mj`) shows one conversation at a time. The conversation sits in the band between the Sessions sidebar on the left and the Targets and Quota panes below. After this change the user can split that band into several panes, side by side or stacked, each showing a different session's conversation. Every pane's transcript advances as its agent replies, the user types into the focused pane, and the arrangement is saved per workspace so it comes back after a restart.

To see it working after the whole plan lands: start `mj` with at least two sessions, open the `⋯` menu on a session row in the Sessions pane, and choose "Open in split right". The conversation area now shows two conversations. Both update as their agents reply. Pressing F2 and choosing "Close pane" removes one. Quitting and restarting `mj` restores the two-pane layout with the same sessions.

## Progress


- [x] (2026-09-17) Design agreed with the user; plan written as this ExecPlan (M0).
- [ ] M1: the layout tree in `mj-tui/src/tile_layout.rs`, with unit tests.
- [ ] M2: the persisted per-workspace layout: type in `mj-core`, schema migration 38, database functions, daemon action, client method, generalized save coordinator in `mj-cli`.
- [ ] M3: many warm chats in the terminal controller (`mj-cli`), keyed by session id, all pumped and acknowledged.
- [ ] M4: rendering of several panes and the pane commands in `mj-tui`, reachable from the F2 palette and the session row `⋯` menu.
- [ ] M5: prefix keybindings. Blocked until the prefix-key work from the `hel3` worktree merges into this branch.

## Surprises & Discoveries


None recorded yet.

## Decision Log


- Decision: Tile only the conversation region (transcript plus prompt). The Sessions, Workspaces, Targets, and Quota panes and the footer stay where they are.
  Rationale: The user wants several conversations visible at once; the support panes already have their own sizing model (`PaneSizes`) and do not need to move.
  Date/Author: 2026-09-17, user and Fable.

- Decision: A split is created with a session in it. "Open in split" is an action on a session in the Sessions pane, not a bare "split" that then needs filling.
  Rationale: The session is the unit the user thinks in; the pane is where it shows. Keyboard splits (M5) act on the Sessions selection for the same reason.
  Date/Author: 2026-09-17, user.

- Decision: Every chat in the layout stays warm: each is pumped every loop iteration and each visible one acknowledges reads.
  Rationale: The user's model is that every conversation is a projection over events processed regardless of what is on screen. The data layer already works that way (sessions run and the controller ingests their events whether or not a chat is open); only the view instance was single.
  Date/Author: 2026-09-17, user.

- Decision: Keys follow herdr's tmux-style prefix, and the prefix router is built once in the `hel3` worktree (its plan is `.agents/plans/prefix-keybindings.md` there). This plan does not build a second router. M5 waits for that work to merge and then adds actions through its `key_actions!` macro.
  Rationale: Two prefix routers would conflict on the same keys.
  Date/Author: 2026-09-17, user.

- Decision: Reserve herdr's pane letters in the prefix map: `v`, `minus`, `x`, `h`, `j`, `k`, `l`, and `shift+h/j/k/l`. The `hel3` letter map currently gives `prefix+v` to dictation, `prefix+x` to cancel-operation, and `prefix+j`/`prefix+k` to support-pane cycling. Before `hel3`'s second milestone lands, that map changes so those letters stay free. Proposed replacements for the `hel3` session to confirm: dictation `prefix+m`, cancel-operation `prefix+shift+x`, support-pane cycling keeps only `prefix+tab` / `prefix+shift+tab`. That change is made in the `hel3` worktree, not here.
  Rationale: Users coming from herdr expect the same letters for pane operations.
  Date/Author: 2026-09-17, user.

- Decision: The layout persists per workspace in a new table, mirroring the existing per-workspace pane-size path end to end.
  Rationale: The pane-size path already solves ordered background saves, failure notices, and a bounded flush on quit. Reusing it keeps one persistence design.
  Date/Author: 2026-09-17, Fable.

- Decision: Pane ids are allocated per layout (a counter field), not from a process-wide atomic as in herdr.
  Rationale: Layouts are saved and restored with their ids; a per-layout counter restores deterministically and needs no global state.
  Date/Author: 2026-09-17, Fable.

- Decision: A split is refused when either resulting pane would be under 40 columns or 8 rows. The dashboard shows the notice "Not enough room to split".
  Rationale: A conversation pane narrower than that cannot show a usable prompt and transcript.
  Date/Author: 2026-09-17, Fable.

- Decision: Out of scope for this plan: zoom, mouse-drag resize of split borders, text selection in an unfocused pane (clicking it focuses it first), swapping panes, and the same session shown in two panes.
  Rationale: Each is separable and none is needed for the core behaviour.
  Date/Author: 2026-09-17, user and Fable.

## Outcomes & Retrospective


To be written at each milestone's completion.

## Context and Orientation


This repository is a Cargo workspace. The crates that matter here:

- `mj-core` holds data types shared by every other crate, including the workspace model in `mj-core/src/workspace.rs`. It does not depend on the terminal UI library `ratatui`.
- `mj-tui` is the terminal user interface. `mj-tui/src/lib.rs` defines `DashboardState`, the model behind the dashboard screen. `mj-tui/src/combined.rs` draws the whole screen in `render_combined`. `mj-tui/src/actions.rs` lists every user command as a `CommandId` with a `CommandSpec` (title, scope, help text); the F2 command palette, the F1 help screen, and the footer are all generated from that table. `mj-tui/src/palette.rs` builds the palettes, including the per-session `⋯` menu in `begin_session_palette`.
- `mj-chat` holds `ActiveChat`, the live view of one session's conversation: its transcript, prompt composer, and the background feeds that keep it current. `ActiveChat::draw_in(ChatRegions { transcript, prompt, footer, overlay })` in `mj-chat/src/chat/active/surfaces.rs` draws into arbitrary rectangles, which is what makes several chats on one screen possible without changing `mj-chat` much. `ActiveChat::pump` in `mj-chat/src/chat/active/io.rs` awaits the chat's feeds and is cancel-safe, meaning it can be dropped mid-await and resumed later without losing anything.
- `mj-cli` is the terminal program. `mj-cli/src/dashboard.rs` owns `DashboardContext`, the controller that runs the event loop, holds the `DashboardState` from `mj-tui`, and today holds one `active_chat: Option<ActiveChat>`. Its submodules under `mj-cli/src/dashboard/` handle drawing (`surface.rs`), attaching to a session (`attachment.rs`), chat lifecycle tasks (`chat_tasks.rs`), input (`io.rs`, `actions.rs`), and the pane-size save coordinator (`pane_sizes.rs`).
- `mj-controller` owns the SQLite database (`mj-controller/src/database.rs` and `mj-controller/src/database/`) and the daemon, the background process that owns the database writer. The daemon's request types are in `mj-controller/src/daemon/actions.rs`; the client side is `mj-client/src/daemon.rs`.

Terms used below. A "pane" is one rectangle in the conversation area that shows one session's chat or is empty. A "leaf" is the same thing seen from the layout tree. A "split" divides one pane into two, either side by side (a vertical divider; herdr calls this `prefix+v`) or stacked (a horizontal divider; `prefix+minus`). The "focused pane" is the one the user is typing into. A "warm chat" is an `ActiveChat` that exists and is pumped, whether or not it is drawn. "Binary space partition" (BSP) is the tree shape used for the layout: every inner node is a split with a first and a second child and a ratio giving the first child's share; every leaf is a pane.

The sibling repository `../herdr` (Apache-2.0) has the same tiling in `../herdr/src/layout.rs`: `PaneId`, `Node`, `TileLayout`, `find_in_direction`, `split_rect`, `SplitBorder`, `NavDirection`, and a set of unit tests. mj is GPL-3.0, so the port is written fresh in mj's style rather than pasted.

What is single in mj today, and where:

- `DashboardContext.active_chat: Option<ActiveChat>` at `mj-cli/src/dashboard.rs:234`, one `SessionAttachment` (`mj-cli/src/dashboard/attachment.rs`), one `opening_chat_session`, and the draw filter in `mj-cli/src/dashboard/surface.rs` that hides the chat unless it matches the Sessions selection.
- The event loop's `select!` at `mj-cli/src/dashboard.rs:590` has one arm `ActiveChat::pump(context.active_chat.as_mut())`, followed by `acknowledge_visible_chat`.
- `render_combined` in `mj-tui/src/combined.rs` computes one transcript band and one prompt band (around line 566) and stores them in `DashboardState.chat_transcript_area` and `chat_prompt_area`.
- The text-selection engine registers one `SurfaceId::Transcript`.

The existing pane-size persistence path, which M2 mirrors:

- Type: `PaneSizes` in `mj-core/src/workspace.rs` with `validate()`.
- Schema: table `workspace_pane_sizes` created by an earlier migration in `mj-controller/src/database/schema.rs`; the current `SCHEMA_VERSION` is 37 in `mj-controller/src/database.rs:34`; the compatibility floor (the oldest schema an older build may still read and write) is 32.
- Database functions: `load_workspace_pane_sizes(_from)` and `save_workspace_pane_sizes(_to)` in `mj-controller/src/database/workspaces.rs`, tested in `mj-controller/src/database/tests.rs` (`workspace_pane_sizes_*`).
- Daemon: `DaemonAction::SaveWorkspacePaneSizes` in `mj-controller/src/daemon/actions.rs:150` and `mj-client/src/daemon.rs:380`, with the client method near `mj-client/src/daemon.rs:1078`.
- Coordinator: `PaneSizePersistence` in `mj-cli/src/dashboard/pane_sizes.rs`, which keeps a `tokio::sync::watch` channel of the latest requested values, a supervised save task, failure notices, and a five-second bounded flush in `finish`. Startup loads pane sizes around `mj-cli/src/dashboard.rs:929`, and updates are sent around lines 691 and 766.

## Plan of Work


### M1: the layout tree


Create `mj-tui/src/tile_layout.rs` and declare it in `mj-tui/src/lib.rs`. Port from `../herdr/src/layout.rs`: `PaneId(u32)` with `raw()` and `from_raw()`; `Node::{Pane(PaneId), Split { direction: ratatui::layout::Direction, ratio: f32, first: Box<Node>, second: Box<Node> }}`; `TileLayout` with `root`, `focus`, `prev_focus`, and a new `next_id: u32` field; `PaneInfo { id, rect, is_focused }` (herdr's `inner_rect`, `scrollbar_rect`, and `borders` fields are for its own chrome and are dropped); `SplitBorder`; `NavDirection`; the free functions `find_in_direction` and `split_rect`; and the tree helpers.

Two behavioural changes from herdr. First, `PaneId` allocation is per layout: `TileLayout::new()` starts `next_id` at 2 after handing out id 1, `split_pane` takes the next id and increments, and `from_saved(root, focus)` sets `next_id` to one more than the largest id in the tree. Second, minimum leaf size: `TileLayout::can_split(&self, target: PaneId, direction: Direction, area: Rect) -> bool` returns false when the target is missing or when either half of the target's current rect (computed with `split_rect` at ratio 0.5) would be under 40 columns or 8 rows. `split_pane(target, direction, ratio, area)` takes the area and returns `None` when `can_split` is false. Herdr's `insert_pane_near` and `swap_panes` are not ported (swapping is out of scope).

Keep the public surface: `new() -> (Self, PaneId)`, `focused()`, `pane_count()`, `panes(area) -> Vec<PaneInfo>`, `splits(area) -> Vec<SplitBorder>`, `split_pane`, `can_split`, `close_focused() -> bool`, `close_pane(id) -> bool`, `focus_pane(id)`, `set_ratio_at(path, ratio) -> bool`, `resize_focused(nav, delta, area)`, `resize_pane(id, nav, delta, area) -> bool`, `pane_ids()`, `root()`, `from_saved(root, focus)`.

Tests: port herdr's tests that apply (split ratio, resize behaviours, `find_in_direction` tie-breaks, close-focus history), and add `split_is_refused_when_a_leaf_would_fall_under_the_minimum` and `restored_layouts_allocate_ids_above_the_saved_ones`.

No behaviour change is visible in this milestone; the module is not yet used.

### M2: persisted per-workspace layout


In `mj-core/src/workspace.rs` add:

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SplitAxis { Horizontal, Vertical }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum LayoutNode {
        Pane { id: u32 },
        Split { axis: SplitAxis, ratio: f32, first: Box<LayoutNode>, second: Box<LayoutNode> },
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub struct ConversationLayout {
        pub root: LayoutNode,
        pub focus: u32,
        pub sessions: BTreeMap<u32, String>,
    }

`SplitAxis::Horizontal` means the children sit side by side (a vertical divider), matching ratatui's `Direction::Horizontal`. `ConversationLayout::default()` is one pane with id 1, focus 1, no sessions. `validate()` rejects duplicate pane ids, a focus not in the tree, a ratio outside `0.1..=0.9` or not finite, and a sessions key not in the tree. `ConversationLayout` is not `Eq` because of `f32`; the coordinator in `mj-cli` compares with `PartialEq`.

In `mj-controller/src/database/schema.rs` add migration 38, classified compatible: it adds table `workspace_layouts(workspace_id TEXT PRIMARY KEY REFERENCES workspaces(workspace_id) ON DELETE CASCADE, layout TEXT NOT NULL) STRICT`. Older readers never select from it; older writers never touch it; deleting a workspace cascades. The compatibility floor stays at 32. Bump `SCHEMA_VERSION` to 38 in `mj-controller/src/database.rs`. Follow the comment style of migration 37 and update any schema fixture or ledger test that pins the current version (search `database/tests.rs` for `37`).

In `mj-controller/src/database/workspaces.rs` add `load_workspace_layout(workspace_id)`, `load_workspace_layout_from(path, workspace_id)`, `save_workspace_layout(workspace_id, layout)`, and `save_workspace_layout_to(path, workspace_id, layout)` beside the pane-size functions, storing the layout as JSON text. A missing row returns `ConversationLayout::default()`; an unknown workspace is an error, as for pane sizes. Loading validates before returning. Tests in `mj-controller/src/database/tests.rs` mirror the `workspace_pane_sizes_*` tests: default for a fresh workspace, round trip, independence between workspaces, cascade on delete, unknown workspace rejected, invalid layout rejected on save.

In `mj-controller/src/daemon/actions.rs` add `DaemonAction::SaveWorkspaceLayout { workspace_id, layout }` handled beside `SaveWorkspacePaneSizes`, and in `mj-client/src/daemon.rs` add the matching enum variant and `save_workspace_layout` method. If the daemon protocol has a version number that changes when actions are added, bump it the way the pane-size change did (check `git log -S SaveWorkspacePaneSizes`).

In `mj-cli/src/dashboard/pane_sizes.rs` generalize `PaneSizePersistence` into `WorkspaceSettingPersistence<T: Clone + PartialEq + Default + Send + Sync + 'static>` with the same watch-channel, ordered-save, failure-notice, and bounded-flush behaviour, parameterized by the save closure and a short setting name used in notices ("workspace pane sizes", "workspace layout"). Keep the existing tests passing against the pane-size instantiation and add one test that the layout instantiation saves a changed layout. Instantiate it twice in `DashboardContext`: `pane_size_persistence` and `layout_persistence`. Load layouts at startup next to pane sizes (around `mj-cli/src/dashboard.rs:929`) into a `BTreeMap<String, ConversationLayout>` and hand it to `DashboardState` through a new `cache_workspace_layout(id, layout)`; send updates next to the pane-size updates (around lines 691 and 766) whenever `dashboard.workspace_layout_modified(id)` reports a change. In M2 `DashboardState` only stores and returns the layout per workspace (`conversation_layout()` and `workspace_layout_modified()`); M4 makes it drive rendering.

### M3: many warm chats in the controller


In `mj-cli/src/dashboard.rs` replace `active_chat: Option<ActiveChat>` with `chats: BTreeMap<String, ActiveChat>` keyed by session id. Add `focused_chat()` and `focused_chat_mut()` that look up the focused pane's session (from `DashboardState::current_session_id()`, which M4 redefines as the focused pane's session) in `chats`. Most of the roughly fifty `active_chat` call sites meant "the chat the user is typing into" and become the accessor. The per-session ones (`mark_active_chat_retiring`, `drop_warm_chat_for`, `apply_runtime_review_to_active_chat`, `refresh_chat_context`) index or iterate `chats` by session id.

Pump: replace the single `ActiveChat::pump` arm in the `select!` with one future built from `futures::future::select_all` over `chats.values_mut().map(|chat| Box::pin(ActiveChat::pump(Some(chat))))`, falling back to `std::future::pending()` when the map is empty. `pump` is cancel-safe, so dropping the other futures when one completes loses nothing. After the arm fires, call `acknowledge_visible_chats`.

Attach: `SessionAttachment` and `opening_chat_session` become per pane, `BTreeMap<PaneId, SessionAttachment>` and `BTreeMap<PaneId, String>`. `open_chat_session(session_id)` opens into the focused pane. An attach result installs the chat into `chats` and records which pane asked; if that pane's session changed while the attach was in flight, the late result is dropped, as today. Until M4 lands, there is one pane whose id comes from `DashboardState::conversation_layout().focus`.

Detach and drafts: `record_detach` in `mj-cli/src/dashboard/chat_tasks.rs` takes the session id explicitly. It runs when a pane's session is replaced, when a pane closes, and for every chat on quit.

Read receipts: `acknowledge_visible_chat` becomes `acknowledge_visible_chats`, over every pane that drew a chat this frame (M4 records which; in M3 that is the single focused pane).

Selection: only the focused pane registers `SurfaceId::Transcript`, `ElicitationMessage`, and `ReviewerTranscript`.

Tests in `mj-cli/src/dashboard/tests.rs`: two sessions opened in two panes both receive view updates; replacing a pane's session records the outgoing draft; closing a pane retires only its chat; quitting records every chat's detach. Because M3 lands before M4's rendering, these tests drive the layout through `DashboardState` methods that M3 adds for the purpose (`split_focused_pane_with_session`, `close_focused_pane`) and that M4 wires to commands.

### M4: rendering and pane commands


In `mj-tui/src/lib.rs`, `DashboardState` gains `conversation_layout: TileLayout` and `pane_sessions: BTreeMap<PaneId, String>` (an empty pane has no key), converted from and to `mj_core::workspace::ConversationLayout` in `cache_workspace_layout` and `conversation_layout()`. `current_session_id` becomes the focused pane's session; `set_current_session` updates the focused pane's entry.

In `render_combined` (`mj-tui/src/combined.rs`), keep the upper band's height allocation, using as `desired_prompt` the maximum over panes. Then `layout.panes(upper_area)` gives one rect per leaf. Each leaf splits into its own transcript and prompt using that chat's `desired_prompt_height(leaf.width)`, and draws through the same three-way choice the band uses today (launch standby, transition surface, chat, or the empty-pane advice) using that pane's session. The focused pane gets `footer`, `overlay: area`, and `prompt_focused`; other panes get `prompt_focused = false` and no footer. Record `conversation_pane_areas: Vec<(PaneId, Rect, Rect)>` in place of `chat_transcript_area` and `chat_prompt_area`; `chat_region_contains` returns the pane hit, and a click focuses that pane. Adjacent chat panels already draw their own borders, so no divider is drawn. `render_combined` receives the chats as `&mut BTreeMap<String, ActiveChat>` (or an equivalent lookup) instead of one `Option<&mut ActiveChat>`.

Session and pane linkage: focusing a pane selects its session row in Sessions. `OpenSession` (Enter) opens into the focused pane, replacing what was there; if the session is already open in another pane, focus moves there instead of duplicating.

New `CommandId`s in `mj-tui/src/actions.rs`, each with a `CommandSpec` so the F2 palette, F1 help, and footer pick them up:

- `OpenSessionSplitRight` and `OpenSessionSplitBelow` (`Scope::Session`, so they appear in the session row's `⋯` palette via `begin_session_palette`). They split the focused pane, put the selected session in the new leaf, and focus it. When `can_split` is false they set the notice "Not enough room to split" and change nothing.
- `ClosePane` (a new `Scope::Pane`): records detach, drops the chat, removes the leaf. Closing the last pane empties it rather than refusing.
- `FocusPaneLeft`, `FocusPaneDown`, `FocusPaneUp`, `FocusPaneRight` via `find_in_direction`; `ResizePaneLeft`, `ResizePaneDown`, `ResizePaneUp`, `ResizePaneRight` via `resize_focused` with delta 0.05.

No keys are bound in this milestone; the commands are reachable from the palettes. Every layout mutation marks the workspace layout modified so M2's coordinator saves it.

Tests in `mj-tui/src/render/tests.rs` and `mj-tui/src/tests.rs`: two panes draw two `Conversation` panels in the expected rects; the focused pane alone shows the focused border and footer; a split below the minimum is refused with the notice; Enter on a session already open elsewhere moves focus instead of duplicating; the layout round-trips through `ConversationLayout`.

### M5: prefix keybindings


Blocked on the `hel3` prefix-keybindings second milestone landing in this branch. Then add to the `key_actions!` table in `mj-core/src/config/keys.rs`, with herdr's letters:

    SplitVertical / split_vertical = "prefix+v"        (side by side)
    SplitHorizontal / split_horizontal = "prefix+minus" (stacked)
    ClosePane / close_pane = "prefix+x"
    FocusPaneLeft / focus_pane_left = "prefix+h"
    FocusPaneDown / focus_pane_down = "prefix+j"
    FocusPaneUp / focus_pane_up = "prefix+k"
    FocusPaneRight / focus_pane_right = "prefix+l"
    ResizePane{Left,Down,Up,Right} / resize_pane_* = unbound by default

`SplitVertical` and `SplitHorizontal` from the keyboard act on the Sessions selection: they split the focused pane and open the selected session in the new leaf. With nothing selected the new leaf is empty. Wire each `CommandSpec.action`, extend the test `every_key_action_maps_to_exactly_one_command`, and update `docs/src/content/docs/terminal-surface.mdx` and the config reference.

## Concrete Steps


Work in `/home/jonathan/Projects/hel4`. After each milestone's edits, run on the dev profile and outside the restricted sandbox:

    cargo fmt --all -- --check
    cargo clippy --all-targets -- -D warnings
    cargo test

Commit each milestone on the current branch with only the files it changed.

For M2, also run the migration and workspace-layout tests with an isolated store, which the tests in `mj-controller/src/database/tests.rs` do by setting `MJ_CONFIG_DIR` and `MJ_DATA_DIR` to temporary directories.

## Validation and Acceptance


After M1: `cargo test -p mj-tui tile_layout` passes, including `split_is_refused_when_a_leaf_would_fall_under_the_minimum`.

After M2: `cargo test -p mj-controller workspace_layout` passes; a fresh store reports schema 38 with floor 32; saving a two-pane layout for a workspace and loading it returns the same tree; deleting the workspace removes the row.

After M3: the `mj-cli` dashboard tests listed under M3 pass; the single-pane behaviour of `mj` is unchanged when run by hand.

After M4, end to end: run `mj` against the fake-harness lab (`tests/e2e/prepare-luna-lab.py`) with two sessions. From the Sessions row `⋯` menu choose "Open in split right"; both transcripts draw and both advance when their agents reply; F2 "Close pane" removes one; restart `mj` and the two-pane layout comes back with the same sessions.

After M5: `ctrl+b v`, `ctrl+b -`, `ctrl+b x`, and `ctrl+b h j k l` behave as in herdr, and the F1 help lists them.

## Idempotence and Recovery


Every milestone is additive until M3, which replaces the single-chat field; M3 and M4 are each one commit that can be reverted together. Migration 38 only creates a table, so an older build opened against the migrated store keeps working. Re-running the tests is safe at any point.

## Artifacts and Notes


To be added as milestones complete.

## Interfaces and Dependencies


In `mj-tui/src/tile_layout.rs` (M1):

    pub struct PaneId(u32);
    pub enum Node { Pane(PaneId), Split { direction: Direction, ratio: f32, first: Box<Node>, second: Box<Node> } }
    pub struct TileLayout { /* root, focus, prev_focus, next_id */ }
    impl TileLayout {
        pub fn new() -> (Self, PaneId);
        pub fn can_split(&self, target: PaneId, direction: Direction, area: Rect) -> bool;
        pub fn split_pane(&mut self, target: PaneId, direction: Direction, ratio: f32, area: Rect) -> Option<PaneId>;
        pub fn panes(&self, area: Rect) -> Vec<PaneInfo>;
        pub fn from_saved(root: Node, focus: PaneId) -> Self;
        // plus focused, pane_count, splits, close_focused, close_pane, focus_pane,
        // set_ratio_at, resize_focused, resize_pane, pane_ids, root
    }
    pub fn find_in_direction(focused: &PaneInfo, direction: NavDirection, panes: &[PaneInfo]) -> Option<PaneId>;

In `mj-core/src/workspace.rs` (M2): `SplitAxis`, `LayoutNode`, `ConversationLayout` with `Default` and `validate(&self) -> Result<()>`.

In `mj-controller/src/database/workspaces.rs` (M2): `load_workspace_layout`, `load_workspace_layout_from`, `save_workspace_layout`, `save_workspace_layout_to`.

In `mj-client/src/daemon.rs` (M2): `DaemonAction::SaveWorkspaceLayout` and `Daemon::save_workspace_layout(&self, workspace_id: String, layout: ConversationLayout) -> Result<()>`.

In `mj-cli/src/dashboard/pane_sizes.rs` (M2): `WorkspaceSettingPersistence<T>` with `start`, `remember`, `forget`, `update`, `is_running`, `wait`, `finish`.

In `mj-cli/src/dashboard.rs` (M3): `chats: BTreeMap<String, ActiveChat>`, `focused_chat()`, `focused_chat_mut()`, `acknowledge_visible_chats()`.

In `mj-tui/src/actions.rs` (M4): `CommandId::{OpenSessionSplitRight, OpenSessionSplitBelow, ClosePane, FocusPaneLeft, FocusPaneDown, FocusPaneUp, FocusPaneRight, ResizePaneLeft, ResizePaneDown, ResizePaneUp, ResizePaneRight}` and `Scope::Pane`.
