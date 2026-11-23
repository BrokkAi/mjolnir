// Port of mj-client/src/transcript.rs.

// Transcript conversion and presentation rules shared by terminal and web
// clients.
//
// Rust's ChatEntry, ChatRole and the other mj_transcript::transcript items
// (re-exported from mj_core::transcript) are core's; the conversion helpers
// are package transcript's.

package client

import (
	"bytes"
	"crypto/sha256"
	"encoding/binary"
	"encoding/json"
	"hash/fnv"
	"log/slog"
	"math"
	"slices"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/schema"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
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
	Position          uint64  `json:"position"`
	Role              string  `json:"role"`
	ToolDetailTrimmed bool    `json:"tool_detail_trimmed,omitempty"`
	Text              string  `json:"text"`
	Label             *string `json:"label,omitempty"`
	RecordedAtMs      *int64  `json:"recorded_at_ms,omitempty"`
}

func HistoryEntries(page *core.TranscriptHistoryPage) []HistoryEntry {
	entries := make([]HistoryEntry, 0, len(page.Items))
	for _, item := range page.Items {
		entry := HistoryEntry{
			StableID:          item.StableID,
			Position:          item.Position,
			Role:              core.TranscriptItemRole(item.Body),
			ToolDetailTrimmed: core.TranscriptBodyHasTrimmedDetail(item.Body),
		}
		if message, ok := item.Body.(core.TranscriptBodyMessage); ok {
			text, found := message.Event.MessageText()
			if !found {
				panic("message entry metadata")
			}
			entry.Text = core.SanitizeTerminalText(text)
			label := message.Event.MessageLabel()
			entry.Label = &label
			recordedAt := item.CreatedAtMs
			entry.RecordedAtMs = &recordedAt
		} else {
			entry.Text = transcript.TranscriptItemText(item)
		}
		entries = append(entries, entry)
	}
	return entries
}

// BrowserTranscript is Rust's browser_transcript. It keeps the name by the
// package's collision rule; web.rs's BrowserTranscript type is
// WebBrowserTranscript.
func BrowserTranscript(sourceEntries []core.ChatEntry, latestSeq, lastCompactionSeq uint64, afterSeq *uint64) WebBrowserTranscript {
	collapsedRestarts := CollapsedSessionRestartStates(sourceEntries, TranscriptRenderModeRich)
	// A disclosure needs the identity of a single stored tool. Keep these
	// rows outside the browser's synthetic groups.
	detailRows := map[uint64]struct{}{}
	for i := range sourceEntries {
		if sourceEntries[i].ToolDetailTrimmed {
			detailRows[sourceEntries[i].StartSeq] = struct{}{}
		}
	}
	collapse := EntryCollapseStates(sourceEntries, TranscriptRenderModeRich, core.ToolOutputGrouped, detailRows)
	restartCollapseWindowStart := RestartCollapseWindowStartSeq(sourceEntries, collapsedRestarts, TranscriptRenderModeRich)
	return browserTranscriptCollapsed(sourceEntries, collapse, latestSeq, lastCompactionSeq, afterSeq, restartCollapseWindowStart)
}

func browserTranscriptCollapsed(sourceEntries []core.ChatEntry, collapse []EntryCollapse, latestSeq, lastCompactionSeq uint64, afterSeq *uint64, restartCollapseWindowStart *uint64) WebBrowserTranscript {
	projected := BrowserProjectionEntries(sourceEntries, collapse, lastCompactionSeq)
	entries := make([]BrowserTranscriptEntry, 0, len(projected))
	for i := range projected {
		entries = append(entries, BrowserEntry(&projected[i]))
	}
	for i := range entries {
		role := core.ChatRoleAgent
		if entries[i].Role == "system" {
			role = core.ChatRoleSystem
		}
		entries[i].MailboxEvents = MailboxEventLines(role, entries[i].Lines)
	}
	entries = slices.DeleteFunc(entries, func(entry BrowserTranscriptEntry) bool { return len(entry.Lines) == 0 })
	// A collapse can remove an entry a viewer already received even when
	// older, unrelated entries remain in the window. Move the reset boundary
	// to the newest replacement marker so the append-only web feed is
	// rebuilt in that case; the server applies this same boundary to cached
	// projections when serving `after_seq` deltas.
	windowStartSeq := latestSeq
	for i, entry := range entries {
		if i == 0 || entry.ID < windowStartSeq {
			windowStartSeq = entry.ID
		}
	}
	if restartCollapseWindowStart != nil {
		windowStartSeq = max(windowStartSeq, *restartCollapseWindowStart)
	}
	reset := afterSeq != nil && *afterSeq < windowStartSeq
	if afterSeq != nil && !reset {
		entries = slices.DeleteFunc(entries, func(entry BrowserTranscriptEntry) bool { return entry.UpdatedSeq <= *afterSeq })
	}
	published := make([]*BrowserTranscriptEntry, len(entries))
	for i := range entries {
		published[i] = &entries[i]
	}
	return WebBrowserTranscript{
		LatestSeq:       latestSeq,
		PresentationKey: RichPresentationKey(sourceEntries, collapse, lastCompactionSeq),
		WindowStartSeq:  windowStartSeq,
		Reset:           reset,
		Entries:         published,
	}
}

func MaterializedChatEntries(session *core.MaterializedSession) []core.ChatEntry {
	return MaterializedChatEntriesWithDiffstats(session, nil)
}

