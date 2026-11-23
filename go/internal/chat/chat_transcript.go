// Port of mj-chat/src/chat/transcript.rs.

// The projected conversation: entries built from the controller's logical
// session, the render cache and collapse rules behind them, the scrollable
// viewport, and the rows every surface draws.
//
// Rust re-exports mj_client::transcript::* and the mj_transcript text helpers
// here; the Go port names them through their own packages (client.X,
// transcript.X, core.ToolStatusOf).

package chat

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"maps"
	"math"
	"slices"
	"sort"
	"strings"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	acp "github.com/BrokkAi/acp-go/schema/unstable"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// TailSeedItems is mj_client::transcript::TAIL_SEED_ITEMS, which Rust
// re-exports from this module.
const TailSeedItems = client.TailSeedItems

// TranscriptViewport is the rows a viewport shows, with both the anchor to
// persist and the concrete position the rows start at.
type TranscriptViewport struct {
	rows []Line
	// anchor is the anchor to store: Bottom whenever these rows reach the
	// newest row.
	anchor TranscriptAnchor
	// top is the entry and row the first visible row came from.
	top AnchorRow
}

type EntryVisual struct {
	glyph       string
	label       string
	headerStyle Style
	bodyStyle   Style
	railStyle   Style
}

type CachedEntry struct {
	fingerprint uint64
	lines       []Line
	mailbox     *RowTrack
}

// TranscriptScrollbarState is the primary transcript scrollbar. Its geometry
// is rebuilt after every draw. During a drag, frozen row estimates keep newly
// rendered history from shifting the thumb under the pointer.
type TranscriptScrollbarState struct {
	pointer          ScrollbarDrag
	metricWidth      int
	metricMode       client.TranscriptRenderMode
	metricGeneration uint64
	estimates        []int
	// textEstimates are text-based row estimates for entries not yet drawn,
	// with the entry render fingerprint each was made from. Cleared when the
	// width, mode, or tool layout changes, or when entries move.
	textEstimates     []transcriptTextEstimate
	textEstimateTools *ToolDisplay
	// userMessages are the messages the user wrote, in content order. They
	// are rebuilt and frozen with estimates.
	userMessages []UserMessageRow
	// marks are the user-message marks drawn on the latest geometry.
	marks []UserMessageMark
}

// transcriptTextEstimate is one element of
// TranscriptScrollbarState.textEstimates, Rust's Option<(u64, usize)>: the
// zero value is None.
type transcriptTextEstimate struct {
	known       bool
	fingerprint uint64
	rows        int
}

// UserMessageRow is a message the user wrote and its estimated first content
// row.
type UserMessageRow struct {
	entry int
	row   int
}

// UserMessageMark is a user-message mark on the scrollbar track. A click on
// it opens entry.
type UserMessageMark struct {
	trackRow int
	entry    int
}

// userMessageMarks lays out user-message marks on a scrollbar.
//
// There are no marks when all the content fits, and none under the thumb,
// because those messages are already on screen. Marks on adjacent or equal
// track rows merge into one mark for the oldest message of the run, so
// closely spaced messages cannot fill the track. The thumb ends a run.
func userMessageMarks(geometry ScrollbarGeometry, contentLength int, messages []UserMessageRow) []UserMessageMark {
	var marks []UserMessageMark
	if geometry.MaxScroll == 0 {
		return marks
	}
	thumbTop, thumbBottom := geometry.Thumb.Min.Y, geometry.Thumb.Max.Y
	runEnd, inRun := 0, false
	for _, message := range messages {
		trackRow := ScrollbarMarkRow(geometry, contentLength, message.row)
		if trackRow >= thumbTop && trackRow < thumbBottom {
			continue
		}
		joinsRun := inRun && trackRow <= runEnd+1
		runEnd, inRun = trackRow, true
		if !joinsRun {
			marks = append(marks, UserMessageMark{trackRow: trackRow, entry: message.entry})
		}
	}
	return marks
}

func defaultTranscriptScrollbarState() TranscriptScrollbarState {
	return TranscriptScrollbarState{metricMode: client.TranscriptRenderModeRich}
}

func (s *TranscriptScrollbarState) clearGeometry() {
	s.pointer.Clear()
	s.marks = nil
}

func (s *TranscriptScrollbarState) clear() {
	s.clearGeometry()
	s.metricWidth = 0
	s.metricGeneration = 0
	s.estimates = nil
	s.textEstimates = nil
	s.textEstimateTools = nil
	s.userMessages = nil
}

// ToolDisplay is how the Rich transcript draws tool calls, from Settings ›
// Advanced.
type ToolDisplay struct {
	Layout core.ToolOutput
	// OutputLines is the output rows each inline call shows.
	OutputLines int
}

func ToolDisplayFromConfig(advanced *core.AdvancedConfig) ToolDisplay {
	return ToolDisplay{Layout: advanced.ToolOutput, OutputLines: advanced.ToolOutputLines}
}

// view is how an entry this layout leaves on its own draws its call.
func (d ToolDisplay) view() ToolView {
	if d.Layout == core.ToolOutputInline {
		return ToolViewInline{OutputLines: d.OutputLines}
	}
	return ToolViewSummary{}
}

func DefaultToolDisplay() ToolDisplay {
	advanced := core.DefaultAdvancedConfig()
	return ToolDisplayFromConfig(&advanced)
}

// HydratedToolDetail is the full body of a trimmed tool call: loading, the
// full entry, or why it is unavailable.
type HydratedToolDetail interface{ isHydratedToolDetail() }

type HydratedToolDetailLoading struct{}

type HydratedToolDetailFull struct{ Entry *core.ChatEntry }

type HydratedToolDetailUnavailable struct{ Reason string }

func (HydratedToolDetailLoading) isHydratedToolDetail()     {}
func (HydratedToolDetailFull) isHydratedToolDetail()        {}
func (HydratedToolDetailUnavailable) isHydratedToolDetail() {}

// ToolDetailRevision is Rust's (i64, Option<u64>): the stored item's
// last_changed_at_ms and latest_content_event_ordinal. It is comparable, so
// revisions compare by value as the Rust tuple does.
type ToolDetailRevision struct {
	LastChangedAtMs              int64
	LatestContentEventOrdinal    uint64
	HasLatestContentEventOrdinal bool
}

func entryToolDetailRevision(entry *core.ChatEntry) (ToolDetailRevision, bool) {
	item := entry.Source.Item
	if item == nil {
		return ToolDetailRevision{}, false
	}
	revision := ToolDetailRevision{LastChangedAtMs: item.LastChangedAtMs}
	if item.LatestContentEventOrdinal != nil {
		revision.LatestContentEventOrdinal = *item.LatestContentEventOrdinal
		revision.HasLatestContentEventOrdinal = true
	}
	return revision, true
}

type TranscriptRenderCache struct {
	theme core.UiTheme
	// ascii is whether the rows were drawn with the ASCII symbol set; a
	// change of set redraws them, like a change of palette.
	ascii                    bool
	width                    int
	mode                     client.TranscriptRenderMode
	tools                    ToolDisplay
	entries                  []*CachedEntry
	collapse                 []client.EntryCollapse
	collapseInputFingerprint uint64
	expandedMailboxEvents    map[MailboxDisclosureId]struct{}
	// hydratedToolDetails: the active conversation owns these bodies and
	// drops them on a session switch.
	hydratedToolDetails map[string]hydratedToolDetailState
}

// hydratedToolDetailState is a value of
// TranscriptRenderCache.hydratedToolDetails, Rust's
// (ToolDetailRevision, HydratedToolDetail).
type hydratedToolDetailState struct {
	revision ToolDetailRevision
	detail   HydratedToolDetail
}

// TranscriptClickTarget is a frame-local target for toggling a tool or
// mailbox event. The target is rebuilt with the rows so a click can never
// address a call that moved after scrolling or wrapping.
type TranscriptClickTarget struct {
	Rect     uv.Rectangle
	StartSeq uint64
	// MailboxEvent is nil for a tool target (Rust's None).
	MailboxEvent MailboxDisclosureId
}

// TranscriptLinkFrame is where one transcript frame drew its rows. A link
// click is resolved against this frame's rows, the same way tool click
// targets are.
type TranscriptLinkFrame struct {
	inner       uv.Rectangle
	top         AnchorRow
	visibleRows int
}

// TranscriptSnapshot is a read-only copy of the projected conversation that
// can be rendered by surfaces other than the interactive chat view.
type TranscriptSnapshot struct {
	// entries are shared (Rust's Arc<Vec<ChatEntry>>) and never mutated.
	entries           []core.ChatEntry
	latestSeq         uint64
	lastCompactionSeq uint64
	renderCache       TranscriptRenderCache
}

func TranscriptSnapshotFromEntries(entries []core.ChatEntry) *TranscriptSnapshot {
	var latestSeq uint64
	if len(entries) > 0 {
		latestSeq = entries[len(entries)-1].Seq
	}
	return TranscriptSnapshotFromEntriesAt(entries, latestSeq)
}

func TranscriptSnapshotFromEntriesAt(entries []core.ChatEntry, latestSeq uint64) *TranscriptSnapshot {
	return &TranscriptSnapshot{
		entries:     entries,
		latestSeq:   latestSeq,
		renderCache: defaultTranscriptRenderCache(),
	}
}

// TranscriptSnapshotFromMaterialized renders the controller's durable
// logical-session projection. Relay ordinals remain the public cursor: an
// item's first ordinal is its stable browser identity, and its update cursor
// is the latest ordinal the projection records for it (see
// client.ItemUpdateOrdinal), so a delta carries the entries that changed
// rather than the whole window.
func TranscriptSnapshotFromMaterialized(session *core.MaterializedSession) *TranscriptSnapshot {
	return TranscriptSnapshotFromMaterializedWithDiffstats(session, map[string][]string{})
}

// TranscriptSnapshotFromMaterializedWithDiffstats renders a materialized
// session with exact diffstats computed by the caller's background
// projection. Missing entries deliberately fall back to path-only labels;
// rendering never performs the expensive diff.
func TranscriptSnapshotFromMaterializedWithDiffstats(session *core.MaterializedSession, diffstats map[string][]string) *TranscriptSnapshot {
	entries := client.MaterializedChatEntriesWithDiffstats(session, diffstats)
	return TranscriptSnapshotFromEntriesAt(entries, session.AppliedEventOrdinal)
}

func (s *TranscriptSnapshot) HasAssistantMessages() bool {
	return slices.ContainsFunc(s.entries, func(entry core.ChatEntry) bool {
		return entry.Role == core.ChatRoleAgent && strings.TrimSpace(entry.Text) != ""
	})
}

// TranscriptSnapshotFromMaterializedReusing reuses conversion of unchanged
// stored items during background preparation.
func TranscriptSnapshotFromMaterializedReusing(session *core.MaterializedSession, diffstats map[string][]string, previous []core.ChatEntry, previousDiffstats map[string][]string) *TranscriptSnapshot {
	entries := make([]core.ChatEntry, 0, len(session.Transcript))
	for index, item := range session.Transcript {
		exact, hasExact := diffstats[item.StableID]
		previousExact, hasPreviousExact := previousDiffstats[item.StableID]
		sameExact := hasExact == hasPreviousExact && slices.Equal(exact, previousExact)
		if index < len(previous) {
			entry := previous[index]
			if (entry.Source.Is(item) || (entry.Source.Item != nil && client.TranscriptItemEqual(entry.Source.Item, item))) && sameExact {
				entry.Seq = client.ItemUpdateOrdinal(item, session.AppliedEventOrdinal)
				entry.Source = core.TranscriptSource{Item: item}
				entries = append(entries, entry)
				continue
			}
		}
		var exactDiffstats *[]string
		if hasExact {
			exactDiffstats = &exact
		}
		entries = append(entries, client.MaterializedChatEntryWithDiffstats(item, session.AppliedEventOrdinal, exactDiffstats))
	}
	client.ApplyEntryListRules(entries, session.PendingElicitations)
	return TranscriptSnapshotFromEntriesAt(entries, session.AppliedEventOrdinal)
}

