// Port of mj-client/src/transcript.rs.

// Transcript conversion and presentation rules shared by terminal and web
// clients.
//
// Rust's ChatEntry, ChatRole and the other mj_transcript::transcript items
// (re-exported from mj_core::transcript) are core's; the conversion helpers
// are package transcript's.

package client

import (
	"strings"
	"unicode/utf8"

	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// TranscriptRenderMode is whether a transcript renders decluttered or raw.
type TranscriptRenderMode int

const (
	TranscriptRenderModeRich TranscriptRenderMode = iota
	TranscriptRenderModeRaw
)

func (m TranscriptRenderMode) Toggled() TranscriptRenderMode {
	if m == TranscriptRenderModeRich {
		return TranscriptRenderModeRaw
	}
	return TranscriptRenderModeRich
}

// EntryCollapseKind is whether an entry renders on its own, renders nothing,
// or heads a collapsed streak of completed tools and thoughts.
type EntryCollapseKind int

const (
	EntryCollapseNone EntryCollapseKind = iota
	// EntryCollapseExpanded is a completed tool the user explicitly opened.
	// It is a streak boundary and renders its provider title plus all
	// available details in Rich mode.
	EntryCollapseExpanded
	// EntryCollapseHidden is a member of a collapsed run whose summary
	// renders on the run head.
	EntryCollapseHidden
	// EntryCollapseOmitted is detail the decluttered feed leaves out.
	EntryCollapseOmitted
	// EntryCollapseSummary is the head of a collapsed streak spanning
	// index..End (End exclusive).
	EntryCollapseSummary
)

// EntryCollapse is one entry's collapse state. One struct with a kind: End
// and Fingerprint belong to Summary. The zero value is None.
type EntryCollapse struct {
	Kind        EntryCollapseKind
	End         int
	Fingerprint uint64
}

const BrowserLineBytes = 4 * 1024

// MailboxEventLines interprets the durable mailbox list shared by historical
// and live clients. Event bullets have two spaces; report bodies have four,
// so report bullets remain part of their event instead of becoming new
// events.
func MailboxEventLines(role core.ChatRole, lines []string) [][]string {
	if role != core.ChatRoleSystem || len(lines) == 0 || lines[0] != "Mailbox events:" {
		return nil
	}
	var events [][]string
	for _, line := range lines[1:] {
		if header, ok := strings.CutPrefix(line, "  - "); ok {
			events = append(events, []string{header})
		} else if len(events) > 0 {
			last := len(events) - 1
			events[last] = append(events[last], strings.TrimPrefix(line, "    "))
		}
	}
	return events
}

// HistoryEntry is one row of the historical reader, which uses the canonical
// text interpretation on both surfaces, without the live feed's presentation
// collapse or tail clipping.
type HistoryEntry struct {
	StableID string `json:"stable_id"`
	// Position is the stored transcript position: the browser orders history
	// by it and matches it against live rows, whose ids are these positions.
	Position     uint64  `json:"position"`
	Role         string  `json:"role"`
	Text         string  `json:"text"`
	Label        *string `json:"label,omitempty"`
	RecordedAtMs *int64  `json:"recorded_at_ms,omitempty"`
}

func HistoryEntries(page *core.TranscriptHistoryPage) []HistoryEntry {
	panic("unported: mj-client/src/transcript.rs history_entries")
}

// BrowserTranscript is Rust's browser_transcript. It keeps the name by the
// package's collision rule; web.rs's BrowserTranscript type is
// WebBrowserTranscript.
func BrowserTranscript(sourceEntries []core.ChatEntry, latestSeq, lastCompactionSeq uint64, afterSeq *uint64) WebBrowserTranscript {
	panic("unported: mj-client/src/transcript.rs browser_transcript")
}

func browserTranscriptCollapsed(sourceEntries []core.ChatEntry, collapse []EntryCollapse, latestSeq, lastCompactionSeq uint64, afterSeq *uint64, restartCollapseWindowStart *uint64) WebBrowserTranscript {
	panic("unported: mj-client/src/transcript.rs browser_transcript_collapsed")
}

func MaterializedChatEntries(session *core.MaterializedSession) []core.ChatEntry {
	return MaterializedChatEntriesWithDiffstats(session, nil)
}

func MaterializedChatEntriesWithDiffstats(session *core.MaterializedSession, diffstats map[string][]string) []core.ChatEntry {
	panic("unported: mj-client/src/transcript.rs materialized_chat_entries_with_diffstats")
}

// TailSeedItems is how many transcript items a freshly opened chat converts
// on the caller's goroutine. Several screens of scrollback are ready in the
// first frame, and the rest of the history is converted off the event loop;
// a long session has thousands of items, and converting them all inline
// costs seconds.
const TailSeedItems = 256

// MaterializedPrefixEntries is the entries for the transcript items in
// items, which is a prefix of a session's transcript. Runs off the event
// loop, so it takes the items rather than the session. List-wide rules run
// after the caller splices these into the visible tail.
func MaterializedPrefixEntries(items []*core.TranscriptItem, frontier uint64) []core.ChatEntry {
	entries := make([]core.ChatEntry, 0, len(items))
	for _, item := range items {
		entries = append(entries, MaterializedChatEntryWithDiffstats(item, frontier, nil))
	}
	return entries
}

// MaterializedChatEntriesReusing rebuilds the entry list, keeping the
// entries whose transcript item did not change. An unchanged item is the
// same pointer as in the previous projection, so pointer identity settles
// reuse without reading a single field. The field comparison is the fallback
// for items that were rebuilt with equal content, which is what a restore
// from the canonical log produces.
//
// skip is the number of leading transcript items that are not converted
// yet, so previous lines up with session.Transcript[skip:]. It is zero for a
// complete projection.
func MaterializedChatEntriesReusing(session *core.MaterializedSession, skip int, previous []core.ChatEntry) []core.ChatEntry {
	panic("unported: mj-client/src/transcript.rs materialized_chat_entries_reusing")
}

// ApplyEntryListRules updates list-wide presentation after a build or
// splice, including reused rows. The session's pending requests own which
// permissions are open.
func ApplyEntryListRules(entries []core.ChatEntry, pending []core.ElicitationRequest) {
	panic("unported: mj-client/src/transcript.rs apply_entry_list_rules")
}

// MarkToolsEndedAfterInterrupt marks each completed tool call whose turn was
// interrupted while it still ran. Codex keeps a command going after an
// interrupt ends the turn and reports it completed when it ends on its own,
// which read as the interrupted turn's finished work (R10-1). A tool belongs
// to the turn that the next prompt ends; it ended after the interruption
// when its item last changed after that turn's "Interrupted" marker was
// recorded.
func MarkToolsEndedAfterInterrupt(entries []core.ChatEntry) {
	panic("unported: mj-client/src/transcript.rs mark_tools_ended_after_interrupt")
}

// SuppressDuplicateStandaloneTerminalOutput hides a legacy standalone result
// from Rich surfaces when a completed tool already carries the exact same
// raw result. New projections attach and remove this duplicate at the
// source; this pass keeps transcripts projected by an older Hel release
// equally quiet after an upgrade.
func SuppressDuplicateStandaloneTerminalOutput(entries []core.ChatEntry) {
	panic("unported: mj-client/src/transcript.rs suppress_duplicate_standalone_terminal_output")
}

func EntryMatchesTranscriptItem(entry *core.ChatEntry, item *core.TranscriptItem) bool {
	panic("unported: mj-client/src/transcript.rs entry_matches_transcript_item")
}

// EntryRole is the role one transcript item renders under.
//
// The matcher and the builder both read it, so an entry is reused only when
// it would be rebuilt the same way.
func EntryRole(item *core.TranscriptItem) core.ChatRole {
	panic("unported: mj-client/src/transcript.rs entry_role")
}

func MaterializedChatEntry(item *core.TranscriptItem, frontier uint64) core.ChatEntry {
	return MaterializedChatEntryWithDiffstats(item, frontier, nil)
}

// ItemUpdateOrdinal is the latest relay ordinal at which this item's
// rendered form can have changed. It is the entry's update cursor, so a
// remote viewer polling with `after_seq` receives an entry again only when
// the projection says the entry moved: stamping every entry with the
// frontier retransmits the whole window on every event.
//
// Overshooting is safe and undershooting is not, so an item whose changes
// the projection records no ordinal for keeps the frontier. Those are the
// bodies the projection edits in place — thoughts, tool calls, plans, and
// loose terminal output — and they stay conservative until the projection
// records a change ordinal for them the way it already does for agent
// messages.
func ItemUpdateOrdinal(item *core.TranscriptItem, frontier uint64) uint64 {
	panic("unported: mj-client/src/transcript.rs item_update_ordinal")
}

// MaterializedChatEntryWithDiffstats converts one item. exactDiffstats nil
// is Rust's None: the diffstats come from the call's own content.
func MaterializedChatEntryWithDiffstats(item *core.TranscriptItem, frontier uint64, exactDiffstats *[]string) core.ChatEntry {
	panic("unported: mj-client/src/transcript.rs materialized_chat_entry_with_diffstats")
}

// decodeToolCall is the stored call decoded, or nil, with a warning logged,
// when it does not decode.
func decodeToolCall(item *core.TranscriptItem, call *core.OpaqueToolCall) *acp.ToolCall {
	panic("unported: mj-client/src/transcript.rs decode_tool_call")
}

func toolRawOnly(call *acp.ToolCall, outputs []core.TerminalOutputRecord) bool {
	panic("unported: mj-client/src/transcript.rs tool_raw_only")
}

func MaterializedChatEntryWithOutputs(item *core.TranscriptItem, outputs *core.TerminalOutputs[core.TerminalOutputRecord], frontier uint64, exactDiffstats *[]string) core.ChatEntry {
	panic("unported: mj-client/src/transcript.rs materialized_chat_entry_with_outputs")
}

func UserLabel(entry *core.ChatEntry) string {
	panic("unported: mj-client/src/transcript.rs user_label")
}

func BrowserEntry(entry *core.ChatEntry) BrowserTranscriptEntry {
	panic("unported: mj-client/src/transcript.rs browser_entry")
}

// BrowserProjectionEntries builds the rows the browser receives from the
// same collapse decisions the Rich terminal renderer uses. A synthetic row
// is a normal ChatEntry so it can share labels, timestamps, tool state, and
// the update cursor with the rest of the projection.
func BrowserProjectionEntries(entries []core.ChatEntry, collapse []EntryCollapse, lastCompactionSeq uint64) []core.ChatEntry {
	panic("unported: mj-client/src/transcript.rs browser_projection_entries")
}

// RichPresentationKey is the stable topology signature for Rich's collapsed
// tool groups. Ordinary appends and content revisions deliberately do not
// participate: they can use the normal append/update cursor. A changed
// group membership produces a new key, which tells an append-only browser
// to rebuild its DOM.
func RichPresentationKey(entries []core.ChatEntry, collapse []EntryCollapse, lastCompactionSeq uint64) string {
	panic("unported: mj-client/src/transcript.rs rich_presentation_key")
}

func RoleTag(role core.ChatRole) []byte {
	panic("unported: mj-client/src/transcript.rs role_tag")
}

// EntryTone is the semantic colour name for one entry.
//
// A tool takes its tone from its state, because a failed tool call is the
// one thing in a transcript a person most needs to find.
func EntryTone(entry *core.ChatEntry) string {
	panic("unported: mj-client/src/transcript.rs entry_tone")
}

// ParseDiffstat reads back what `format_diffstat` wrote.
//
// It emits the path, two spaces, `+{insertions}`, a space, and
// `-{deletions}` using a Unicode MINUS SIGN. Parsing it here rather than in
// the browser keeps one definition of the format beside the code that
// produces it, and means a line that does not match is dropped rather than
// mangled.
func ParseDiffstat(line string) (BrowserDiffStat, bool) {
	panic("unported: mj-client/src/transcript.rs parse_diffstat")
}

func TruncateBrowserLine(line string) string {
	if len(line) <= BrowserLineBytes {
		return line
	}
	const suffix = "… [truncated]"
	end := BrowserLineBytes - len(suffix)
	for !utf8.RuneStart(line[end]) {
		end--
	}
	return line[:end] + suffix
}

const ToolAwaitingApproval = "waiting for approval"

func ToolStatusName(status core.ToolStatus) string {
	switch status {
	case core.ToolStatusCompleted:
		return "done"
	case core.ToolStatusFailed:
		return "failed"
	default:
		return "running"
	}
}

// ToolEndedAfterInterrupt is the state a tool row names for a call that
// ended after its turn was interrupted.
const ToolEndedAfterInterrupt = "ended after interrupt"

// ToolStateName is the state a tool row names: its call's status, except
// for a call that ended after its turn was interrupted.
func ToolStateName(entry *core.ChatEntry) string {
	switch {
	case entry.EndedAfterInterrupt:
		return ToolEndedAfterInterrupt
	case entry.AwaitingApproval:
		return ToolAwaitingApproval
	case entry.ToolStatus == nil:
		return ToolStatusName(core.ToolStatusPending)
	default:
		return ToolStatusName(*entry.ToolStatus)
	}
}

func IsCompletedTool(entry *core.ChatEntry) bool {
	return entry.Role == core.ChatRoleTool && entry.ToolStatus != nil &&
		*entry.ToolStatus == core.ToolStatusCompleted
}

// EntryRenderFingerprint is the cache identity of the entry and its
// list-derived presentation. It never leaves the process.
func EntryRenderFingerprint(entry *core.ChatEntry) uint64 {
	panic("unported: mj-client/src/transcript.rs entry_render_fingerprint")
}

func CollapseRevisionFingerprint(entries []core.ChatEntry) uint64 {
	var accumulated uint64
	for i := range entries {
		accumulated = accumulated*31 + EntryRenderFingerprint(&entries[i])
	}
	return accumulated
}

// CollapsedSessionRestartStates marks all but the newest restart in each run
// of restart markers. Entries omitted from the selected feed do not
// interrupt a run, because they cannot be visible between the markers. Raw
// mode has no omitted entries, while Rich omits clean terminal output. The
// returned positions let browser and TUI presentation use the same
// adjacency rule without mutating the durable transcript or its cursors.
func CollapsedSessionRestartStates(entries []core.ChatEntry, mode TranscriptRenderMode) []bool {
	panic("unported: mj-client/src/transcript.rs collapsed_session_restart_states")
}

// RestartCollapseWindowStartSeq is the conservative reset boundary for a
// browser feed whose previous restart marker may have been removed by a
// later projection. The wire shape only carries one `window_start_seq`, so
// use the newest replacement marker across all collapsed runs. This can
// resend a little history, but it guarantees a client never retains a
// hidden entry in its append-only DOM.
func RestartCollapseWindowStartSeq(entries []core.ChatEntry, collapsedRestarts []bool, mode TranscriptRenderMode) *uint64 {
	panic("unported: mj-client/src/transcript.rs restart_collapse_window_start_seq")
}

// EntryCollapseStates is what every entry renders as, which is the one place
// the decluttered feed is decided. In rich mode a maximal streak of
// completed tools and thoughts renders its newest thought followed by one
// synthetic tool summary. Earlier thoughts are hidden. Multiple thoughts,
// multiple completed tools, or a thought following a completed tool use the
// summary layout. A singleton tool uses its parser-derived summary in place,
// including while it is pending, running, or failed. Every other entry,
// including an active or failed tool, breaks the streak. A tool the reader
// opened renders expanded whatever its status, so a running call's full
// invocation can be read before it ends. A raw-only entry renders nothing at
// all and is transparent to a streak rather than breaking it, since nothing
// of it is on screen to separate the surrounding entries. Raw mode does not
// tool-collapse or omit entries, but it still coalesces adjacent restart
// markers as a presentation rule. core.ToolOutputInline keeps the Rich
// omissions but forms no streaks: every tool and thought renders on its own.
func EntryCollapseStates(entries []core.ChatEntry, mode TranscriptRenderMode, tools core.ToolOutput, expandedToolCalls map[uint64]struct{}) []EntryCollapse {
	panic("unported: mj-client/src/transcript.rs entry_collapse_states")
}

// CollapsedToolEntry is the single cell that stands in for a streak of
// completed tools: each member's compact summary in order. Non-tool entries
// contribute none.
func CollapsedToolEntry(members []core.ChatEntry) core.ChatEntry {
	refs := make([]*core.ChatEntry, len(members))
	for i := range members {
		refs[i] = &members[i]
	}
	return collapsedToolEntryRefs(refs)
}

func collapsedToolEntryRefs(members []*core.ChatEntry) core.ChatEntry {
	panic("unported: mj-client/src/transcript.rs collapsed_tool_entry_refs")
}

// CollapsedStreakEntries materializes the Rich order for a collapsed streak:
// newest thought first, followed by the synthetic tool row. This is shared
// by the terminal and browser projections so an interleaved thought cannot
// disappear on one surface or move behind the tool summary on the other.
func CollapsedStreakEntries(members []core.ChatEntry) []core.ChatEntry {
	refs := make([]*core.ChatEntry, len(members))
	for i := range members {
		refs[i] = &members[i]
	}
	return collapsedStreakEntryRefs(refs)
}

func collapsedStreakEntryRefs(members []*core.ChatEntry) []core.ChatEntry {
	panic("unported: mj-client/src/transcript.rs collapsed_streak_entry_refs")
}

// EntryGlyph is the mark the browser projection sends for one entry. The
// browser always has UTF-8, so these stay Unicode; a terminal reads its
// marks from the symbol set in force instead (mj-chat's `role_glyph`),
// because a Linux console or a locale without UTF-8 draws mojibake for them.
func EntryGlyph(entry *core.ChatEntry) string {
	panic("unported: mj-client/src/transcript.rs entry_glyph")
}

func MaterializedBrowserTranscript(session *core.MaterializedSession) WebBrowserTranscript {
	return BrowserTranscript(MaterializedChatEntries(session), session.AppliedEventOrdinal, 0, nil)
}

// BrowserTranscriptProjector reuses unchanged typed entries while
// recomputing collapse topology and cursors. Owned by one session's
// supervised background projection task.
type BrowserTranscriptProjector struct {
	entries []core.ChatEntry
	// published is the entries this projector last published, by id.
	published map[string]*BrowserTranscriptEntry
}

// releaseRawViewDetail: a browser tool row renders its summary and
// diffstats only. The Raw-view detail below is read by the terminal
// renderer, which converts its own entries, so the browser projector does
// not keep it.
func releaseRawViewDetail(entries []core.ChatEntry) {
	for i := range entries {
		if entries[i].Role != core.ChatRoleTool {
			continue
		}
		entries[i].ToolContent = nil
		entries[i].ToolInput = nil
		entries[i].ToolError = nil
		entries[i].ToolLocations = nil
	}
}

// Project projects session for the browser, with an exact update cursor on
// every entry.
//
// An entry that looks exactly as it did in the previous projection keeps
// the cursor it was published with, so a browser polling with `after_seq` is
// not sent it again. A changed entry gets the new frontier. This holds for
// every item kind and for changes the item itself does not record: a tool
// call marked ended by a later interrupt, or changes to collapse groups as
// the window slides.
func (p *BrowserTranscriptProjector) Project(session *core.MaterializedSession) WebBrowserTranscript {
	panic("unported: mj-client/src/transcript.rs BrowserTranscriptProjector::project")
}

// sameRendering is whether two published entries render the same, whatever
// cursor they carry.
func sameRendering(previous, next *BrowserTranscriptEntry) bool {
	panic("unported: mj-client/src/transcript.rs same_rendering")
}