func MaterializedChatEntriesWithDiffstats(session *core.MaterializedSession, diffstats map[string][]string) []core.ChatEntry {
	entries := make([]core.ChatEntry, 0, len(session.Transcript))
	for _, item := range session.Transcript {
		var exact *[]string
		if stats, ok := diffstats[item.StableID]; ok {
			exact = &stats
		}
		entries = append(entries, MaterializedChatEntryWithDiffstats(item, session.AppliedEventOrdinal, exact))
	}
	ApplyEntryListRules(entries, session.PendingElicitations)
	return entries
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
	next := 0
	items := session.Transcript[min(skip, len(session.Transcript)):]
	entries := make([]core.ChatEntry, 0, len(items))
	for _, item := range items {
		// A bounded canonical tail slides forward as messages arrive. Align
		// by durable position so appending one message does not reparse
		// every unchanged tool body in the window.
		for next < len(previous) && previous[next].StartSeq < item.Position {
			next++
		}
		if next >= len(previous) || previous[next].StartSeq != item.Position {
			entries = append(entries, MaterializedChatEntryWithDiffstats(item, session.AppliedEventOrdinal, nil))
			continue
		}
		entry := previous[next]
		next++
		if entry.Source.Is(item) {
			entry.Seq = ItemUpdateOrdinal(item, session.AppliedEventOrdinal)
			entries = append(entries, entry)
			continue
		}
		if EntryMatchesTranscriptItem(&entry, item) {
			entry.Seq = ItemUpdateOrdinal(item, session.AppliedEventOrdinal)
			stableID := item.StableID
			entry.StableID = &stableID
			entry.ToolDetailTrimmed = core.TranscriptBodyHasTrimmedDetail(item.Body)
			entry.Source = core.TranscriptSource{Item: item}
			entries = append(entries, entry)
			continue
		}
		entries = append(entries, MaterializedChatEntryWithDiffstats(item, session.AppliedEventOrdinal, nil))
	}
	ApplyEntryListRules(entries, session.PendingElicitations)
	return entries
}

// ApplyEntryListRules updates list-wide presentation after a build or
// splice, including reused rows. The session's pending requests own which
// permissions are open.
func ApplyEntryListRules(entries []core.ChatEntry, pending []core.ElicitationRequest) {
	open := map[string]struct{}{}
	for i := range pending {
		if permission := pending[i].ToolPermission; permission != nil && permission.ToolCallID != nil {
			open[transcript.ToolItemID(*permission.ToolCallID)] = struct{}{}
		}
	}
	for i := range entries {
		entry := &entries[i]
		awaiting := false
		if len(open) > 0 && entry.Role == core.ChatRoleTool {
			if item := entry.Source.Item; item != nil {
				_, awaiting = open[item.StableID]
			} else if entry.ToolCallID != nil {
				// Direct ACP replay keeps the raw call ID for tool updates.
				_, awaiting = open[transcript.ToolItemID(*entry.ToolCallID)]
			}
		}
		entry.AwaitingApproval = awaiting
	}
	SuppressDuplicateStandaloneTerminalOutput(entries)
	MarkToolsEndedAfterInterrupt(entries)
}

// MarkToolsEndedAfterInterrupt marks each completed tool call whose turn was
// interrupted while it still ran. Codex keeps a command going after an
// interrupt ends the turn and reports it completed when it ends on its own,
// which read as the interrupted turn's finished work (R10-1). A tool belongs
// to the turn that the next prompt ends; it ended after the interruption
// when its item last changed after that turn's "Interrupted" marker was
// recorded.
func MarkToolsEndedAfterInterrupt(entries []core.ChatEntry) {
	var interruptedAt *int64
	for i := len(entries) - 1; i >= 0; i-- {
		entry := &entries[i]
		item := entry.Source.Item
		if item == nil {
			entry.EndedAfterInterrupt = false
			continue
		}
		switch item.Body.(type) {
		case core.TranscriptBodyUser:
			interruptedAt = nil
		case core.TranscriptBodySystem:
			if strings.HasPrefix(item.StableID, core.HarnessTurnItemPrefix) {
				interruptedAt = nil
			} else if strings.HasPrefix(item.StableID, core.TurnInterruptedItemPrefix) {
				at := item.CreatedAtMs
				interruptedAt = &at
			}
		}
		entry.EndedAfterInterrupt = entry.Role == core.ChatRoleTool && entry.ToolStatus != nil &&
			*entry.ToolStatus == core.ToolStatusCompleted && interruptedAt != nil && item.LastChangedAtMs > *interruptedAt
	}
}

// SuppressDuplicateStandaloneTerminalOutput hides a legacy standalone result
// from Rich surfaces when a completed tool already carries the exact same
// raw result. New projections attach and remove this duplicate at the
// source; this pass keeps transcripts projected by an older Hel release
// equally quiet after an upgrade.
func SuppressDuplicateStandaloneTerminalOutput(entries []core.ChatEntry) {
	hasStandalone := slices.ContainsFunc(entries, func(entry core.ChatEntry) bool {
		if entry.Source.Item == nil {
			return false
		}
		_, ok := entry.Source.Item.Body.(core.TranscriptBodyTerminalOutput)
		return ok
	})
	if !hasStandalone {
		return
	}
	var tools []core.TranscriptBodyTool
	for i := range entries {
		if item := entries[i].Source.Item; item != nil {
			if tool, ok := item.Body.(core.TranscriptBodyTool); ok {
				tools = append(tools, tool)
			}
		}
	}
	for i := range entries {
		item := entries[i].Source.Item
		if item == nil {
			continue
		}
		output, ok := item.Body.(core.TranscriptBodyTerminalOutput)
		if !ok {
			continue
		}
		record := output.Record
		digest := record.RawResultDigest()
		entries[i].RawOnly = record.ExitedCleanly() || slices.ContainsFunc(tools, func(tool core.TranscriptBodyTool) bool {
			return tool.Call.MatchesRawResult(&record, &digest)
		})
	}
}