func (s *TranscriptSnapshot) ConvertedEntries() []core.ChatEntry {
	return s.entries
}

// RichTailScrolled is the last maximumLines non-empty rows, skipping scroll
// rows above the live tail. Renders only the entries the window touches.
// Returns the rows and the scroll actually applied, clamped to the history
// available.
func (s *TranscriptSnapshot) RichTailScrolled(width int, maximumLines int, scroll int, tools ToolDisplay) ([]Line, int) {
	prepareRenderCache(
		s.entries,
		&s.renderCache,
		width,
		client.TranscriptRenderModeRich,
		tools,
		map[uint64]struct{}{},
		map[MailboxDisclosureId]struct{}{},
	)
	wanted := transcriptSaturatingAdd(maximumLines, scroll)
	// collected holds the rows newest first; it is reversed at the end, as
	// Rust pushes each row onto the front of a deque.
	var collected []Line
entries:
	for index := len(s.entries) - 1; index >= 0; index-- {
		lines := cachedEntryLines(s.entries, &s.renderCache, index)
		for row := len(lines) - 1; row >= 0; row-- {
			if lineIsEmpty(lines[row]) {
				continue
			}
			if len(collected) >= wanted {
				break entries
			}
			collected = append(collected, lines[row])
		}
	}
	slices.Reverse(collected)
	applied := min(scroll, max(len(collected)-maximumLines, 0))
	end := len(collected) - applied
	start := max(end-maximumLines, 0)
	return slices.Clone(collected[start:end]), applied
}

func (s *TranscriptSnapshot) BrowserTranscript(afterSeq *uint64) client.WebBrowserTranscript {
	return client.BrowserTranscript(s.entries, s.latestSeq, s.lastCompactionSeq, afterSeq)
}

func (s *TranscriptSnapshot) BrowserTail(maximumLines int) []string {
	var lines []string
	for _, entry := range s.BrowserTranscript(nil).Entries {
		for index, line := range entry.Lines {
			line = strings.TrimSpace(line)
			if line == "" {
				continue
			}
			if index == 0 {
				line = entry.Label + ": " + line
			}
			lines = append(lines, line)
		}
	}
	start := max(len(lines)-maximumLines, 0)
	return lines[start:]
}

// TranscriptAnchor is where the transcript viewport is pinned. Anchoring to
// an entry rather than an absolute row keeps the view stable while the agent
// appends new rows below, and lets the renderer touch only the entries the
// viewport covers. Every variant is comparable, so anchors compare with ==.
type TranscriptAnchor interface{ isTranscriptAnchor() }

// TranscriptAnchorBottom follows the newest rows.
type TranscriptAnchorBottom struct{}

// TranscriptAnchorRow: the top visible row is Row rows into Entry.
type TranscriptAnchorRow struct {
	Entry int
	Row   int
}

// TranscriptAnchorRestoring keeps the saved position until its entry is
// available, including across draws or another detach while the history is
// still loading.
type TranscriptAnchorRestoring struct{ Position TranscriptPosition }

func (TranscriptAnchorBottom) isTranscriptAnchor()    {}
func (TranscriptAnchorRow) isTranscriptAnchor()       {}
func (TranscriptAnchorRestoring) isTranscriptAnchor() {}

// anchorIsBottom is `anchor == TranscriptAnchor::Bottom`.
func anchorIsBottom(anchor TranscriptAnchor) bool {
	_, bottom := anchor.(TranscriptAnchorBottom)
	return bottom
}

// TranscriptPosition is a session-local transcript position, retained when
// its view is replaced. It serializes as its SavedTranscriptAnchor (a serde
// newtype struct).
type TranscriptPosition struct{ anchor SavedTranscriptAnchor }

// TranscriptPositionAtStableID starts an opening view at a materialized
// transcript item. Resolution can wait for the older prefix to finish
// loading.
func TranscriptPositionAtStableID(stableID string) TranscriptPosition {
	return TranscriptPosition{anchor: SavedTranscriptAnchor{
		Kind:        SavedTranscriptAnchorEntry,
		StableID:    stableID,
		HasStableID: true,
	}}
}

func (p TranscriptPosition) MarshalJSON() ([]byte, error) {
	return p.anchor.MarshalJSON()
}

func (p *TranscriptPosition) UnmarshalJSON(data []byte) error {
	return p.anchor.UnmarshalJSON(data)
}

// SavedTranscriptAnchorKind names the variants of SavedTranscriptAnchor.
type SavedTranscriptAnchorKind int

const (
	SavedTranscriptAnchorBottom SavedTranscriptAnchorKind = iota
	// SavedTranscriptAnchorRow: previous builds wrote entry indexes into
	// terminal upgrade handoffs.
	SavedTranscriptAnchorRow
	// SavedTranscriptAnchorEntry is durable entry identity, independent of
	// how much history is loaded.
	SavedTranscriptAnchorEntry
)

// SavedTranscriptAnchor is serde's externally tagged enum, as one comparable
// struct with a kind: "Bottom", {"Row":{"entry":..,"row":..}}, or
// {"Entry":{"start_seq":..,"stable_id":..,"row":..,"revealed_through":..}}.
// The zero value is Bottom.
type SavedTranscriptAnchor struct {
	Kind SavedTranscriptAnchorKind
	// Entry is the entry index of a Row anchor.
	Entry int
	// Row is the row into the entry, for Row and Entry anchors.
	Row int
	// StartSeq, StableID and RevealedThrough belong to Entry. An opening
	// reveal follows new content, but keeps its visible row when the
	// conversation has not changed since the last draw (RevealedThrough).
	StartSeq           uint64
	StableID           string
	HasStableID        bool
	RevealedThrough    uint64
	HasRevealedThrough bool
}

