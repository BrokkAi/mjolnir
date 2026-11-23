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
	"math"
	"slices"
	"strings"
	"unicode"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
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
	panic("unported: mj-chat/src/chat/transcript.rs user_message_marks")
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
	panic("unported: mj-chat/src/chat/transcript.rs TranscriptSnapshot::from_materialized_reusing")
}

func (s *TranscriptSnapshot) ConvertedEntries() []core.ChatEntry {
	return s.entries
}

// RichTailScrolled is the last maximumLines non-empty rows, skipping scroll
// rows above the live tail. Renders only the entries the window touches.
// Returns the rows and the scroll actually applied, clamped to the history
// available.
func (s *TranscriptSnapshot) RichTailScrolled(width int, maximumLines int, scroll int, tools ToolDisplay) ([]Line, int) {
	panic("unported: mj-chat/src/chat/transcript.rs TranscriptSnapshot::rich_tail_scrolled")
}

func (s *TranscriptSnapshot) BrowserTranscript(afterSeq *uint64) client.WebBrowserTranscript {
	return client.BrowserTranscript(s.entries, s.latestSeq, s.lastCompactionSeq, afterSeq)
}

func (s *TranscriptSnapshot) BrowserTail(maximumLines int) []string {
	panic("unported: mj-chat/src/chat/transcript.rs TranscriptSnapshot::browser_tail")
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

// prepareRenderCache drops rows invalidated by width, rendering mode, tool
// display, or palette changes, and sizes the cache to the current entry
// count.
func prepareRenderCache(entries []core.ChatEntry, cache *TranscriptRenderCache, width int, mode client.TranscriptRenderMode, tools ToolDisplay, expandedToolCalls map[uint64]struct{}, expandedMailboxEvents map[MailboxDisclosureId]struct{}) {
	panic("unported: mj-chat/src/chat/transcript.rs prepare_render_cache")
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
	panic("unported: mj-chat/src/chat/transcript.rs cached_entry_lines")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::queue_tool_detail")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::apply_tool_detail")
}

func (s *ChatState) invalidateToolDetailRows(index int) {
	if index < len(s.renderCache.entries) {
		s.renderCache.entries[index] = nil
	}
	s.renderCacheGeneration++
}

func (s *ChatState) queueVisibleRawToolDetails(top AnchorRow, visibleRows int) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::queue_visible_raw_tool_details")
}

// TranscriptPosition is where the reader is, independent of the tail-first
// loading window.
func (s *ChatState) TranscriptPosition() TranscriptPosition {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::transcript_position")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::resolve_transcript_position")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::handle_transcript_scrollbar_mouse")
}

// transcriptScrollbarMarkAt is the message a left click on a user-message
// mark opens. A click starts no drag, so a held mark does not seek the track.
func (s *ChatState) transcriptScrollbarMarkAt(mouse tea.MouseMsg) (int, bool) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::transcript_scrollbar_mark_at")
}

func (s *ChatState) setTranscriptScrollbarTarget(targetScroll int) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::set_transcript_scrollbar_target")
}

func (s *ChatState) prepareTranscriptScrollbarMetrics(width int) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::prepare_transcript_scrollbar_metrics")
}

func (s *ChatState) transcriptScrollbarOffset(top AnchorRow, anchor TranscriptAnchor) int {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::transcript_scrollbar_offset")
}

func (s *ChatState) updateTranscriptScrollbar(track uv.Rectangle, width int, viewportRows int, top AnchorRow, anchor TranscriptAnchor) (ScrollbarGeometry, bool) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::update_transcript_scrollbar")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::splice_transcript_prefix")
}

// restsOnOpeningReveal is whether the view still rests where the opening
// reveal put it, which the reader did not choose: no scroll has moved it
// since.
func (s *ChatState) restsOnOpeningReveal() bool {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::rests_on_opening_reveal")
}

func (s *ChatState) viewport(width int, height int) TranscriptViewport {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::viewport")
}

// tailViewport is the last height rows, walking backwards from the newest
// entry. These rows always reach the newest row, so the anchor to store is
// Bottom.
func (s *ChatState) tailViewport(height int) TranscriptViewport {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::tail_viewport")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::active_terminal_fallback")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::resolved_anchor")
}

// jumpToUserMessage moves the view to the previous or next message the user
// wrote, so that message starts at the top. Prompts Hel generated and entries
// the Rich mode hides are skipped. Past the newest message the view follows
// the tail again; before the oldest it stays where it is.
func (s *ChatState) jumpToUserMessage(forward bool) bool {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::jump_to_user_message")
}