func EntryMatchesTranscriptItem(entry *core.ChatEntry, item *core.TranscriptItem) bool {
	if source := entry.Source.Item; source != nil && !transcriptItemEqual(source, item) {
		return false
	}
	revision := uint64(0)
	if item.LastChangedAtMs > 0 {
		revision = uint64(item.LastChangedAtMs)
	}
	if entry.StartSeq != item.Position || entry.RecordedAtMs == nil || *entry.RecordedAtMs != item.CreatedAtMs ||
		entry.Revision != revision || entry.Role != EntryRole(item) {
		return false
	}
	switch item.Body.(type) {
	case core.TranscriptBodyAgent, core.TranscriptBodyThought:
		return entry.MessageID != nil && *entry.MessageID == item.StableID
	case core.TranscriptBodyTool:
		return entry.ToolCallID != nil && *entry.ToolCallID == item.StableID
	}
	return true
}

// transcriptItemEqual is TranscriptItem's PartialEq: the same identity,
// clocks and body. Bodies compare as they serialize.
func transcriptItemEqual(a, b *core.TranscriptItem) bool {
	if a == b {
		return true
	}
	if a.StableID != b.StableID || a.Position != b.Position || a.CreatedAtMs != b.CreatedAtMs ||
		a.LastChangedAtMs != b.LastChangedAtMs ||
		!quotaOptionalEqual(a.LatestContentEventOrdinal, b.LatestContentEventOrdinal) {
		return false
	}
	left, leftErr := json.Marshal(a.Body)
	right, rightErr := json.Marshal(b.Body)
	return leftErr == nil && rightErr == nil && bytes.Equal(left, right)
}

// TranscriptItemEqual is TranscriptItem's PartialEq, for callers outside
// this package that compare items by value (mj-chat's background
// conversion compares `Some(item) == source`).
func TranscriptItemEqual(a, b *core.TranscriptItem) bool {
	return transcriptItemEqual(a, b)
}

// EntryRole is the role one transcript item renders under.
//
// The matcher and the builder both read it, so an entry is reused only when
// it would be rebuilt the same way.
func EntryRole(item *core.TranscriptItem) core.ChatRole {
	switch body := item.Body.(type) {
	case core.TranscriptBodyUser:
		// A prompt Hel generated for a review is a control-origin record.
		// Rendering it as the user's would put words in their mouth, so it
		// reads as Hel's own line instead.
		if core.IsControlOriginPrompt(core.MaterializedContentText(body.Content)) {
			return core.ChatRoleSystem
		}
		return core.ChatRoleUser
	case core.TranscriptBodyMessage:
		return core.ChatRoleMessage
	case core.TranscriptBodyAgent:
		return core.ChatRoleAgent
	case core.TranscriptBodyThought:
		return core.ChatRoleThought
	case core.TranscriptBodyTool:
		return core.ChatRoleTool
	case core.TranscriptBodyPlan:
		return core.ChatRolePlan
	case core.TranscriptBodyPlanProposal:
		return core.ChatRolePlanProposal
	}
	return core.ChatRoleSystem
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
	var latest uint64
	switch item.Body.(type) {
	// Created once and never revisited, so the creating event is exact.
	case core.TranscriptBodyMessage, core.TranscriptBodyUser, core.TranscriptBodySystem, core.TranscriptBodyPlanProposal:
		latest = item.Position
	// Every appended chunk records the ordinal that appended it. Closing the
	// stream is the only other edit and nothing rendered reads it.
	case core.TranscriptBodyAgent:
		latest = frontier
		if item.LatestContentEventOrdinal != nil {
			latest = *item.LatestContentEventOrdinal
		}
	default:
		latest = frontier
	}
	return max(latest, item.Position)
}

// MaterializedChatEntryWithDiffstats converts one item. exactDiffstats nil
// is Rust's None: the diffstats come from the call's own content.
func MaterializedChatEntryWithDiffstats(item *core.TranscriptItem, frontier uint64, exactDiffstats *[]string) core.ChatEntry {
	return MaterializedChatEntryWithOutputs(item, core.TerminalOutputsOf([]core.TranscriptItem{*item}), frontier, exactDiffstats)
}

// decodeToolCall is the stored call decoded, or nil, with a warning logged,
// when it does not decode.
func decodeToolCall(item *core.TranscriptItem, call *core.OpaqueToolCall) *acp.ToolCall {
	decoded, err := call.Decode()
	if err != nil {
		slog.Warn("could not decode a stored tool call; rendering it as invalid",
			"stable_id", item.StableID, "error", err)
		return nil
	}
	return &decoded
}

// stableToolCall is call in the stable schema's types, which the core's
// terminal fallback rule reads.
func stableToolCall(call *acp.ToolCall) (schema.ToolCall, bool) {
	var stable schema.ToolCall
	encoded, err := json.Marshal(call)
	if err != nil {
		return stable, false
	}
	return stable, json.Unmarshal(encoded, &stable) == nil
}

func isFallbackTerminalToolCall(call *acp.ToolCall) bool {
	stable, ok := stableToolCall(call)
	return ok && core.IsFallbackTerminalToolCall(&stable)
}

func toolRawOnly(call *acp.ToolCall, outputs []core.TerminalOutputRecord) bool {
	if !isFallbackTerminalToolCall(call) || len(outputs) == 0 {
		return false
	}
	for i := range outputs {
		if !outputs[i].ExitedCleanly() {
			return false
		}
	}
	return true
}