// transcriptMarshalJSON encodes v as serde_json does: compact, without
// encoding/json's HTML escaping.
func transcriptMarshalJSON(v any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

// savedTranscriptAnchorVariants is serde's expected-variant list.
const savedTranscriptAnchorVariants = "expected one of `Bottom`, `Row`, `Entry`"

func (a SavedTranscriptAnchor) MarshalJSON() ([]byte, error) {
	switch a.Kind {
	case SavedTranscriptAnchorRow:
		return transcriptMarshalJSON(map[string]any{"Row": struct {
			Entry int `json:"entry"`
			Row   int `json:"row"`
		}{a.Entry, a.Row}})
	case SavedTranscriptAnchorEntry:
		var stableID *string
		if a.HasStableID {
			stableID = &a.StableID
		}
		var revealedThrough *uint64
		if a.HasRevealedThrough {
			revealedThrough = &a.RevealedThrough
		}
		return transcriptMarshalJSON(map[string]any{"Entry": struct {
			StartSeq        uint64  `json:"start_seq"`
			StableID        *string `json:"stable_id,omitempty"`
			Row             int     `json:"row"`
			RevealedThrough *uint64 `json:"revealed_through,omitempty"`
		}{a.StartSeq, stableID, a.Row, revealedThrough}})
	default:
		return []byte(`"Bottom"`), nil
	}
}

// UnmarshalJSON accepts what serde's derived Deserialize accepts: the unit
// variant as a string or as {"Bottom":null}, and the struct variants as a
// single-key object holding a map or a sequence of their fields. Unknown
// fields are ignored.
func (a *SavedTranscriptAnchor) UnmarshalJSON(data []byte) error {
	var name string
	if err := json.Unmarshal(data, &name); err == nil {
		switch name {
		case "Bottom":
			*a = SavedTranscriptAnchor{}
			return nil
		case "Row", "Entry":
			return errors.New("invalid type: unit variant, expected struct variant")
		default:
			return fmt.Errorf("unknown variant `%s`, %s", name, savedTranscriptAnchorVariants)
		}
	}
	var outer map[string]json.RawMessage
	if err := json.Unmarshal(data, &outer); err != nil {
		return errors.New("invalid type, expected enum SavedTranscriptAnchor")
	}
	if len(outer) != 1 {
		return errors.New("invalid length, expected map with a single key")
	}
	for name, body := range outer {
		switch name {
		case "Bottom":
			if strings.TrimSpace(string(body)) != "null" {
				return errors.New("invalid type, expected unit variant SavedTranscriptAnchor::Bottom")
			}
			*a = SavedTranscriptAnchor{}
			return nil
		case "Row":
			return a.decodeRow(body)
		case "Entry":
			return a.decodeEntry(body)
		default:
			return fmt.Errorf("unknown variant `%s`, %s", name, savedTranscriptAnchorVariants)
		}
	}
	return nil
}

// savedAnchorFields decodes a struct variant's body given as a map or as a
// sequence (serde's derived visitors accept both). names lists the fields
// in declaration order; the result maps each present field to its raw value.
func savedAnchorFields(body []byte, names []string) (map[string]json.RawMessage, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(body, &fields); err == nil {
		return fields, nil
	}
	var sequence []json.RawMessage
	if err := json.Unmarshal(body, &sequence); err != nil {
		return nil, errors.New("invalid type, expected a struct variant")
	}
	if len(sequence) > len(names) {
		return nil, fmt.Errorf("invalid length %d, expected fewer elements in array", len(sequence))
	}
	fields = make(map[string]json.RawMessage, len(sequence))
	for index, value := range sequence {
		fields[names[index]] = value
	}
	return fields, nil
}

// savedAnchorUsize decodes a required usize field.
func savedAnchorUsize(fields map[string]json.RawMessage, name string) (int, error) {
	raw, ok := fields[name]
	if !ok {
		return 0, fmt.Errorf("missing field `%s`", name)
	}
	var value uint64
	if err := json.Unmarshal(raw, &value); err != nil {
		return 0, fmt.Errorf("field `%s`: %w", name, err)
	}
	if value > math.MaxInt {
		return 0, fmt.Errorf("field `%s`: invalid value %d, expected usize", name, value)
	}
	return int(value), nil
}

func (a *SavedTranscriptAnchor) decodeRow(body []byte) error {
	fields, err := savedAnchorFields(body, []string{"entry", "row"})
	if err != nil {
		return err
	}
	entry, err := savedAnchorUsize(fields, "entry")
	if err != nil {
		return err
	}
	row, err := savedAnchorUsize(fields, "row")
	if err != nil {
		return err
	}
	*a = SavedTranscriptAnchor{Kind: SavedTranscriptAnchorRow, Entry: entry, Row: row}
	return nil
}

func (a *SavedTranscriptAnchor) decodeEntry(body []byte) error {
	fields, err := savedAnchorFields(body, []string{"start_seq", "stable_id", "row", "revealed_through"})
	if err != nil {
		return err
	}
	decoded := SavedTranscriptAnchor{Kind: SavedTranscriptAnchorEntry}
	raw, ok := fields["start_seq"]
	if !ok {
		return errors.New("missing field `start_seq`")
	}
	if err := json.Unmarshal(raw, &decoded.StartSeq); err != nil {
		return fmt.Errorf("field `start_seq`: %w", err)
	}
	if raw, ok := fields["stable_id"]; ok {
		var stableID *string
		if err := json.Unmarshal(raw, &stableID); err != nil {
			return fmt.Errorf("field `stable_id`: %w", err)
		}
		if stableID != nil {
			decoded.StableID, decoded.HasStableID = *stableID, true
		}
	}
	if decoded.Row, err = savedAnchorUsize(fields, "row"); err != nil {
		return err
	}
	if raw, ok := fields["revealed_through"]; ok {
		var revealedThrough *uint64
		if err := json.Unmarshal(raw, &revealedThrough); err != nil {
			return fmt.Errorf("field `revealed_through`: %w", err)
		}
		if revealedThrough != nil {
			decoded.RevealedThrough, decoded.HasRevealedThrough = *revealedThrough, true
		}
	}
	*a = decoded
	return nil
}

// AnchorRow is a concrete transcript position: row rendered rows into entry.
type AnchorRow struct {
	entry int
	row   int
}

func (a AnchorRow) anchor() TranscriptAnchor {
	return TranscriptAnchorRow{Entry: a.entry, Row: a.row}
}

// less is Rust's derived Ord: by entry, then by row.
func (a AnchorRow) less(other AnchorRow) bool {
	return a.entry < other.entry || (a.entry == other.entry && a.row < other.row)
}

// selectionBaseRow is the content row the frozen base anchor of a transcript
// selection sits at.
//
// Absolute transcript rows would need the rendered row count of every entry
// above the viewport, and those rows only exist once something renders them.
// A selection therefore lives in a drag-local row space whose origin is this
// constant, with enough headroom either side to scroll a long way in both
// directions without the space going negative.
const selectionBaseRow int64 = 1 << 32

// selectionScrollMarginRows is how far past the last visible row a
// scrolled-back transcript claims to reach. The engine only needs more rows
// than the viewport is tall to keep a drag's edge auto-scroll armed; the
// visible band is what clamps the cursor.
const selectionScrollMarginRows = 4_096

// selectionWalkBudget is the rows, or entries, one selection walk may cross
// before the row space is declared untrustworthy. A drag moves a few rows
// per frame; anything at this scale is a jump across history the selection
// cannot describe, and dropping it is cheaper and more honest than rendering
// the deep past to measure it.
const selectionWalkBudget = 20_000

// TranscriptSelectionSpace is the row space a transcript selection is
// measured in.
//
// While no gesture is running the base is re-pinned to the viewport top
// every frame. A gesture freezes it, and each later frame reports its own top
// as the running sum of per-frame deltas — a handful of rows each, all warm
// in the render cache.
type TranscriptSelectionSpace struct {
	// base is the viewport top when the base was pinned; content row
	// selectionBaseRow.
	base AnchorRow
	// offsetRows is the rows the current viewport top sits below base.
	offsetRows int64
	// prevTop is the viewport top the previous frame registered, to walk the
	// delta from.
	prevTop AnchorRow
	// layout is the row layout the space was pinned against.
	layout TranscriptRowLayout
}

// TranscriptRowLayout is what the transcript's rendered rows depend on
// wholesale. Any change moves rows under a frozen selection, so the space is
// re-pinned and the caller drops the selection instead of copying the wrong
// text.
type TranscriptRowLayout struct {
	width      int
	mode       client.TranscriptRenderMode
	generation uint64
}

// selectionRow is the content row offset rows from the base of a selection's
// row space.
func selectionRow(offset int64) int {
	row := selectionBaseRow + offset
	if offset > 0 && row < selectionBaseRow {
		row = math.MaxInt64
	}
	if row < 0 {
		return 0
	}
	return int(row)
}

// transcriptSaturatingAdd is usize::saturating_add for the non-negative counts Rust
// keeps in usize.
func transcriptSaturatingAdd(a, b int) int {
	if sum := a + b; sum >= a {
		return sum
	}
	return math.MaxInt
}

// saturatingAddInt64 is i64::saturating_add.
func saturatingAddInt64(a, b int64) int64 {
	sum := a + b
	if b > 0 && sum < a {
		return math.MaxInt64
	}
	if b < 0 && sum > a {
		return math.MinInt64
	}
	return sum
}

// resizeCachedEntries is Vec::resize(n, None) on the cached rows.
func resizeCachedEntries(entries []*CachedEntry, n int) []*CachedEntry {
	if len(entries) >= n {
		clear(entries[n:])
		return entries[:n]
	}
	return append(entries, make([]*CachedEntry, n-len(entries))...)
}

// prepareRenderCache drops rows invalidated by width, rendering mode, tool
// display, or palette changes, and sizes the cache to the current entry
// count.
func prepareRenderCache(entries []core.ChatEntry, cache *TranscriptRenderCache, width int, mode client.TranscriptRenderMode, tools ToolDisplay, expandedToolCalls map[uint64]struct{}, expandedMailboxEvents map[MailboxDisclosureId]struct{}) {
	modeChanged := cache.mode != mode || cache.tools != tools
	collapseInputFingerprint := client.CollapseRevisionFingerprint(entries)
	if cache.width != width ||
		modeChanged ||
		cache.theme != Current() ||
		cache.ascii != ASCII() ||
		!maps.Equal(cache.expandedMailboxEvents, expandedMailboxEvents) {
		cache.theme = Current()
		cache.ascii = ASCII()
		cache.width = width
		cache.mode = mode
		cache.tools = tools
		cache.entries = resizeCachedEntries(cache.entries, 0)
		cache.expandedMailboxEvents = maps.Clone(expandedMailboxEvents)
		if cache.expandedMailboxEvents == nil {
			cache.expandedMailboxEvents = map[MailboxDisclosureId]struct{}{}
		}
	}
	cache.entries = resizeCachedEntries(cache.entries, len(entries))
	if !modeChanged &&
		len(cache.collapse) == len(entries) &&
		cache.collapseInputFingerprint == collapseInputFingerprint {
		return
	}
	collapse := client.EntryCollapseStates(entries, mode, tools.Layout, expandedToolCalls)
	for index, state := range collapse {
		if index >= len(cache.collapse) || cache.collapse[index] != state {
			cache.entries[index] = nil
		}
	}
	cache.collapse = collapse
	cache.collapseInputFingerprint = collapseInputFingerprint
}

// collapsedStreakLines renders one collapsed streak in the requested fixed
// order: newest thought, then one synthetic tool summary.
func collapsedStreakLines(members []core.ChatEntry, width int, mode client.TranscriptRenderMode) []Line {
	var lines []Line
	for _, entry := range client.CollapsedStreakEntries(members) {
		lines = append(lines, renderTranscriptEntry(&entry, width, mode)...)
	}
	return lines
}

// cachedEntryLines is the rendered rows for one entry, rendering and caching
// it on first use. prepareRenderCache must have run for the current width and
// mode.
func cachedEntryLines(entries []core.ChatEntry, cache *TranscriptRenderCache, index int) []Line {
	entry := &entries[index]
	collapse := cache.collapse[index]
	staleSummaryFingerprint := collapse.Kind == client.EntryCollapseSummary &&
		client.CollapseRevisionFingerprint(entries[index:collapse.End]) != collapse.Fingerprint
	fingerprint := client.EntryRenderFingerprint(entry)
	stale := staleSummaryFingerprint ||
		cache.entries[index] == nil ||
		cache.entries[index].fingerprint != fingerprint
	if stale {
		width := cache.width
		var lines []Line
		var mailbox *RowTrack
		renderedMailbox := false
		if cache.mode == client.TranscriptRenderModeRich {
			if rows, track, ok := renderMailboxEntry(entry, width, cache.expandedMailboxEvents); ok {
				lines, mailbox, renderedMailbox = rows, &track, true
			}
		}
		if !renderedMailbox {
			detailEntry := cache.detailEntry(entry, collapse)
			var rows []Line
			switch collapse.Kind {
			case client.EntryCollapseNone:
				rows = renderTranscriptEntryWithOptions(detailEntry, width, cache.mode, cache.tools.view())
			case client.EntryCollapseHidden, client.EntryCollapseOmitted:
				rows = nil
			case client.EntryCollapseExpanded:
				rows = renderTranscriptEntryExpanded(detailEntry, width)
			case client.EntryCollapseSummary:
				rows = collapsedStreakLines(entries[index:collapse.End], width, cache.mode)
			}
			if len(rows) > 0 &&
				(cache.mode == client.TranscriptRenderModeRaw || collapse.Kind == client.EntryCollapseExpanded) {
				notice, hasNotice := "", false
				switch detail := cache.toolDetail(entry).(type) {
				case HydratedToolDetailLoading:
					notice, hasNotice = "Loading tool details…", true
				case HydratedToolDetailUnavailable:
					notice, hasNotice = "Tool details unavailable: "+detail.Reason, true
				}
				if hasNotice {
					blank := len(rows) - 1
					rows = slices.Insert(rows, blank, WrapStyledLine(
						StyledLine("  "+sanitizeTerminalText(notice), Muted()),
						width,
						2,
					)...)
				}
			}
			lines = rows
		}
		if collapse.Kind == client.EntryCollapseSummary {
			cache.collapse[index] = client.EntryCollapse{
				Kind:        client.EntryCollapseSummary,
				End:         collapse.End,
				Fingerprint: client.CollapseRevisionFingerprint(entries[index:collapse.End]),
			}
		}
		cache.entries[index] = &CachedEntry{
			fingerprint: fingerprint,
			lines:       lines,
			mailbox:     mailbox,
		}
	}
	return cache.entries[index].lines
}

func defaultTranscriptRenderCache() TranscriptRenderCache {
	return TranscriptRenderCache{
		theme:                 Current(),
		ascii:                 ASCII(),
		mode:                  client.TranscriptRenderModeRich,
		tools:                 DefaultToolDisplay(),
		expandedMailboxEvents: map[MailboxDisclosureId]struct{}{},
		hydratedToolDetails:   map[string]hydratedToolDetailState{},
	}
}

// toolDetail is the hydrated detail of a trimmed tool entry at its current
// revision, or nil.
func (c *TranscriptRenderCache) toolDetail(entry *core.ChatEntry) HydratedToolDetail {
	if !entry.ToolDetailTrimmed || entry.StableID == nil {
		return nil
	}
	revision, ok := entryToolDetailRevision(entry)
	if !ok {
		return nil
	}
	state, ok := c.hydratedToolDetails[*entry.StableID]
	if !ok || state.revision != revision {
		return nil
	}
	return state.detail
}

func (c *TranscriptRenderCache) detailEntry(entry *core.ChatEntry, collapse client.EntryCollapse) *core.ChatEntry {
	if c.mode == client.TranscriptRenderModeRaw || collapse.Kind == client.EntryCollapseExpanded {
		if full, ok := c.toolDetail(entry).(HydratedToolDetailFull); ok {
			return full.Entry
		}
	}
	return entry
}

// clear drops every cached row. The cache is indexed by entry position, so
// any change that moves entries between positions has to clear it: a cached
// row whose revision happens to match the entry that moved into its slot
// would otherwise be served for the wrong entry. Rows are re-rendered lazily,
// so only the visible window pays for the refill.
func (c *TranscriptRenderCache) clear() {
	c.entries = nil
	c.collapse = nil
	c.collapseInputFingerprint = 0
}

func (s *ChatState) resetToolDetails() {
	s.toolDetailGeneration++
	s.pendingToolDetailRequests = nil
	clear(s.renderCache.hydratedToolDetails)
	clear(s.expandedToolCalls)
	s.invalidateRenderCache()
}

func (s *ChatState) queueToolDetail(index int, retry bool) {
	entry := &s.entries[index]
	if !entry.ToolDetailTrimmed {
		return
	}
	revision, ok := entryToolDetailRevision(entry)
	if entry.StableID == nil || !ok {
		return
	}
	if state := s.renderCache.toolDetail(entry); state != nil {
		if _, unavailable := state.(HydratedToolDetailUnavailable); !retry || !unavailable {
			return
		}
	}
	if s.renderCache.hydratedToolDetails == nil {
		s.renderCache.hydratedToolDetails = map[string]hydratedToolDetailState{}
	}
	s.renderCache.hydratedToolDetails[*entry.StableID] = hydratedToolDetailState{
		revision: revision,
		detail:   HydratedToolDetailLoading{},
	}
	s.pendingToolDetailRequests = append(s.pendingToolDetailRequests, *entry)
	s.invalidateToolDetailRows(index)
}

func (s *ChatState) refreshExpandedToolDetails() {
	if len(s.expandedToolCalls) == 0 {
		return
	}
	for index := range s.entries {
		if _, ok := s.expandedToolCalls[s.entries[index].StartSeq]; ok {
			s.queueToolDetail(index, false)
		}
	}
}

// applyToolDetail installs a hydrated tool body: full on success, or err's
// message as why it is unavailable (Rust's Result<Box<ChatEntry>, String>).
func (s *ChatState) applyToolDetail(generation uint64, stableID string, revision ToolDetailRevision, full *core.ChatEntry, err error) {
	if generation != s.toolDetailGeneration {
		return
	}
	index := slices.IndexFunc(s.entries, func(entry core.ChatEntry) bool {
		if entry.StableID == nil || *entry.StableID != stableID {
			return false
		}
		entryRevision, ok := entryToolDetailRevision(&entry)
		return ok && entryRevision == revision
	})
	if index < 0 {
		return
	}
	if _, loading := s.renderCache.toolDetail(&s.entries[index]).(HydratedToolDetailLoading); !loading {
		return
	}
	var state HydratedToolDetail
	if err != nil {
		state = HydratedToolDetailUnavailable{Reason: err.Error()}
	} else {
		state = HydratedToolDetailFull{Entry: full}
	}
	s.invalidateToolDetailRows(index)
	s.renderCache.hydratedToolDetails[stableID] = hydratedToolDetailState{revision: revision, detail: state}
}

func (s *ChatState) invalidateToolDetailRows(index int) {
	if index < len(s.renderCache.entries) {
		s.renderCache.entries[index] = nil
	}
	s.renderCacheGeneration++
}

func (s *ChatState) queueVisibleRawToolDetails(top AnchorRow, visibleRows int) {
	remaining := visibleRows
	skip := top.row
	var requests []int
	for index := top.entry; index < len(s.entries); index++ {
		if remaining == 0 {
			break
		}
		rows := cachedEntryLines(s.entries, &s.renderCache, index)
		visible := max(len(rows)-skip, 0)
		if visible > 0 && s.entries[index].ToolDetailTrimmed {
			requests = append(requests, index)
		}
		remaining = max(remaining-visible, 0)
		skip = 0
	}
	for _, index := range requests {
		s.queueToolDetail(index, false)
	}
}

// TranscriptPosition is where the reader is, independent of the tail-first
// loading window.
func (s *ChatState) TranscriptPosition() TranscriptPosition {
	switch anchor := s.anchor.(type) {
	case TranscriptAnchorRestoring:
		return anchor.Position
	case TranscriptAnchorRow:
		if anchor.Entry >= len(s.entries) {
			return TranscriptPosition{anchor: SavedTranscriptAnchor{Kind: SavedTranscriptAnchorBottom}}
		}
		entry := &s.entries[anchor.Entry]
		saved := SavedTranscriptAnchor{
			Kind:     SavedTranscriptAnchorEntry,
			StartSeq: entry.StartSeq,
			Row:      anchor.Row,
		}
		if entry.Role == core.ChatRoleMessage {
			if entry.MessageID == nil {
				panic("message identity")
			}
			saved.StableID, saved.HasStableID = *entry.MessageID, true
		}
		if s.revealedAnchor != nil && s.restsOnOpeningReveal() {
			saved.RevealedThrough, saved.HasRevealedThrough = s.revealedAnchor.newestStartSeq, true
		}
		return TranscriptPosition{anchor: saved}
	default:
		return TranscriptPosition{anchor: SavedTranscriptAnchor{Kind: SavedTranscriptAnchorBottom}}
	}
}

func (s *ChatState) transcriptMessageEntry(stableID string) (int, bool) {
	index := slices.IndexFunc(s.entries, func(entry core.ChatEntry) bool {
		return entry.MessageID != nil && *entry.MessageID == stableID
	})
	return index, index >= 0
}

func (s *ChatState) RestoreTranscriptPosition(position TranscriptPosition) {
	// Restoration owns the viewport; a fresh opening reveal would move an
	// explicitly followed tail back to the start of the latest reply.
	s.revealLatestAgentOnDraw = false
	s.revealedAnchor = nil
	s.anchor = TranscriptAnchorRestoring{Position: position}
	s.resolveTranscriptPosition()
}

func (s *ChatState) resolveTranscriptPosition() {
	restoring, ok := s.anchor.(TranscriptAnchorRestoring)
	if !ok {
		return
	}
	position := restoring.Position.anchor
	var anchor TranscriptAnchor
	switch position.Kind {
	case SavedTranscriptAnchorBottom:
		anchor = TranscriptAnchorBottom{}
	case SavedTranscriptAnchorRow:
		if position.Entry >= s.unconvertedPrefix {
			if entry := position.Entry - s.unconvertedPrefix; entry < len(s.entries) {
				anchor = TranscriptAnchorRow{Entry: entry, Row: position.Row}
			}
		}
	case SavedTranscriptAnchorEntry:
		var index int
		var found bool
		if position.HasStableID {
			index, found = s.transcriptMessageEntry(position.StableID)
		} else {
			index = slices.IndexFunc(s.entries, func(entry core.ChatEntry) bool {
				return entry.StartSeq == position.StartSeq
			})
			found = index >= 0
		}
		if found {
			anchor = TranscriptAnchorRow{Entry: index, Row: position.Row}
		}
	}
	if anchor != nil {
		s.anchor = anchor
		if position.Kind == SavedTranscriptAnchorEntry && position.HasRevealedThrough {
			s.revealedAnchor = &revealedAnchor{
				replyStartSeq:  position.StartSeq,
				newestStartSeq: position.RevealedThrough,
			}
		}
	} else if s.unconvertedPrefix == 0 && !s.transcriptLoading {
		// The complete projection no longer contains the saved entry.
		s.anchor = TranscriptAnchorBottom{}
	}
}

func (s *ChatState) cancelTranscriptPositionRestore() {
	if _, ok := s.anchor.(TranscriptAnchorRestoring); ok {
		s.anchor = TranscriptAnchorBottom{}
	}
}

func (s *ChatState) cancelTranscriptScrollbarPointer() {
	s.transcriptScrollbar.pointer.CancelPointer()
}

// TranscriptScrollbarDragging is whether the host must keep routing
// left-button motion to this chat. The pointer may leave the pane while a
// thumb is held.
func (s *ChatState) TranscriptScrollbarDragging() bool {
	return s.transcriptScrollbar.pointer.IsDragging() && !s.transcriptScrollbarCovered()
}

func (s *ChatState) transcriptScrollbarCovered() bool {
	return s.elicitation == nil && s.configPickerActive()
}

// handleTranscriptScrollbarMouse handles the primary transcript's one-cell
// scrollbar. A drag is kept alive outside the transcript rectangle; the
// dashboard uses TranscriptScrollbarDragging to route those events here.
func (s *ChatState) handleTranscriptScrollbarMouse(mouse tea.MouseMsg) bool {
	if s.transcriptScrollbarCovered() {
		wasDragging := s.transcriptScrollbar.pointer.IsDragging()
		s.transcriptScrollbar.clearGeometry()
		return wasDragging
	}
	if entry, ok := s.transcriptScrollbarMarkAt(mouse); ok {
		s.setAnchor(TranscriptAnchorRow{Entry: entry, Row: 0})
		return true
	}
	switch pointer := s.transcriptScrollbar.pointer.HandleMouse(mouse).(type) {
	case ScrollbarPointerConsumed:
		return true
	case ScrollbarPointerScrollTo:
		s.setTranscriptScrollbarTarget(pointer.Offset)
		return true
	default:
		return false
	}
}

// transcriptScrollbarMarkAt is the message a left click on a user-message
// mark opens. A click starts no drag, so a held mark does not seek the track.
func (s *ChatState) transcriptScrollbarMarkAt(mouse tea.MouseMsg) (int, bool) {
	state := &s.transcriptScrollbar
	click, isClick := mouse.(tea.MouseClickMsg)
	if state.pointer.IsDragging() || !isClick || click.Button != tea.MouseLeft {
		return 0, false
	}
	geometry, ok := state.pointer.Geometry()
	if !ok {
		return 0, false
	}
	position := mouse.Mouse()
	if position.X != geometry.Track.Min.X {
		return 0, false
	}
	for _, mark := range state.marks {
		if mark.trackRow == position.Y {
			return mark.entry, true
		}
	}
	return 0, false
}

func (s *ChatState) setTranscriptScrollbarTarget(targetScroll int) {
	geometry, ok := s.transcriptScrollbar.pointer.Geometry()
	if !ok {
		return
	}
	if targetScroll >= geometry.MaxScroll {
		s.setAnchor(TranscriptAnchorBottom{})
		return
	}
	if len(s.entries) == 0 {
		s.setAnchor(TranscriptAnchorBottom{})
		return
	}
	remaining := targetScroll
	for entry := 0; entry < len(s.transcriptScrollbar.estimates); entry++ {
		estimated := s.transcriptScrollbar.estimates[entry]
		if remaining < estimated {
			actual := s.entryRows(entry)
			s.setAnchor(TranscriptAnchorRow{
				Entry: entry,
				Row:   remaining * actual / estimated,
			})
			return
		}
		remaining = max(remaining-estimated, 0)
	}
	s.setAnchor(TranscriptAnchorBottom{})
}

func (s *ChatState) prepareTranscriptScrollbarMetrics(width int) {
	state := &s.transcriptScrollbar
	keyChanged := state.metricWidth != width ||
		state.metricMode != s.renderMode ||
		state.metricGeneration != s.renderCacheGeneration
	if keyChanged {
		state.clearGeometry()
	}
	if state.pointer.IsDragging() {
		// Newly rendered entries must not move the thumb under a held pointer.
		return
	}
	if keyChanged || state.textEstimateTools == nil || *state.textEstimateTools != s.toolDisplay {
		state.textEstimates = nil
		tools := s.toolDisplay
		state.textEstimateTools = &tools
	}
	state.metricWidth = width
	state.metricMode = s.renderMode
	state.metricGeneration = s.renderCacheGeneration
	if len(state.textEstimates) >= len(s.entries) {
		state.textEstimates = state.textEstimates[:len(s.entries)]
	} else {
		state.textEstimates = append(state.textEstimates, make([]transcriptTextEstimate, len(s.entries)-len(state.textEstimates))...)
	}
	state.estimates = state.estimates[:0]
	toolView := s.toolDisplay.view()
	for index := range s.entries {
		entry := &s.entries[index]
		var rows int
		collapse := s.renderCache.collapse[index]
		if collapse.Kind == client.EntryCollapseHidden || collapse.Kind == client.EntryCollapseOmitted {
			rows = 0
		} else if index < len(s.renderCache.entries) &&
			s.renderCache.entries[index] != nil &&
			s.renderCache.entries[index].fingerprint == client.EntryRenderFingerprint(entry) {
			rows = len(s.renderCache.entries[index].lines)
		} else if estimate := state.textEstimates[index]; estimate.known && estimate.fingerprint == client.EntryRenderFingerprint(entry) {
			rows = estimate.rows
		} else {
			rows = estimatedEntryRows(entry, width, s.renderMode, toolView)
			state.textEstimates[index] = transcriptTextEstimate{
				known:       true,
				fingerprint: client.EntryRenderFingerprint(entry),
				rows:        rows,
			}
		}
		state.estimates = append(state.estimates, rows)
	}
	row := 0
	state.userMessages = state.userMessages[:0]
	for entry, estimate := range state.estimates {
		if estimate > 0 && s.entries[entry].IsUserAuthored() {
			state.userMessages = append(state.userMessages, UserMessageRow{entry: entry, row: row})
		}
		row = transcriptSaturatingAdd(row, estimate)
	}
}

func (s *ChatState) transcriptScrollbarOffset(top AnchorRow, anchor TranscriptAnchor) int {
	if anchorIsBottom(anchor) {
		return math.MaxInt
	}
	estimated := 0
	if top.entry < len(s.transcriptScrollbar.estimates) {
		estimated = s.transcriptScrollbar.estimates[top.entry]
	}
	actual := estimated
	if top.entry < len(s.renderCache.entries) && s.renderCache.entries[top.entry] != nil {
		actual = len(s.renderCache.entries[top.entry].lines)
	}
	above := 0
	for _, rows := range s.transcriptScrollbar.estimates[:min(top.entry, len(s.transcriptScrollbar.estimates))] {
		above += rows
	}
	return transcriptSaturatingAdd(above, top.row*estimated/max(actual, 1))
}

func (s *ChatState) updateTranscriptScrollbar(track uv.Rectangle, width int, viewportRows int, top AnchorRow, anchor TranscriptAnchor) (ScrollbarGeometry, bool) {
	if track.Dx() == 0 || track.Dy() == 0 || s.transcriptScrollbarCovered() {
		s.transcriptScrollbar.clearGeometry()
		return ScrollbarGeometry{}, false
	}
	if geometry, ok := s.transcriptScrollbar.pointer.Geometry(); !ok || geometry.Track != track {
		s.transcriptScrollbar.pointer.SetGeometry(ScrollbarGeometry{}, false)
	}
	s.prepareTranscriptScrollbarMetrics(width)
	viewportRows = max(viewportRows, 1)
	totalRows := 0
	for _, rows := range s.transcriptScrollbar.estimates {
		totalRows += rows
	}
	totalRows = max(totalRows, viewportRows)
	maxScroll := max(totalRows-viewportRows, 0)
	rawScroll := s.transcriptScrollbarOffset(top, anchor)
	scroll := min(rawScroll, maxScroll)
	if rawScroll == math.MaxInt {
		scroll = maxScroll
	}
	geometry, ok := NewScrollbarGeometry(track, totalRows, scroll, viewportRows)
	if !ok {
		return ScrollbarGeometry{}, false
	}
	state := &s.transcriptScrollbar
	state.pointer.SetGeometry(geometry, true)
	state.marks = userMessageMarks(geometry, totalRows, state.userMessages)
	return geometry, true
}

func (s *ChatState) TranscriptSnapshot() *TranscriptSnapshot {
	return &TranscriptSnapshot{
		entries:           slices.Clone(s.entries),
		latestSeq:         s.latestSeq,
		lastCompactionSeq: s.lastCompactionSeq,
		renderCache:       defaultTranscriptRenderCache(),
	}
}

// invalidateRenderCache drops the cached rows, for a change that moved
// entries between positions rather than editing one in place.
func (s *ChatState) invalidateRenderCache() {
	s.renderCache.clear()
	s.renderCacheGeneration++
}

// Rust's ChatState::unconverted_prefix() getter (how many leading transcript
// items this view has not converted yet; zero once the conversation is
// complete) is not declared: in Go it would collide with the
// unconvertedPrefix field, which callers read directly.

// spliceTranscriptPrefix puts the history built off the event loop in front
// of the tail this view opened with. Reports whether the prefix still fits:
// the transcript can be rewritten by compaction while the conversion runs,
// and a prefix that no longer meets the tail is refused rather than spliced
// into the wrong place.
//
// Fitting is an identity question, not a counting one. A rewrite that leaves
// the transcript at or above the pending prefix's length keeps every count
// valid, so the seam decides: the prefix's last entry has to be the
// projection item that still sits immediately in front of the tail. Anything
// else is history the projection has replaced.
func (s *ChatState) spliceTranscriptPrefix(prefix []core.ChatEntry) bool {
	if s.unconvertedPrefix == 0 || len(prefix) != s.unconvertedPrefix {
		return false
	}
	meetsTheTail := false
	if len(prefix) > 0 && s.prefixSeam != nil {
		// Pointer identity settles the common case; the field comparison
		// covers items a restore from the canonical log rebuilt with equal
		// content.
		last := &prefix[len(prefix)-1]
		meetsTheTail = last.Source.Is(s.prefixSeam) || client.EntryMatchesTranscriptItem(last, s.prefixSeam)
	}
	if !meetsTheTail {
		return false
	}
	// The prefix was converted against the frontier the view opened at, so
	// its update cursors are restamped against the current one.
	for index := range prefix {
		entry := &prefix[index]
		if item := entry.Source.Item; item != nil {
			entry.Seq = client.ItemUpdateOrdinal(item, s.latestSeq)
		} else {
			entry.Seq = max(s.latestSeq, entry.StartSeq)
		}
	}
	shift := len(prefix)
	s.entries = append(prefix, s.entries...)
	client.ApplyEntryListRules(s.entries, s.pendingElicitations)
	s.unconvertedPrefix = 0
	s.prefixSeam = nil
	if row, ok := s.anchor.(TranscriptAnchorRow); ok {
		s.anchor = TranscriptAnchorRow{Entry: transcriptSaturatingAdd(row.Entry, shift), Row: row.Row}
	}
	if s.hasInlineSubmissions() {
		s.syncInlineSubmissions()
	}
	s.invalidateRenderCache()
	return true
}

// restsOnOpeningReveal is whether the view still rests where the opening
// reveal put it, which the reader did not choose: no scroll has moved it
// since.
func (s *ChatState) restsOnOpeningReveal() bool {
	if s.revealedAnchor == nil {
		return false
	}
	row, ok := s.anchor.(TranscriptAnchorRow)
	if !ok || row.Row != 0 {
		return false
	}
	return row.Entry < len(s.entries) && s.entries[row.Entry].StartSeq == s.revealedAnchor.replyStartSeq
}

func (s *ChatState) viewport(width int, height int) TranscriptViewport {
	s.resolveTranscriptPosition()
	if _, restoring := s.anchor.(TranscriptAnchorRestoring); restoring {
		return TranscriptViewport{
			rows:   []Line{emptyTranscriptRow(true)},
			anchor: s.anchor,
			top:    AnchorRow{},
		}
	}
	trailing := s.trailingEntries()
	prepareRenderCache(
		s.entries,
		&s.renderCache,
		width,
		s.renderMode,
		s.toolDisplay,
		s.expandedToolCalls,
		s.expandedMailboxEvents,
	)
	top := AnchorRow{}
	if len(s.entries) == 0 && len(trailing) == 0 {
		return TranscriptViewport{
			rows:   []Line{emptyTranscriptRow(s.transcriptLoading)},
			anchor: TranscriptAnchorBottom{},
			top:    top,
		}
	}
	// The opening reveal parks the view on the latest reply, but the reader
	// did not scroll there. Once something newer arrives, follow the tail
	// again. Explicit reader navigation clears this intent immediately, even
	// when it selects the same row.
	if s.revealedAnchor != nil {
		last := s.revealedAnchor.newestStartSeq
		if !s.restsOnOpeningReveal() {
			s.revealedAnchor = nil
		} else if len(s.entries) == 0 || s.entries[len(s.entries)-1].StartSeq != last {
			s.anchor = TranscriptAnchorBottom{}
			s.revealedAnchor = nil
		}
	}
	if s.revealLatestAgentOnDraw && s.unconvertedPrefix == 0 && anchorIsBottom(s.anchor) {
		s.revealLatestAgentOnDraw = false
		tail := s.tailViewport(height)
		latestAgent := -1
		for index := len(s.entries) - 1; index >= 0; index-- {
			if s.entries[index].Role == core.ChatRoleAgent && strings.TrimSpace(s.entries[index].Text) != "" {
				latestAgent = index
				break
			}
		}
		agentIsAboveTail := latestAgent >= 0 &&
			(latestAgent < tail.top.entry || latestAgent == tail.top.entry && tail.top.row > 0)
		if !agentIsAboveTail {
			return tail
		}
		s.anchor = TranscriptAnchorRow{Entry: latestAgent, Row: 0}
		if len(s.entries) > 0 {
			s.revealedAnchor = &revealedAnchor{
				replyStartSeq:  s.entries[latestAgent].StartSeq,
				newestStartSeq: s.entries[len(s.entries)-1].StartSeq,
			}
		}
	}
	if anchor, ok := s.anchor.(TranscriptAnchorRow); ok && anchor.Entry < len(s.entries) {
		rows := make([]Line, 0, height)
		skip := anchor.Row
		for index := anchor.Entry; index < len(s.entries); index++ {
			lines := cachedEntryLines(s.entries, &s.renderCache, index)
			for _, line := range lines[min(skip, len(lines)):] {
				if len(rows) == height {
					break
				}
				rows = append(rows, line)
			}
			skip = 0
			if len(rows) == height {
				break
			}
		}
		for index := range trailing {
			for _, line := range s.renderTrailingEntry(&trailing[index], width) {
				if len(rows) == height {
					break
				}
				rows = append(rows, line)
			}
		}
		// Anchors inside the final screenful cannot fill the viewport; those
		// views already reach the newest row, so follow the tail instead of
		// painting a short page.
		if len(rows) == height {
			top := AnchorRow{entry: anchor.Entry, row: anchor.Row}
			return TranscriptViewport{
				rows:   rows,
				anchor: top.anchor(),
				top:    top,
			}
		}
	}
	return s.tailViewport(height)
}

// tailViewport is the last height rows, walking backwards from the newest
// entry. These rows always reach the newest row, so the anchor to store is
// Bottom.
func (s *ChatState) tailViewport(height int) TranscriptViewport {
	top := AnchorRow{}
	if height == 0 {
		return TranscriptViewport{
			rows:   []Line{},
			anchor: TranscriptAnchorBottom{},
			top:    top,
		}
	}
	var trailing []Line
	trailingEntries := s.trailingEntries()
	for index := range trailingEntries {
		trailing = append(trailing, s.renderTrailingEntry(&trailingEntries[index], s.renderCache.width)...)
	}
	// reversed holds the rows newest first; it is reversed at the end, as
	// Rust pushes each row onto the front of a deque.
	reversed := make([]Line, 0, height)
	start := max(len(trailing)-height, 0)
	for index := len(trailing) - 1; index >= start; index-- {
		reversed = append(reversed, trailing[index])
	}
	for index := len(s.entries) - 1; index >= 0; index-- {
		if len(reversed) >= height {
			break
		}
		lines := cachedEntryLines(s.entries, &s.renderCache, index)
		take := max(height-len(reversed), 0)
		start := max(len(lines)-take, 0)
		for row := len(lines) - 1; row >= start; row-- {
			reversed = append(reversed, lines[row])
		}
		top = AnchorRow{entry: index, row: start}
		if len(reversed) >= height {
			break
		}
	}
	slices.Reverse(reversed)
	return TranscriptViewport{
		rows:   reversed,
		anchor: TranscriptAnchorBottom{},
		top:    top,
	}
}

func (s *ChatState) renderTrailingEntry(entry *core.ChatEntry, width int) []Line {
	rows := renderTranscriptEntry(entry, width, s.renderMode)
	if status, ok := s.submissionStatus(entry); ok {
		rows = append(rows, WrapStyledLine(
			Line{Spans: []Span{{Content: "  " + core.SanitizeTerminalText(status), Style: Muted()}}},
			width,
			2,
		)...)
	}
	return rows
}

// trailingEntries: local activity, submissions, and unpublished notices
// survive rebuilds of the authoritative projection and yield when their
// durable rows arrive.
func (s *ChatState) trailingEntries() []core.ChatEntry {
	var rows []core.ChatEntry
	if fallback, ok := s.activeTerminalFallback(); ok {
		rows = append(rows, fallback)
	}
	rows = append(rows, s.submissionEntries()...)
	rows = append(rows, s.noticeEntries()...)
	return rows
}

// activeTerminalFallback is a live, non-durable tool card for ACP terminals
// whose agent omitted the matching tool-call update. It disappears on exit
// and yields immediately when a real transcript tool claims an active
// terminal.
func (s *ChatState) activeTerminalFallback() (core.ChatEntry, bool) {
	var unclaimed []*core.ActiveAgentTerminal
	for index := range s.activeAgentTerminals {
		terminal := &s.activeAgentTerminals[index]
		if claimedAtMs, claimed := s.claimedAgentTerminals[terminal.TerminalID]; !claimed || claimedAtMs < terminal.StartedAtMs {
			unclaimed = append(unclaimed, terminal)
		}
	}
	sort.SliceStable(unclaimed, func(i, j int) bool {
		return unclaimed[i].StartedAtMs < unclaimed[j].StartedAtMs
	})
	if len(unclaimed) == 0 {
		return core.ChatEntry{}, false
	}
	oldest := unclaimed[0]
	var startedAt *uint64
	if seconds := oldest.StartedAtMs / 1_000; seconds >= 0 {
		value := uint64(seconds)
		startedAt = &value
	}
	elapsed := client.FormatTurnClock(core.EpochSeconds(), startedAt)
	command := compactTerminalCommand(oldest.Command)
	var text string
	if len(unclaimed) == 1 {
		text = fmt.Sprintf("%s\nRunning · %s", command, elapsed)
	} else {
		text = fmt.Sprintf("%d shell commands active\nOldest: %s\nRunning · %s", len(unclaimed), command, elapsed)
	}
	return core.ChatEntryTool(s.latestSeq, text, nil, core.ToolStatusRunning), true
}

// prepareEntryRows brings the row cache in line with the current entries
// before a scroll traversal. A collapsed run can cross hundreds of entries,
// so doing the whole collapse pass again for every zero-row member would be
// quadratic.
func (s *ChatState) prepareEntryRows() {
	prepareRenderCache(
		s.entries,
		&s.renderCache,
		s.renderCache.width,
		s.renderMode,
		s.toolDisplay,
		s.expandedToolCalls,
		s.expandedMailboxEvents,
	)
}

// entryRows is the rendered row count for one entry after prepareEntryRows.
func (s *ChatState) entryRows(index int) int {
	return len(cachedEntryLines(s.entries, &s.renderCache, index))
}

// resolvedAnchor is the anchor the current view is showing, resolving Bottom
// into the concrete entry and row at the top of the viewport. Nil before the
// first draw, when no width is known to wrap against.
func (s *ChatState) resolvedAnchor() TranscriptAnchor {
	// An empty transcript has no rows to anchor to, and its render cache has
	// no entry to read.
	if s.renderCache.width == 0 || len(s.entries) == 0 {
		return nil
	}
	switch anchor := s.anchor.(type) {
	case TranscriptAnchorRestoring:
		return nil
	case TranscriptAnchorRow:
		if anchor.Entry < len(s.entries) {
			return anchor
		}
	}
	return s.tailViewport(max(s.lastViewportHeight, 1)).top.anchor()
}

// jumpToUserMessage moves the view to the previous or next message the user
// wrote, so that message starts at the top. Prompts Hel generated and entries
// the Rich mode hides are skipped. Past the newest message the view follows
// the tail again; before the oldest it stays where it is.
func (s *ChatState) jumpToUserMessage(forward bool) bool {
	before := s.anchor
	current, ok := s.resolvedAnchor().(TranscriptAnchorRow)
	if !ok {
		return false
	}
	s.prepareEntryRows()
	isStop := func(index int) bool {
		if !s.entries[index].IsUserAuthored() {
			return false
		}
		if index < len(s.renderCache.collapse) {
			kind := s.renderCache.collapse[index].Kind
			return kind != client.EntryCollapseHidden && kind != client.EntryCollapseOmitted
		}
		return true
	}
	target := -1
	if forward {
		for index := current.Entry + 1; index < len(s.entries); index++ {
			if isStop(index) {
				target = index
				break
			}
		}
	} else {
		// A message scrolled partly off the top is the first stop back.
		limit := current.Entry
		if current.Row > 0 {
			limit = current.Entry + 1
		}
		for index := limit - 1; index >= 0; index-- {
			if isStop(index) {
				target = index
				break
			}
		}
	}
	var anchor TranscriptAnchor
	switch {
	case target >= 0:
		anchor = TranscriptAnchorRow{Entry: target, Row: 0}
	case forward:
		anchor = TranscriptAnchorBottom{}
	default:
		anchor = before
	}
	s.setAnchor(anchor)
	return s.anchor != before
}

func (s *ChatState) scrollHistoryUp(rows int) bool {
	before := s.anchor
	current, ok := s.resolvedAnchor().(TranscriptAnchorRow)
	if !ok {
		// Either no draw has happened yet, or the transcript is shorter than
		// the viewport and has nothing above it.
		return false
	}
	s.prepareEntryRows()
	entry, row := current.Entry, current.Row
	remaining := rows
	for remaining > 0 {
		if row > 0 {
			step := min(remaining, row)
			row -= step
			remaining -= step
		} else if entry > 0 {
			entry--
			row = s.entryRows(entry)
		} else {
			break
		}
	}
	s.setAnchor(TranscriptAnchorRow{Entry: entry, Row: row})
	return s.anchor != before
}

func (s *ChatState) scrollHistoryDown(rows int) bool {
	before := s.anchor
	current, ok := s.resolvedAnchor().(TranscriptAnchorRow)
	if !ok {
		return false
	}
	s.prepareEntryRows()
	entry, row := current.Entry, current.Row
	remaining := rows
	for remaining > 0 {
		entryRows := s.entryRows(entry)
		if entryRows == 0 || row >= entryRows {
			// Rich rendering collapses a tool run into one summary entry and
			// gives its other entries zero rows. They are not scroll distance:
			// upward traversal already skips them, and charging for them here
			// can strand the viewport in a long tool run.
			if entry+1 >= len(s.entries) {
				s.setAnchor(TranscriptAnchorBottom{})
				return s.anchor != before
			}
			entry++
			row = 0
			continue
		}
		below := entryRows - row - 1
		if below >= remaining {
			row += remaining
			break
		}
		if entry+1 >= len(s.entries) {
			s.setAnchor(TranscriptAnchorBottom{})
			return s.anchor != before
		}
		remaining -= below + 1
		entry++
		row = 0
	}
	position := AnchorRow{entry: entry, row: row}
	// A scroll step can land exactly on the first row of the final
	// screenful. That page fills the viewport, but it is still the live tail
	// and must resume following new output. Resolve this on input so ordinary
	// render frames do not have to scan the tail twice.
	var anchor TranscriptAnchor
	if s.tailViewport(max(s.lastViewportHeight, 1)).top == position {
		anchor = TranscriptAnchorBottom{}
	} else {
		anchor = position.anchor()
	}
	s.setAnchor(anchor)
	return s.anchor != before
}

// transcriptRowLayout is what the current rendered rows depend on wholesale.
func (s *ChatState) transcriptRowLayout() TranscriptRowLayout {
	return TranscriptRowLayout{
		width:      s.renderCache.width,
		mode:       s.renderMode,
		generation: s.renderCacheGeneration,
	}
}

// registerTranscriptSurface registers the transcript's selectable surface
// for this frame.
//
// gestureActive says whether the engine still owns a transcript selection.
// While it does the base anchor stays frozen and this frame's top is reported
// as an offset walked from the previous frame's; when it does not, the base
// is re-pinned here so the next gesture starts from a space that matches the
// screen.
func (s *ChatState) registerTranscriptSurface(inner uv.Rectangle, top AnchorRow, visibleRows int, atTail bool, gestureActive bool) {
	layout := s.transcriptRowLayout()
	var frozen *TranscriptSelectionSpace
	if space := s.transcriptSelection; space != nil &&
		gestureActive && space.layout == layout && space.base.entry < len(s.entries) {
		copied := *space
		frozen = &copied
	}
	if gestureActive && frozen == nil && s.transcriptSelection != nil {
		s.transcriptSelectionInvalid = true
	}
	var topRow int
	if frozen != nil {
		if delta, ok := s.rowsBetween(frozen.prevTop, top); ok {
			frozen.offsetRows = saturatingAddInt64(frozen.offsetRows, delta)
			frozen.prevTop = top
			s.transcriptSelection = frozen
			topRow = selectionRow(frozen.offsetRows)
		} else {
			s.transcriptSelectionInvalid = true
			topRow = s.pinTranscriptSelection(top, layout)
		}
	} else {
		topRow = s.pinTranscriptSelection(top, layout)
	}
	// A viewport that reaches the newest row reports its exact end, so a drag
	// held below it stops extending; anything above the tail claims room
	// beyond the screen, which is what keeps auto-scroll armed.
	totalRows := transcriptSaturatingAdd(topRow, visibleRows)
	if !atTail {
		totalRows = transcriptSaturatingAdd(totalRows, selectionScrollMarginRows)
	}
	s.frameSurfaces.Push(SurfaceFrameScrollable(
		SurfaceIdTranscript{},
		inner,
		topRow,
		totalRows,
	))
}

// rebuildTranscriptClickTargets rebuilds frame-local expansion targets from
// the same cached rows that were just painted. Keeping these targets
// transient avoids stale coordinates after wrapping, scrolling, or a collapse
// change.
func (s *ChatState) rebuildTranscriptClickTargets(inner uv.Rectangle, top AnchorRow, visibleRows int) {
	s.transcriptClickTargets = s.transcriptClickTargets[:0]
	if visibleRows == 0 || inner.Dx() == 0 || top.entry >= len(s.entries) {
		return
	}
	if s.renderMode == client.TranscriptRenderModeRaw {
		return
	}

	width := inner.Dx()
	screenOffset := 0
	skip := top.row
	entries := s.entries
	cache := &s.renderCache
	targets := &s.transcriptClickTargets
	for index := top.entry; index < len(entries); index++ {
		if screenOffset >= visibleRows {
			break
		}
		collapse := cache.collapse[index]
		cachedEntryLines(entries, cache, index)
		cached := cache.entries[index]
		lines := cached.lines
		entryStart := screenOffset
		visibleLineCount := min(max(len(lines)-skip, 0), max(visibleRows-screenOffset, 0))
		if visibleLineCount == 0 {
			skip = 0
			continue
		}
		if cached.mailbox != nil {
			for _, event := range cached.mailbox.MailboxEvents {
				start := max(event.Start, skip)
				end := min(event.End, skip+visibleLineCount)
				if start >= end {
					continue
				}
				*targets = append(*targets, TranscriptClickTarget{
					Rect: uv.Rect(
						inner.Min.X,
						inner.Min.Y+entryStart+start-skip,
						inner.Dx(),
						end-start,
					),
					StartSeq:     entries[index].StartSeq,
					MailboxEvent: event.ID,
				})
			}
		} else {
			switch {
			case collapse.Kind == client.EntryCollapseExpanded:
				addWholeToolRows(targets, inner, entryStart, skip, lines, entries[index].StartSeq)
			case collapse.Kind == client.EntryCollapseNone &&
				(entries[index].Role == core.ChatRoleTool || entries[index].ToolDetailTrimmed):
				addWholeToolRows(targets, inner, entryStart, skip, lines, entries[index].StartSeq)
			case collapse.Kind == client.EntryCollapseSummary:
				addSummaryToolRows(targets, inner, entryStart, skip, width, entries[index:collapse.End], lines)
			}
		}
		screenOffset = transcriptSaturatingAdd(screenOffset, visibleLineCount)
		skip = 0
	}
}

// recordTranscriptLinkFrame remembers where this frame drew the transcript,
// for transcriptLinkAt.
func (s *ChatState) recordTranscriptLinkFrame(inner uv.Rectangle, top AnchorRow, visibleRows int) {
	s.transcriptLinkFrame = &TranscriptLinkFrame{inner: inner, top: top, visibleRows: visibleRows}
}

// findRowLink is the destination of the link drawn at column of row, if any.
func findRowLink(links []RowLink, row int, column int) (string, bool) {
	for _, link := range links {
		if link.Row == row && column >= link.Start && column < link.End {
			return link.URL, true
		}
	}
	return "", false
}

// transcriptLinkAt is the destination of the link drawn at a screen cell of
// the last transcript frame. Only the clicked entry is rendered again, with
// link tracking, so ordinary frames pay nothing for links.
func (s *ChatState) transcriptLinkAt(column int, row int) (string, bool) {
	frame := s.transcriptLinkFrame
	if frame == nil {
		return "", false
	}
	if !uv.Pos(column, row).In(frame.inner) {
		return "", false
	}
	target := row - frame.inner.Min.Y
	if target >= frame.visibleRows {
		return "", false
	}
	column -= frame.inner.Min.X
	screenOffset := 0
	skip := frame.top.row
	for index := frame.top.entry; index < len(s.entries); index++ {
		if index >= len(s.renderCache.collapse) {
			return "", false
		}
		collapse := s.renderCache.collapse[index]
		rows := len(cachedEntryLines(s.entries, &s.renderCache, index))
		visible := max(rows-skip, 0)
		if target < screenOffset+visible {
			entryRow := skip + (target - screenOffset)
			if cached := s.renderCache.entries[index]; cached != nil && cached.mailbox != nil {
				return findRowLink(cached.mailbox.Links, entryRow, column)
			}
			// The view cachedEntryLines drew the entry with.
			var mode client.TranscriptRenderMode
			var tool ToolView
			switch collapse.Kind {
			case client.EntryCollapseNone:
				mode, tool = s.renderCache.mode, s.renderCache.tools.view()
			case client.EntryCollapseExpanded:
				mode, tool = client.TranscriptRenderModeRich, ToolViewExpanded{}
			default:
				// A collapsed streak draws a summary, not entry text.
				return "", false
			}
			track := trackTranscriptEntry(
				s.renderCache.detailEntry(&s.entries[index], collapse),
				s.renderCache.width,
				mode,
				tool,
			)
			return findRowLink(track.Links, entryRow, column)
		}
		screenOffset += visible
		skip = 0
	}
	return "", false
}

// toggleTranscriptAt toggles a tool or mailbox event at a frame-local screen
// coordinate. The selection router calls this only for a synthetic click, so
// ordinary mouse-down events remain owned by drag selection.
func (s *ChatState) toggleTranscriptAt(column int, row int) bool {
	position := uv.Pos(column, row)
	index := slices.IndexFunc(s.transcriptClickTargets, func(target TranscriptClickTarget) bool {
		return position.In(target.Rect)
	})
	if index < 0 {
		return false
	}
	target := s.transcriptClickTargets[index]
	if key := target.MailboxEvent; key != nil {
		if s.expandedMailboxEvents == nil {
			s.expandedMailboxEvents = map[MailboxDisclosureId]struct{}{}
		}
		if _, open := s.expandedMailboxEvents[key]; !open {
			s.expandedMailboxEvents[key] = struct{}{}
		} else {
			delete(s.expandedMailboxEvents, key)
			// A body click can collapse the event containing the viewport
			// top. Keep its header visible instead of skipping later text.
			if anchor, ok := s.anchor.(TranscriptAnchorRow); ok &&
				anchor.Entry < len(s.renderCache.entries) &&
				s.renderCache.entries[anchor.Entry] != nil &&
				s.renderCache.entries[anchor.Entry].mailbox != nil {
				for _, event := range s.renderCache.entries[anchor.Entry].mailbox.MailboxEvents {
					if event.ID != key {
						continue
					}
					if anchor.Row >= event.Start && anchor.Row < event.End {
						s.anchor = TranscriptAnchorRow{Entry: anchor.Entry, Row: event.Start}
					}
					break
				}
			}
		}
	} else if _, open := s.expandedToolCalls[target.StartSeq]; open {
		delete(s.expandedToolCalls, target.StartSeq)
	} else {
		if s.expandedToolCalls == nil {
			s.expandedToolCalls = map[uint64]struct{}{}
		}
		s.expandedToolCalls[target.StartSeq] = struct{}{}
		if index := slices.IndexFunc(s.entries, func(entry core.ChatEntry) bool {
			return entry.StartSeq == target.StartSeq &&
				(entry.Role == core.ChatRoleTool || entry.ToolDetailTrimmed)
		}); index >= 0 {
			s.queueToolDetail(index, true)
		}
	}

	// A tool toggle can replace its whole layout; reset an anchor inside that
	// tool to its header.
	if target.MailboxEvent == nil {
		if anchor, ok := s.anchor.(TranscriptAnchorRow); ok &&
			anchor.Entry < len(s.entries) &&
			s.entries[anchor.Entry].StartSeq == target.StartSeq {
			if index := slices.IndexFunc(s.entries, func(entry core.ChatEntry) bool {
				return entry.StartSeq == target.StartSeq
			}); index >= 0 {
				s.anchor = TranscriptAnchorRow{Entry: index, Row: 0}
			}
		}
	}
	s.invalidateRenderCache()
	s.transcriptSelection = nil
	s.transcriptSelectionInvalid = true
	s.transcriptClickTargets = s.transcriptClickTargets[:0]
	return true
}

// pinTranscriptSelection restarts the row space at the viewport top, and
// reports the content row that top now sits at.
func (s *ChatState) pinTranscriptSelection(top AnchorRow, layout TranscriptRowLayout) int {
	s.transcriptSelection = &TranscriptSelectionSpace{
		base:    top,
		prevTop: top,
		layout:  layout,
	}
	return selectionRow(0)
}

// TranscriptSelectionInvalidated is whether the transcript's selection row
// space stopped describing the rows on screen since the last call, so the
// caller drops the selection.
func (s *ChatState) TranscriptSelectionInvalidated() bool {
	invalid := s.transcriptSelectionInvalid
	s.transcriptSelectionInvalid = false
	return invalid
}

// rowsBetween is the rendered rows from from to to, negative when to sits
// above it.
//
// False when the walk would cross more rows or entries than
// selectionWalkBudget allows: only a drag's own movement belongs in this row
// space, and a jump across the deep past is not worth rendering every entry
// in between to measure.
func (s *ChatState) rowsBetween(from AnchorRow, to AnchorRow) (int64, bool) {
	if from == to {
		return 0, true
	}
	start, end, sign := from, to, int64(1)
	if !from.less(to) {
		start, end, sign = to, from, -1
	}
	if end.entry >= len(s.entries) || end.entry-start.entry > selectionWalkBudget {
		return 0, false
	}
	s.prepareEntryRows()
	var rows int
	if start.entry == end.entry {
		rows = max(end.row-start.row, 0)
	} else {
		rows = max(s.entryRows(start.entry)-start.row, 0)
		for index := start.entry + 1; index < end.entry; index++ {
			rows = transcriptSaturatingAdd(rows, s.entryRows(index))
			if rows > selectionWalkBudget {
				return 0, false
			}
		}
		rows = transcriptSaturatingAdd(rows, end.row)
	}
	if rows > selectionWalkBudget {
		return 0, false
	}
	return sign * int64(rows), true
}

// anchorOffsetBy is the position offset rendered rows from from, clamped to
// the ends of the transcript. False when the walk exceeds the budget.
func (s *ChatState) anchorOffsetBy(from AnchorRow, offset int64) (AnchorRow, bool) {
	if from.entry >= len(s.entries) {
		return AnchorRow{}, false
	}
	if offset > selectionWalkBudget || offset < -selectionWalkBudget {
		return AnchorRow{}, false
	}
	remaining := int(offset)
	if remaining < 0 {
		remaining = -remaining
	}
	s.prepareEntryRows()
	entry, row := from.entry, from.row
	steps := 0
	for remaining > 0 {
		steps++
		if steps > selectionWalkBudget {
			return AnchorRow{}, false
		}
		if offset > 0 {
			rows := s.entryRows(entry)
			available := max(rows-row, 0)
			if available > remaining {
				row += remaining
				remaining = 0
			} else {
				remaining -= available
				if entry+1 >= len(s.entries) {
					row = rows
					break
				}
				entry++
				row = 0
			}
		} else if row > 0 {
			step := min(remaining, row)
			row -= step
			remaining -= step
		} else if entry > 0 {
			entry--
			row = s.entryRows(entry)
		} else {
			break
		}
	}
	return AnchorRow{entry: entry, row: row}, true
}

// transcriptRowsFrom is count rendered rows starting at start, from the
// render cache, with how copying each gives back its text (Rust's
// Vec<(Line, RowCopy)>; the rows carry no selected columns).
//
// Copies come from rendering each entry again with tracking, as
// transcriptLinkAt does.
func (s *ChatState) transcriptRowsFrom(start AnchorRow, count int) ([]CopiedRow, bool) {
	if start.entry >= len(s.entries) || count > selectionWalkBudget {
		return nil, false
	}
	s.prepareEntryRows()
	width := s.renderCache.width
	rows := make([]CopiedRow, 0, count)
	skip := start.row
	for index := start.entry; index < len(s.entries); index++ {
		if index-start.entry > selectionWalkBudget {
			return nil, false
		}
		lines := cachedEntryLines(s.entries, &s.renderCache, index)
		wanted := min(max(len(lines)-skip, 0), count-len(rows))
		if wanted == 0 {
			skip = 0
			continue
		}
		picked := slices.Clone(lines[skip : skip+wanted])
		collapse := s.renderCache.collapse[index]
		var copies []rowCopy
		if cached := s.renderCache.entries[index]; cached != nil && cached.mailbox != nil {
			copies = slices.Clone(cached.mailbox.Copies)
		} else {
			// The view cachedEntryLines drew the entry with. A collapsed
			// streak draws a summary, not entry text, so its rows copy as
			// drawn.
			switch collapse.Kind {
			case client.EntryCollapseNone:
				entry := s.renderCache.detailEntry(&s.entries[index], collapse)
				copies = trackTranscriptEntry(entry, width, s.renderCache.mode, s.renderCache.tools.view()).Copies
			case client.EntryCollapseExpanded:
				entry := s.renderCache.detailEntry(&s.entries[index], collapse)
				copies = trackTranscriptEntry(entry, width, client.TranscriptRenderModeRich, ToolViewExpanded{}).Copies
			}
		}
		for offset, line := range picked {
			var copy rowCopy
			if skip+offset < len(copies) {
				copy = copies[skip+offset]
			}
			rows = append(rows, CopiedRow{Line: line, Copy: copy})
		}
		skip = 0
		if len(rows) == count {
			break
		}
	}
	return rows, true
}

// TranscriptSelectionText is the transcript text a finished selection
// covers.
//
// Rows come from the render cache, so rows scrolled out of the viewport are
// copied without the frame buffer holding them. Only endpoints that cut a row
// mid-column go back through a one-row render, which is what gives the
// engine the cells it needs to slice wide characters.
func (s *ChatState) TranscriptSelectionText(selection *SelectionRange) (string, bool) {
	if s.transcriptSelection == nil {
		return "", false
	}
	space := *s.transcriptSelection
	width := s.renderCache.width
	if width == 0 {
		return "", false
	}
	offset := int64(selection.Start.Row) - selectionBaseRow
	start, ok := s.anchorOffsetBy(space.base, offset)
	if !ok {
		return "", false
	}
	if selection.End.Row < selection.Start.Row {
		return "", false
	}
	count := selection.End.Row - selection.Start.Row + 1
	rows, ok := s.transcriptRowsFrom(start, count)
	if !ok {
		return "", false
	}
	for index := range rows {
		row := transcriptSaturatingAdd(selection.Start.Row, index)
		rows[index].First, rows[index].Last, rows[index].Selected = selection.ColumnsOn(row, width)
	}
	return copiedRowsText(rows, width), true
}

// CopiedRow is one row handed to copiedRowsText: Rust's
// (&Line, RowCopy, Option<(u16, u16)>). Selected says whether First..=Last
// limits the copied cells; false copies the whole row (Rust's None).
type CopiedRow struct {
	Line     Line
	Copy     rowCopy
	Selected bool
	First    int
	Last     int
}

// copiedRowsText is the text of selected rows: each row without its
// decoration cells, and a row that wrapping split from the one above joined
// back onto it rather than starting a new line.
func copiedRowsText(rows []CopiedRow, width int) string {
	var text strings.Builder
	for index, row := range rows {
		if index > 0 {
			if row.Copy.joins != nil {
				text.WriteString(*row.Copy.joins)
			} else {
				text.WriteString("\n")
			}
		}
		first, last := 0, max(width-1, 0)
		if row.Selected {
			first, last = row.First, row.Last
		}
		first = max(first, min(row.Copy.lead, math.MaxUint16))
		if first > last {
			continue
		}
		if first > 0 || last+1 < width {
			text.WriteString(slicedRowText(row.Line, width, first, last))
		} else {
			text.WriteString(rowText(row.Line))
		}
	}
	return text.String()
}

// rowText is one cached transcript row as plain text.
func rowText(line Line) string {
	var text strings.Builder
	for _, span := range line.Spans {
		text.WriteString(span.Content)
	}
	return strings.TrimRightFunc(text.String(), unicode.IsSpace)
}

// targetRow is inner.y.saturating_add(u16::try_from(offset).unwrap_or(u16::MAX)).
func targetRow(inner uv.Rectangle, offset int) int {
	return inner.Min.Y + min(offset, math.MaxUint16)
}

func addWholeToolRows(targets *[]TranscriptClickTarget, inner uv.Rectangle, screenStart int, skip int, lines []Line, startSeq uint64) {
	for lineIndex := skip; lineIndex < len(lines); lineIndex++ {
		row := targetRow(inner, screenStart+lineIndex-skip)
		if row >= inner.Max.Y {
			break
		}
		*targets = append(*targets, TranscriptClickTarget{
			Rect:     uv.Rect(inner.Min.X, row, inner.Dx(), 1),
			StartSeq: startSeq,
		})
	}
}

// memberRange is the byte range of one tool's summary in a collapsed run's
// joined summary, and the tool it belongs to.
type memberRange struct {
	start    int
	end      int
	startSeq uint64
}

func addSummaryToolRows(targets *[]TranscriptClickTarget, inner uv.Rectangle, screenStart int, skip int, width int, members []core.ChatEntry, lines []Line) {
	var tools []*core.ChatEntry
	for index := range members {
		if client.IsCompletedTool(&members[index]) {
			tools = append(tools, &members[index])
		}
	}
	if len(tools) == 0 {
		return
	}
	thoughtLines := 0
	for index := len(members) - 1; index >= 0; index-- {
		if members[index].Role == core.ChatRoleThought {
			thoughtLines = len(renderTranscriptEntry(&members[index], width, client.TranscriptRenderModeRich))
			break
		}
	}
	if len(tools) == 1 {
		toolLines := renderTranscriptEntry(tools[0], width, client.TranscriptRenderModeRich)
		toolStart := thoughtLines
		for lineIndex := toolStart; lineIndex < transcriptSaturatingAdd(toolStart, len(toolLines)); lineIndex++ {
			if lineIndex < skip || lineIndex >= len(lines) {
				continue
			}
			row := inner.Min.Y + min(screenStart+lineIndex-skip, math.MaxUint16)
			if row > math.MaxUint16 {
				// u16::checked_add overflowed.
				continue
			}
			if row >= inner.Max.Y {
				continue
			}
			*targets = append(*targets, TranscriptClickTarget{
				Rect:     uv.Rect(inner.Min.X, row, inner.Dx(), 1),
				StartSeq: tools[0].StartSeq,
			})
		}
		return
	}

	synthetic := client.CollapsedToolEntry(members)
	body := entryBodyRows(&synthetic, width, client.TranscriptRenderModeRich)
	syntheticLines := renderTranscriptEntry(&synthetic, width, client.TranscriptRenderModeRich)
	syntheticBodyStart := transcriptSaturatingAdd(max(len(syntheticLines)-transcriptSaturatingAdd(len(body), 1), 0), thoughtLines)
	summaries := make([]string, len(tools))
	for index, tool := range tools {
		if tool.ToolSummary != nil {
			summaries[index] = *tool.ToolSummary
		} else {
			summaries[index] = tool.Text
		}
	}
	source := strings.Join(summaries, ", ")
	memberRanges := make([]memberRange, 0, len(tools))
	sourceOffset := 0
	for index, tool := range tools {
		end := transcriptSaturatingAdd(sourceOffset, len(summaries[index]))
		memberRanges = append(memberRanges, memberRange{start: sourceOffset, end: end, startSeq: tool.StartSeq})
		separator := 0
		if index+1 < len(tools) {
			separator = 2
		}
		sourceOffset = transcriptSaturatingAdd(end, separator)
	}

	searchFrom := 0
	for bodyIndex, line := range body {
		lineIndex := transcriptSaturatingAdd(syntheticBodyStart, bodyIndex)
		lineText := rowText(line)
		content := strings.TrimRightFunc(strings.TrimPrefix(lineText, roleGutter()), unicode.IsSpace)
		content = strings.TrimLeftFunc(content, unicode.IsSpace)
		if content == "" {
			continue
		}
		offset := strings.Index(source[searchFrom:], content)
		if offset < 0 {
			continue
		}
		matchStart := searchFrom + offset
		matchEnd := transcriptSaturatingAdd(matchStart, len(content))
		searchFrom = matchEnd
		// Offscreen rows still consume source text so repeated summaries
		// remain associated with the call that actually occupies each row.
		if lineIndex < skip || lineIndex >= len(lines) {
			continue
		}
		row := targetRow(inner, screenStart+lineIndex-skip)
		if row >= inner.Max.Y {
			continue
		}
		for _, member := range memberRanges {
			overlapStart := max(member.start, matchStart)
			overlapEnd := min(member.end, matchEnd)
			if overlapStart >= overlapEnd {
				continue
			}
			left := min(displayWidth(content[:overlapStart-matchStart]), math.MaxUint16)
			right := displayWidth(content[:overlapEnd-matchStart])
			targetWidth := max(min(max(right-left, 0), math.MaxUint16), 1)
			x := inner.Min.X + roleGutterWidth + left
			*targets = append(*targets, TranscriptClickTarget{
				Rect:     uv.Rect(x, row, targetWidth, 1),
				StartSeq: member.startSeq,
			})
		}
	}
}

// slicedRowText is one cached transcript row cut to first..=last.
//
// The row is drawn the way the transcript draws it, so the engine slices the
// same cells that are on screen rather than a second column model.
func slicedRowText(line Line, width int, first int, last int) string {
	area := uv.Rect(0, 0, width, 1)
	buffer := uv.NewBuffer(width, 1)
	NewParagraph(TextFrom(line)).Render(area, buffer)
	frame := SurfaceFrameFixed(SurfaceIdTranscript{}, area)
	return ExtractRows(buffer, &frame, &SelectionRange{
		Start: NewContentPos(0, first),
		End:   NewContentPos(0, last),
	})
}

const terminalCommandPreviewCharacters = 160

func compactTerminalCommand(command string) string {
	command = strings.Join(strings.Fields(core.SanitizeTerminalText(command)), " ")
	if utf8.RuneCountInString(command) <= terminalCommandPreviewCharacters {
		return command
	}
	var preview strings.Builder
	for index, character := range []rune(command) {
		if index == terminalCommandPreviewCharacters-1 {
			break
		}
		preview.WriteRune(character)
	}
	preview.WriteString("…")
	return preview.String()
}

type ToolDiffstatRequest struct {
	toolCallID string
	revision   uint64
	item       *core.TranscriptItem
}

func toolDiffstatRequestFromItem(item *core.TranscriptItem) (ToolDiffstatRequest, bool) {
	body, ok := item.Body.(core.TranscriptBodyTool)
	if !ok {
		return ToolDiffstatRequest{}, false
	}
	call, err := body.Call.Decode()
	if err != nil {
		slog.Warn("could not decode a stored tool call while scheduling diff summary",
			"stable_id", item.StableID, "error", err)
		return ToolDiffstatRequest{}, false
	}
	status := acp.ToolCallStatusPending
	if call.Status != nil {
		status = *call.Status
	}
	toolStatus := core.ToolStatusOf(status)
	terminal := toolStatus == core.ToolStatusCompleted || toolStatus == core.ToolStatusFailed
	hasDiff := slices.ContainsFunc(call.Content, func(content acp.ToolCallContent) bool {
		return content.Kind == acp.ToolCallContentKindDiff
	})
	if !terminal || !hasDiff {
		return ToolDiffstatRequest{}, false
	}
	var revision uint64
	if item.LastChangedAtMs > 0 {
		revision = uint64(item.LastChangedAtMs)
	}
	return ToolDiffstatRequest{
		toolCallID: item.StableID,
		revision:   revision,
		item:       item,
	}, true
}

func (r ToolDiffstatRequest) compute() ([]string, error) {
	diffstats, ok := transcript.MaterializedToolDiffstats(r.item)
	if !ok {
		return nil, fmt.Errorf("tool %s no longer has a final diff", r.toolCallID)
	}
	return diffstats, nil
}