func (s *ChatState) scrollHistoryUp(rows int) bool {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::scroll_history_up")
}

func (s *ChatState) scrollHistoryDown(rows int) bool {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::scroll_history_down")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::register_transcript_surface")
}

// rebuildTranscriptClickTargets rebuilds frame-local expansion targets from
// the same cached rows that were just painted. Keeping these targets
// transient avoids stale coordinates after wrapping, scrolling, or a collapse
// change.
func (s *ChatState) rebuildTranscriptClickTargets(inner uv.Rectangle, top AnchorRow, visibleRows int) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::rebuild_transcript_click_targets")
}

// recordTranscriptLinkFrame remembers where this frame drew the transcript,
// for transcriptLinkAt.
func (s *ChatState) recordTranscriptLinkFrame(inner uv.Rectangle, top AnchorRow, visibleRows int) {
	s.transcriptLinkFrame = &TranscriptLinkFrame{inner: inner, top: top, visibleRows: visibleRows}
}

// transcriptLinkAt is the destination of the link drawn at a screen cell of
// the last transcript frame. Only the clicked entry is rendered again, with
// link tracking, so ordinary frames pay nothing for links.
func (s *ChatState) transcriptLinkAt(column int, row int) (string, bool) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::transcript_link_at")
}

// toggleTranscriptAt toggles a tool or mailbox event at a frame-local screen
// coordinate. The selection router calls this only for a synthetic click, so
// ordinary mouse-down events remain owned by drag selection.
func (s *ChatState) toggleTranscriptAt(column int, row int) bool {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::toggle_transcript_at")
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
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::rows_between")
}

// anchorOffsetBy is the position offset rendered rows from from, clamped to
// the ends of the transcript. False when the walk exceeds the budget.
func (s *ChatState) anchorOffsetBy(from AnchorRow, offset int64) (AnchorRow, bool) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::anchor_offset_by")
}

// transcriptRowsFrom is count rendered rows starting at start, from the
// render cache, with how copying each gives back its text (Rust's
// Vec<(Line, RowCopy)>; the rows carry no selected columns).
//
// Copies come from rendering each entry again with tracking, as
// transcriptLinkAt does.
func (s *ChatState) transcriptRowsFrom(start AnchorRow, count int) ([]CopiedRow, bool) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::transcript_rows_from")
}

// TranscriptSelectionText is the transcript text a finished selection
// covers.
//
// Rows come from the render cache, so rows scrolled out of the viewport are
// copied without the frame buffer holding them. Only endpoints that cut a row
// mid-column go back through a one-row render, which is what gives the
// engine the cells it needs to slice wide characters.
func (s *ChatState) TranscriptSelectionText(selection *SelectionRange) (string, bool) {
	panic("unported: mj-chat/src/chat/transcript.rs ChatState::transcript_selection_text")
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
	panic("unported: mj-chat/src/chat/transcript.rs copied_rows_text")
}

// rowText is one cached transcript row as plain text.
func rowText(line Line) string {
	var text strings.Builder
	for _, span := range line.Spans {
		text.WriteString(span.Content)
	}
	return strings.TrimRightFunc(text.String(), unicode.IsSpace)
}

func addWholeToolRows(targets *[]TranscriptClickTarget, inner uv.Rectangle, screenStart int, skip int, lines []Line, startSeq uint64) {
	panic("unported: mj-chat/src/chat/transcript.rs add_whole_tool_rows")
}

func addSummaryToolRows(targets *[]TranscriptClickTarget, inner uv.Rectangle, screenStart int, skip int, width int, members []core.ChatEntry, lines []Line) {
	panic("unported: mj-chat/src/chat/transcript.rs add_summary_tool_rows")
}

// slicedRowText is one cached transcript row cut to first..=last.
//
// The row is drawn the way the transcript draws it, so the engine slices the
// same cells that are on screen rather than a second column model.
func slicedRowText(line Line, width int, first int, last int) string {
	panic("unported: mj-chat/src/chat/transcript.rs sliced_row_text")
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
	panic("unported: mj-chat/src/chat/transcript.rs ToolDiffstatRequest::from_item")
}

func (r ToolDiffstatRequest) compute() ([]string, error) {
	diffstats, ok := transcript.MaterializedToolDiffstats(r.item)
	if !ok {
		return nil, fmt.Errorf("tool %s no longer has a final diff", r.toolCallID)
	}
	return diffstats, nil
}