func MaterializedChatEntryWithOutputs(item *core.TranscriptItem, outputs *core.TerminalOutputs[core.TerminalOutputRecord], frontier uint64, exactDiffstats *[]string) core.ChatEntry {
	var entry core.ChatEntry
	switch body := item.Body.(type) {
	case core.TranscriptBodyUser:
		entry = core.ChatEntryPlain(item.Position, EntryRole(item), core.MaterializedContentText(body.Content))
	case core.TranscriptBodyMessage:
		entry = core.ChatEntryReceivedMessage(item.Position, body.Event)
	case core.TranscriptBodyAgent:
		entry = core.ChatEntryPlain(item.Position, core.ChatRoleAgent, core.MaterializedChunksText(body.Chunks))
	case core.TranscriptBodyThought:
		entry = core.ChatEntryPlain(item.Position, core.ChatRoleThought, core.MaterializedChunksText(body.Chunks))
	case core.TranscriptBodyTool:
		terminalOutputs := outputs.ForTool(item.StableID)
		call := decodeToolCall(item, &body.Call)
		title := "[invalid tool call]"
		status := core.ToolStatusPending
		if call != nil {
			title = call.Title
			callStatus := acp.ToolCallStatusPending
			if call.Status != nil {
				callStatus = *call.Status
			}
			status = core.ToolStatusOf(callStatus)
		}
		stableID := item.StableID
		entry = core.ChatEntryTool(item.Position, title, &stableID, status)
		if call != nil {
			presentation := transcript.MaterializedToolCallPresentation(body.Presentation, call)
			// Inline chat still shows the full command while the raw call is
			// available. The materialized row stores only its label.
			if presentation.Source.IsEmpty() {
				presentation.Source = transcript.ToolCallPresentation(call).Source
			}
			summary := presentation.Summary
			entry.ToolSummary = &summary
			entry.ToolPresentation = &presentation
			entry.ToolInput = call.RawInput
			entry.ToolError = transcript.ToolErrorDetail(call.RawOutput)
			fallbackTerminal := isFallbackTerminalToolCall(call)
			callStatus := acp.ToolCallStatusPending
			if call.Status != nil {
				callStatus = *call.Status
			}
			entry.ToolContent = transcript.ToolContentDetails(call.Content, terminalOutputs, call.RawOutput, core.ToolStatusOf(callStatus))
			if exactDiffstats != nil {
				entry.ToolDiffstats = slices.Clone(*exactDiffstats)
			} else {
				entry.ToolDiffstats = transcript.ToolDiffPaths(call.Content)
			}
			entry.ToolLocations = transcript.ToolLocationDetails(call.Locations)
			// Successful fallback calls replace the successful standalone
			// terminal blocks Rich already omitted. Raw mode still keeps
			// every command, while failures remain visible everywhere.
			entry.RawOnly = toolRawOnly(call, terminalOutputs)
			if fallbackTerminal && callStatus == acp.ToolCallStatusFailed {
				details := entry.ToolContent
				entry.ToolContent = nil
				if len(details) > 0 {
					joined := strings.Join(details, "\n")
					entry.Text += "\n" + joined
					// Raw keeps the provider title plus its captured output
					// in Text. Rich/browser need the same failed-call detail
					// while reading the compact presentation field, so append
					// it only to this display value; the cached parser
					// metadata remains the clean command summary above.
					if entry.ToolSummary != nil {
						extended := *entry.ToolSummary + "\n" + joined
						entry.ToolSummary = &extended
					}
				}
			}
		}
	case core.TranscriptBodyTerminalOutput:
		detail := transcript.TerminalOutputDetail(&body.Record)
		text := detail
		if body.FirstErrorLine != nil {
			text = *body.FirstErrorLine + "\n" + detail
		}
		entry = core.ChatEntryPlain(item.Position, core.ChatRoleSystem, core.SanitizeTerminalText(text))
		// Output no tool call refers to is a whole block per command. A
		// command that ended cleanly says nothing the decluttered feed
		// needs, so only the raw transcript carries it; anything abnormal
		// stays visible everywhere.
		entry.RawOnly = body.Record.ExitedCleanly()
	case core.TranscriptBodyPlan:
		var plan acp.Plan
		var lines []core.PlanLine
		if err := json.Unmarshal(body.Plan, &plan); err != nil {
			slog.Warn("could not decode a stored plan; rendering it empty", "stable_id", item.StableID, "error", err)
		} else {
			for _, line := range plan.Entries {
				lines = append(lines, core.PlanLine{
					Text:   core.SanitizeTerminalText(line.Content),
					Status: core.PlanStatusOf(line.Status),
				})
			}
		}
		entry = core.ChatEntryPlan(item.Position, lines)
	case core.TranscriptBodyPlanProposal:
		entry = core.ChatEntryPlain(item.Position, core.ChatRolePlanProposal, body.Plan)
	case core.TranscriptBodySystem:
		entry = core.ChatEntryPlain(item.Position, core.ChatRoleSystem, body.Text)
	}
	entry.Seq = ItemUpdateOrdinal(item, frontier)
	recordedAt := item.CreatedAtMs
	entry.RecordedAtMs = &recordedAt
	entry.Revision = 0
	if item.LastChangedAtMs > 0 {
		entry.Revision = uint64(item.LastChangedAtMs)
	}
	// A finished call is not updated again, so its last change is its end.
	if entry.Role == core.ChatRoleTool && entry.ToolStatus != nil &&
		(*entry.ToolStatus == core.ToolStatusCompleted || *entry.ToolStatus == core.ToolStatusFailed) {
		ended := item.LastChangedAtMs
		entry.ToolEndedAtMs = &ended
	}
	switch item.Body.(type) {
	case core.TranscriptBodyAgent, core.TranscriptBodyThought:
		messageID := item.StableID
		entry.MessageID = &messageID
	}
	stableID := item.StableID
	entry.StableID = &stableID
	entry.ToolDetailTrimmed = core.TranscriptBodyHasTrimmedDetail(item.Body)
	entry.Source = core.TranscriptSource{Item: item}
	return entry
}

// ToolDetailChatEntry renders a reloaded body with the same identity and
// timestamps as its row.
func ToolDetailChatEntry(original *core.TranscriptItem, frontier uint64, body core.TranscriptBody) core.ChatEntry {
	item := *original
	item.Body = body
	var exact *[]string
	if diffstats, ok := transcript.MaterializedToolDiffstats(&item); ok {
		exact = &diffstats
	}
	return MaterializedChatEntryWithDiffstats(&item, frontier, exact)
}

// ToolDetailPatches is the stored patches of a call's diff content.
func ToolDetailPatches(call *core.OpaqueToolCall) []core.DiffPatch {
	var decoded schema.ToolCall
	if err := json.Unmarshal(call.Value(), &decoded); err != nil {
		return []core.DiffPatch{}
	}
	patches := []core.DiffPatch{}
	for i := range decoded.Content {
		content := &decoded.Content[i]
		if content.Kind == schema.ToolCallContentKindDiff && content.Diff != nil {
			patches = append(patches, core.PatchOf(content.Diff))
		}
	}
	return patches
}

func UserLabel(entry *core.ChatEntry) string {
	item := entry.Source.Item
	if item != nil && strings.HasPrefix(item.StableID, "user:quota-retry-") {
		return "Automatic · quota recovery"
	}
	if item != nil {
		if commandID, ok := strings.CutPrefix(item.StableID, "user:"); ok && core.IsCapacityRetryCommand(commandID) {
			return "Automatic · server retry"
		}
	}
	return "You"
}

func BrowserEntry(entry *core.ChatEntry) BrowserTranscriptEntry {
	var role, label string
	switch entry.Role {
	case core.ChatRoleUser:
		role, label = "user", UserLabel(entry)
	case core.ChatRoleMessage:
		if entry.IncomingMessage == nil {
			panic("message entry metadata")
		}
		role, label = "message", entry.IncomingMessage.MessageLabel()
	case core.ChatRoleAgent:
		role, label = "agent", "Agent"
	case core.ChatRoleThought:
		role, label = "thought", "Thinking"
	case core.ChatRoleTool:
		role, label = "tool", "Tool · "+ToolStateName(entry)
	case core.ChatRolePlan:
		role, label = "plan", "Plan"
	case core.ChatRolePlanProposal:
		role, label = "plan-proposal", "Proposed plan"
	default:
		role, label = "system", "Mjolnir"
	}
	var source []string
	switch entry.Role {
	case core.ChatRolePlan:
		for _, line := range entry.Plan {
			marker := "○"
			switch line.Status {
			case core.PlanStatusRunning:
				marker = "●"
			case core.PlanStatusCompleted:
				marker = "✓"
			}
			source = append(source, marker+" "+line.Text)
		}
	case core.ChatRoleTool:
		// The remote viewer mirrors the TUI's Rich feed: the parser-derived
		// summary plus any diffstat, not the full Raw detail. Summaries are
		// present while a call is pending too, so the title never changes
		// shape merely because the call completed.
		summary := entry.Text
		if entry.ToolSummary != nil {
			summary = *entry.ToolSummary
		}
		source = append([]string{summary}, entry.ToolDiffstats...)
	default:
		source = rustLines(entry.Text)
	}
	var commandID *string
	if item := entry.Source.Item; item != nil {
		if id, ok := strings.CutPrefix(item.StableID, "user:"); ok {
			commandID = &id
		} else if id, ok := strings.CutPrefix(item.StableID, "shell:"); ok {
			commandID = &id
		}
	}
	var stableID *string
	switch {
	case entry.Role == core.ChatRoleMessage:
		stableID = cloneString(entry.MessageID)
	case entry.Role == core.ChatRoleTool:
		stableID = cloneString(entry.StableID)
	case entry.Source.Item != nil:
		if _, ok := entry.Source.Item.Body.(core.TranscriptBodyTerminalOutput); ok {
			stableID = cloneString(entry.StableID)
		}
	}
	lines := make([]string, len(source))
	for i, line := range source {
		if entry.Role == core.ChatRoleMessage {
			lines[i] = line
		} else {
			lines[i] = TruncateBrowserLine(line)
		}
	}
	var toolStatus *string
	if entry.Role == core.ChatRoleTool {
		state := ToolStateName(entry)
		toolStatus = &state
	}
	var diffstats []BrowserDiffStat
	for _, line := range entry.ToolDiffstats {
		if stat, ok := ParseDiffstat(line); ok {
			diffstats = append(diffstats, stat)
		}
	}
	return BrowserTranscriptEntry{
		CommandID:         commandID,
		StableID:          stableID,
		ToolDetailTrimmed: entry.ToolDetailTrimmed,
		ID:                entry.StartSeq,
		UpdatedSeq:        entry.Seq,
		Role:              role,
		Label:             label,
		RecordedAtMs:      cloneInt64(entry.RecordedAtMs),
		ToolEndedAtMs:     cloneInt64(entry.ToolEndedAtMs),
		Generated:         entry.Role == core.ChatRoleUser && !entry.IsUserAuthored(),
		Lines:             lines,
		Glyph:             EntryGlyph(entry),
		Tone:              EntryTone(entry),
		ToolStatus:        toolStatus,
		Diffstats:         diffstats,
	}
}

func cloneString(value *string) *string {
	if value == nil {
		return nil
	}
	copied := *value
	return &copied
}

// rustLines is Rust's str::lines: lines split on `\n`, each without a
// trailing `\r`, and no final empty line for a trailing newline.
func rustLines(text string) []string {
	lines := []string{}
	for text != "" {
		line, rest, found := strings.Cut(text, "\n")
		if found {
			// Only a carriage return before a line feed ends the line.
			line = strings.TrimSuffix(line, "\r")
		}
		lines = append(lines, line)
		text = rest
	}
	return lines
}

// BrowserProjectionEntries builds the rows the browser receives from the
// same collapse decisions the Rich terminal renderer uses. A synthetic row
// is a normal ChatEntry so it can share labels, timestamps, tool state, and
// the update cursor with the rest of the projection.
func BrowserProjectionEntries(entries []core.ChatEntry, collapse []EntryCollapse, lastCompactionSeq uint64) []core.ChatEntry {
	var projected []core.ChatEntry
	index := 0
	for index < len(entries) {
		switch collapse[index].Kind {
		case EntryCollapseNone, EntryCollapseExpanded:
			entry := &entries[index]
			if entry.StartSeq > lastCompactionSeq && !entry.RawOnly {
				projected = append(projected, *entry)
			}
			index++
		case EntryCollapseOmitted, EntryCollapseHidden:
			index++
		case EntryCollapseSummary:
			end := collapse[index].End
			var members []*core.ChatEntry
			for i := index; i < end; i++ {
				if entries[i].StartSeq > lastCompactionSeq && !entries[i].RawOnly {
					members = append(members, &entries[i])
				}
			}
			projected = append(projected, collapsedStreakEntryRefs(members)...)
			index = end
		}
	}
	return projected
}

// RichPresentationKey is the stable topology signature for Rich's collapsed
// tool groups. Ordinary appends and content revisions deliberately do not
// participate: they can use the normal append/update cursor. A changed
// group membership produces a new key, which tells an append-only browser
// to rebuild its DOM.
func RichPresentationKey(entries []core.ChatEntry, collapse []EntryCollapse, lastCompactionSeq uint64) string {
	digest := sha256.New()
	digest.Write([]byte("rich-presentation-v2"))
	writeSeq := func(seq uint64) {
		var encoded [8]byte
		binary.LittleEndian.PutUint64(encoded[:], seq)
		digest.Write(encoded[:])
	}
	index := 0
	for index < len(entries) {
		switch collapse[index].Kind {
		case EntryCollapseSummary:
			end := collapse[index].End
			// Include the complete visible membership of the synthetic row,
			// including an omitted/raw-only member. That member is
			// structurally transparent to the browser rows, but its insertion
			// can change which durable entries are represented by a collapsed
			// run.
			visible := false
			for i := index; i < end; i++ {
				if entries[i].StartSeq > lastCompactionSeq {
					visible = true
					break
				}
			}
			if visible {
				digest.Write([]byte("S"))
				for i := index; i < end; i++ {
					entry := &entries[i]
					if entry.StartSeq <= lastCompactionSeq {
						continue
					}
					if entry.RawOnly {
						digest.Write([]byte("O"))
					} else {
						digest.Write([]byte("M"))
					}
					digest.Write(RoleTag(entry.Role))
					writeSeq(entry.StartSeq)
				}
			}
			index = end
		case EntryCollapseHidden, EntryCollapseOmitted:
			if entries[index].StartSeq <= lastCompactionSeq {
				index++
				continue
			}
			if collapse[index].Kind == EntryCollapseHidden {
				digest.Write([]byte("H"))
			} else {
				digest.Write([]byte("O"))
			}
			digest.Write(RoleTag(entries[index].Role))
			writeSeq(entries[index].StartSeq)
			index++
		default:
			index++
		}
	}
	return core.LowerHex(digest.Sum(nil))
}

func RoleTag(role core.ChatRole) []byte {
	switch role {
	case core.ChatRoleUser:
		return []byte("user")
	case core.ChatRoleMessage:
		return []byte("message")
	case core.ChatRoleAgent:
		return []byte("agent")
	case core.ChatRoleThought:
		return []byte("thought")
	case core.ChatRoleTool:
		return []byte("tool")
	case core.ChatRolePlan:
		return []byte("plan")
	case core.ChatRolePlanProposal:
		return []byte("plan-proposal")
	}
	return []byte("system")
}

// EntryTone is the semantic colour name for one entry.
//
// A tool takes its tone from its state, because a failed tool call is the
// one thing in a transcript a person most needs to find.
func EntryTone(entry *core.ChatEntry) string {
	switch entry.Role {
	case core.ChatRoleUser:
		return "user"
	case core.ChatRoleMessage:
		return "message"
	case core.ChatRoleAgent:
		return "agent"
	case core.ChatRoleThought:
		return "thinking"
	case core.ChatRoleTool:
		switch {
		case entry.EndedAfterInterrupt:
			return "system"
		case entry.AwaitingApproval:
			return "approval"
		}
		status := core.ToolStatusPending
		if entry.ToolStatus != nil {
			status = *entry.ToolStatus
		}
		switch status {
		case core.ToolStatusCompleted:
			return "done"
		case core.ToolStatusFailed:
			return "failed"
		}
		return "running"
	case core.ChatRolePlan:
		return "plan"
	case core.ChatRolePlanProposal:
		return "plan-proposal"
	}
	return "system"
}

// ParseDiffstat reads back what `format_diffstat` wrote.
//
// It emits the path, two spaces, `+{insertions}`, a space, and
// `-{deletions}` using a Unicode MINUS SIGN. Parsing it here rather than in
// the browser keeps one definition of the format beside the code that
// produces it, and means a line that does not match is dropped rather than
// mangled.
func ParseDiffstat(line string) (BrowserDiffStat, bool) {
	split := strings.LastIndex(line, "  ")
	if split < 0 {
		return BrowserDiffStat{}, false
	}
	path, counts := line[:split], line[split+2:]
	added, removed, ok := strings.Cut(counts, " ")
	if !ok {
		return BrowserDiffStat{}, false
	}
	addedDigits, ok := strings.CutPrefix(added, "+")
	if !ok {
		return BrowserDiffStat{}, false
	}
	insertions, err := parseRustUint32(addedDigits)
	if err != nil {
		return BrowserDiffStat{}, false
	}
	removedDigits, ok := strings.CutPrefix(removed, "−")
	if !ok {
		if removedDigits, ok = strings.CutPrefix(removed, "-"); !ok {
			return BrowserDiffStat{}, false
		}
	}
	deletions, err := parseRustUint32(removedDigits)
	if err != nil {
		return BrowserDiffStat{}, false
	}
	return BrowserDiffStat{Path: strings.TrimSpace(path), Insertions: insertions, Deletions: deletions}, true
}

// parseRustUint32 is Rust's str::parse::<u32>.
func parseRustUint32(text string) (uint32, error) {
	value, err := parseRustUint64(text)
	if err != nil || value > math.MaxUint32 {
		return 0, strconv.ErrRange
	}
	return uint32(value), nil
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
// list-derived presentation. It never leaves the process, so it hashes the
// same fields as Rust does, with FNV-1a in place of Rust's DefaultHasher.
func EntryRenderFingerprint(entry *core.ChatEntry) uint64 {
	hasher := fnv.New64a()
	var encoded [8]byte
	for _, value := range []uint64{entry.StartSeq, entry.Revision} {
		binary.LittleEndian.PutUint64(encoded[:], value)
		hasher.Write(encoded[:])
	}
	for _, flag := range []bool{entry.RawOnly, entry.LeadingOmitted, entry.ToolDetailTrimmed, entry.EndedAfterInterrupt, entry.AwaitingApproval} {
		if flag {
			hasher.Write([]byte{1})
		} else {
			hasher.Write([]byte{0})
		}
	}
	return hasher.Sum64()
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
	hidden := make([]bool, len(entries))
	latestRestart := -1
	for index := range entries {
		entry := &entries[index]
		if mode == TranscriptRenderModeRich && entry.RawOnly {
			continue
		}
		if entry.IsSessionRestart() {
			if latestRestart >= 0 {
				hidden[latestRestart] = true
			}
			latestRestart = index
		} else {
			latestRestart = -1
		}
	}
	return hidden
}

// RestartCollapseWindowStartSeq is the conservative reset boundary for a
// browser feed whose previous restart marker may have been removed by a
// later projection. The wire shape only carries one `window_start_seq`, so
// use the newest replacement marker across all collapsed runs. This can
// resend a little history, but it guarantees a client never retains a
// hidden entry in its append-only DOM.
func RestartCollapseWindowStartSeq(entries []core.ChatEntry, collapsedRestarts []bool, mode TranscriptRenderMode) *uint64 {
	latestRestart := -1
	var boundary *uint64
	for index := range entries {
		entry := &entries[index]
		if mode == TranscriptRenderModeRich && entry.RawOnly {
			continue
		}
		if entry.IsSessionRestart() {
			if latestRestart >= 0 && collapsedRestarts[latestRestart] {
				seq := entry.Seq
				if boundary != nil {
					seq = max(seq, *boundary)
				}
				boundary = &seq
			}
			latestRestart = index
		} else {
			latestRestart = -1
		}
	}
	return boundary
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
	states := make([]EntryCollapse, len(entries))
	expanded := func(index int) bool {
		_, ok := expandedToolCalls[entries[index].StartSeq]
		return ok
	}
	if mode == TranscriptRenderModeRich {
		for index := range entries {
			entry := &entries[index]
			if entry.RawOnly {
				states[index] = EntryCollapse{Kind: EntryCollapseOmitted}
			} else if (entry.Role == core.ChatRoleTool || entry.ToolDetailTrimmed) && expanded(index) {
				states[index] = EntryCollapse{Kind: EntryCollapseExpanded}
			}
		}
	}
	if mode != TranscriptRenderModeRich {
		for index, hidden := range CollapsedSessionRestartStates(entries, mode) {
			if hidden {
				states[index] = EntryCollapse{Kind: EntryCollapseHidden}
			}
		}
		return states
	}
	// Interrupted tools and tools awaiting approval keep their own rows.
	streakMember := func(index int) bool {
		entry := &entries[index]
		return tools == core.ToolOutputGrouped &&
			(entry.Role == core.ChatRoleThought ||
				(IsCompletedTool(entry) && !entry.EndedAfterInterrupt && !entry.AwaitingApproval && !expanded(index)))
	}
	start := 0
	for start < len(entries) {
		if !streakMember(start) {
			start++
			continue
		}
		// end stops at the last visible member, so an omitted entry the
		// streak reached across is only inside it when another member
		// follows.
		end := start + 1
		cursor := start + 1
		for cursor < len(entries) {
			if streakMember(cursor) {
				cursor++
				end = cursor
			} else if entries[cursor].RawOnly {
				cursor++
			} else {
				break
			}
		}
		members := entries[start:end]
		thoughts, completed := 0, 0
		for i := range members {
			if members[i].Role == core.ChatRoleThought {
				thoughts++
			}
			if IsCompletedTool(&members[i]) {
				completed++
			}
		}
		toolPrecedesThought := false
		for i := range members {
			if !members[i].RawOnly {
				toolPrecedesThought = members[i].Role == core.ChatRoleTool && thoughts > 0
				break
			}
		}
		if thoughts > 1 || completed > 1 || toolPrecedesThought {
			// A member's update does not bump the head's revision, so fold
			// the streak's revisions into the head's state: its cached rows
			// then drop whenever any member changes.
			fingerprint := CollapseRevisionFingerprint(members)
			states[start] = EntryCollapse{Kind: EntryCollapseSummary, End: end, Fingerprint: fingerprint}
			// Omitted members render nothing either way, so one state covers
			// everything the head speaks for.
			for i := start + 1; i < end; i++ {
				states[i] = EntryCollapse{Kind: EntryCollapseHidden}
			}
		}
		start = end
	}
	// Apply restart coalescing last: unlike tool streaks, restart markers
	// are never streak members, but keeping this pass last makes that
	// invariant explicit if more Rich collapse rules are added later.
	for index, hidden := range CollapsedSessionRestartStates(entries, mode) {
		if hidden {
			states[index] = EntryCollapse{Kind: EntryCollapseHidden}
		}
	}
	return states
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
	var tools []*core.ChatEntry
	for _, member := range members {
		if IsCompletedTool(member) {
			tools = append(tools, member)
		}
	}
	summaries := make([]string, len(tools))
	for i, member := range tools {
		if member.ToolSummary != nil {
			summaries[i] = *member.ToolSummary
		} else {
			summaries[i] = member.Text
		}
	}
	joined := strings.Join(summaries, ", ")
	first := tools[0]
	summary := core.ChatEntryTool(first.StartSeq, joined, nil, core.ToolStatusCompleted)
	summary.Seq = first.Seq
	summary.Revision = first.Revision
	for i, member := range members {
		if i == 0 || member.Seq > summary.Seq {
			summary.Seq = member.Seq
		}
		if i == 0 || member.Revision > summary.Revision {
			summary.Revision = member.Revision
		}
	}
	for i := len(tools) - 1; i >= 0; i-- {
		if tools[i].RecordedAtMs != nil {
			summary.RecordedAtMs = cloneInt64(tools[i].RecordedAtMs)
			break
		}
	}
	summary.ToolSummary = &joined
	return summary
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
	var projected []core.ChatEntry
	for i := len(members) - 1; i >= 0; i-- {
		if members[i].Role == core.ChatRoleThought {
			projected = append(projected, *members[i])
			break
		}
	}
	var tools []*core.ChatEntry
	for _, member := range members {
		if IsCompletedTool(member) {
			tools = append(tools, member)
		}
	}
	if len(tools) >= 2 {
		projected = append(projected, collapsedToolEntryRefs(members))
	} else {
		for _, tool := range tools {
			projected = append(projected, *tool)
		}
	}
	return projected
}

// EntryGlyph is the mark the browser projection sends for one entry. The
// browser always has UTF-8, so these stay Unicode; a terminal reads its
// marks from the symbol set in force instead (mj-chat's `role_glyph`),
// because a Linux console or a locale without UTF-8 draws mojibake for them.
func EntryGlyph(entry *core.ChatEntry) string {
	switch entry.Role {
	case core.ChatRoleUser:
		return "❯"
	case core.ChatRoleMessage:
		return "←"
	case core.ChatRoleAgent:
		return "●"
	case core.ChatRoleThought:
		return "○"
	case core.ChatRolePlan:
		return "◇"
	case core.ChatRolePlanProposal:
		return "◈"
	case core.ChatRoleTool:
		switch {
		case entry.EndedAfterInterrupt:
			return "■"
		case entry.AwaitingApproval:
			return "?"
		}
		status := core.ToolStatusPending
		if entry.ToolStatus != nil {
			status = *entry.ToolStatus
		}
		switch status {
		case core.ToolStatusCompleted:
			return "✓"
		case core.ToolStatusFailed:
			return "×"
		}
		return "●"
	}
	return "─"
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
	p.entries = MaterializedChatEntriesReusing(session, 0, p.entries)
	releaseRawViewDetail(p.entries)
	transcript := BrowserTranscript(p.entries, session.AppliedEventOrdinal, 0, nil)
	first := len(p.published) == 0
	for _, entry := range transcript.Entries {
		// Freshly rendered rows have one owner until publication below.
		previous, found := p.published[entry.Identity()]
		switch {
		case found && sameRendering(previous, entry):
			entry.UpdatedSeq = previous.UpdatedSeq
		case found:
			entry.UpdatedSeq = transcript.LatestSeq
		case !first:
			entry.UpdatedSeq = max(entry.UpdatedSeq, transcript.LatestSeq)
		}
	}
	p.published = make(map[string]*BrowserTranscriptEntry, len(transcript.Entries))
	for _, entry := range transcript.Entries {
		p.published[entry.Identity()] = entry
	}
	return transcript
}

// sameRendering is whether two published entries render the same, whatever
// cursor they carry.
func sameRendering(previous, next *BrowserTranscriptEntry) bool {
	return optionalStringEqual(previous.StableID, next.StableID) &&
		optionalStringEqual(previous.CommandID, next.CommandID) &&
		previous.ID == next.ID &&
		previous.Role == next.Role &&
		previous.Label == next.Label &&
		quotaOptionalEqual(previous.RecordedAtMs, next.RecordedAtMs) &&
		quotaOptionalEqual(previous.ToolEndedAtMs, next.ToolEndedAtMs) &&
		previous.Generated == next.Generated &&
		slices.Equal(previous.Lines, next.Lines) &&
		slices.EqualFunc(previous.MailboxEvents, next.MailboxEvents, slices.Equal[[]string, string]) &&
		previous.Glyph == next.Glyph &&
		previous.Tone == next.Tone &&
		optionalStringEqual(previous.ToolStatus, next.ToolStatus) &&
		previous.ToolDetailTrimmed == next.ToolDetailTrimmed &&
		slices.Equal(previous.Diffstats, next.Diffstats)
}

func optionalStringEqual(a, b *string) bool { return quotaOptionalEqual(a, b) }
