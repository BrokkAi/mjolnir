// Port of mj-tui/src/resume.rs.

// The resume dialog: the one surface that lists sessions which are not live.
//
// Its Hel tab lists Hel's own stopped, lost, and destroyed records. Its Import
// tab lists native sessions scanned out of each harness home. A Hel record and
// the native session it was imported from are the same conversation, so the
// native copy is omitted: the Hel record carries the checkpoint and durable
// queue.
//
// Nothing here reads the filesystem. Native scans arrive from background tasks
// as ImportProfileOption updates, and the merge below is a pure function over
// what has already been received.
//
// Rust shares the scan results, previews and hits behind Arc so a
// confirmation can copy the dialog cheaply, and copies on write
// (Arc::make_mut). The Go slices and maps below are shared the same way when
// a ResumeDialog value is copied, so code that changes one replaces it with
// a changed copy rather than mutating it in place. Rust's RefCell and Cell
// fields are plain fields mutated through the dialog's pointer.

package tui

import (
	"cmp"
	"fmt"
	"iter"
	"maps"
	"math"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// localOrigin is the origin shown for a native session that has never run
// under Hel.
const localOrigin = "local"

// previewScrollRows is how many rows one wheel notch moves the preview pane,
// matching the chat transcript's step.
const previewScrollRows = 3

// ResumeFocus names the resume dialog's controls.
type ResumeFocus int

const (
	ResumeFocusTabs ResumeFocus = iota
	ResumeFocusSearch
	ResumeFocusSessions
	ResumeFocusPreviousHit
	ResumeFocusNextHit
	ResumeFocusCancel
	ResumeFocusDestroy
	ResumeFocusCopyID
	ResumeFocusOpen
)

// ResumeTab is one tab of the resume dialog's strip.
type ResumeTab int

const (
	// ResumeTabLive lists sessions running right now, in every workspace.
	// Enter goes to one instead of recovering it, so this is the only tab
	// listing sessions the dashboard already holds.
	ResumeTabLive ResumeTab = iota
	ResumeTabHel
	ResumeTabImport
	// ResumeTabArchive lists sessions SessionWiki kept after the tool that
	// ran them deleted its own copy. They have no checkpoint; Enter restores
	// one into a new session.
	ResumeTabArchive
)

// resumeTabCount is Rust's `ResumeTab::COUNT`: how many tabs the strip has.
// The arrow keys wrap around it and the per-tab hit counts are one entry
// each, so both follow the tab list.
const resumeTabCount = 4

func (t ResumeTab) index() int { return int(t) }

func resumeTabFromIndex(index int) ResumeTab {
	switch index {
	case 0:
		return ResumeTabLive
	case 1:
		return ResumeTabHel
	case 2:
		return ResumeTabImport
	default:
		return ResumeTabArchive
	}
}

func (t ResumeTab) includes(row *ResumeRow) bool {
	switch t {
	case ResumeTabLive:
		return row.key.Kind == ResumeRowKeyKindLive
	case ResumeTabHel:
		return row.key.Kind == ResumeRowKeyKindHel
	case ResumeTabImport:
		return row.key.Kind == ResumeRowKeyKindNative
	case ResumeTabArchive:
		return row.key.Kind == ResumeRowKeyKindArchive
	}
	return false
}

// ResumeRowKeyKind is the variant of a ResumeRowKey.
type ResumeRowKeyKind int

const (
	// ResumeRowKeyKindLive is a session running right now, keyed by its Hel
	// session id.
	ResumeRowKeyKindLive ResumeRowKeyKind = iota
	// ResumeRowKeyKindHel is a Hel session record, keyed by its Hel session
	// id.
	ResumeRowKeyKindHel
	// ResumeRowKeyKindNative is a native session with no Hel record, keyed by
	// harness and native id.
	ResumeRowKeyKindNative
	// ResumeRowKeyKindArchive is an archived session that lives only in the
	// SessionWiki index, keyed by its SessionWiki id.
	ResumeRowKeyKindArchive
)

// ResumeRowKey is the identity of one row, stable across rescans so the
// selection survives an incremental scan update.
//
// Rust's enum is a kind and its fields: ID is the Hel session id (Live,
// Hel), the native session id (Native) or the SessionWiki id (Archive), and
// Harness is set only on a Native key. The struct is comparable, so == is
// Rust's derived PartialEq; compare is its derived Ord.
type ResumeRowKey struct {
	Kind    ResumeRowKeyKind
	Harness core.HarnessKind
	ID      string
}

func resumeRowKeyLive(sessionID string) ResumeRowKey {
	return ResumeRowKey{Kind: ResumeRowKeyKindLive, ID: sessionID}
}

func resumeRowKeyHel(sessionID string) ResumeRowKey {
	return ResumeRowKey{Kind: ResumeRowKeyKindHel, ID: sessionID}
}

func resumeRowKeyNative(harness core.HarnessKind, nativeSessionID string) ResumeRowKey {
	return ResumeRowKey{Kind: ResumeRowKeyKindNative, Harness: harness, ID: nativeSessionID}
}

func resumeRowKeyArchive(wikiID string) ResumeRowKey {
	return ResumeRowKey{Kind: ResumeRowKeyKindArchive, ID: wikiID}
}

// compare is Rust's derived Ord: by variant, then by the variant's fields
// (a Native key's harness in declaration order, then its id).
func (k ResumeRowKey) compare(other ResumeRowKey) int {
	if k.Kind != other.Kind {
		if k.Kind < other.Kind {
			return -1
		}
		return 1
	}
	if k.Kind == ResumeRowKeyKindNative {
		left, right := core.HarnessKindOrdinal(k.Harness), core.HarnessKindOrdinal(other.Harness)
		if left != right {
			if left < right {
				return -1
			}
			return 1
		}
	}
	return strings.Compare(k.ID, other.ID)
}

// GoString is Rust's derived Debug for the key, which the dialog uses as the
// list's identity. It names a Native key's harness by its wire value.
func (k ResumeRowKey) GoString() string {
	switch k.Kind {
	case ResumeRowKeyKindLive:
		return fmt.Sprintf("Live(%q)", k.ID)
	case ResumeRowKeyKindHel:
		return fmt.Sprintf("Hel(%q)", k.ID)
	case ResumeRowKeyKindNative:
		return fmt.Sprintf("Native(%s, %q)", k.Harness, k.ID)
	default:
		return fmt.Sprintf("Archive(%q)", k.ID)
	}
}

// ResumeRowStatus is what selecting the row does, and whether it may be
// selected at all.
type ResumeRowStatus int

const (
	// ResumeRowStatusRunning is a session the dashboard is already running:
	// Enter goes to it.
	ResumeRowStatusRunning ResumeRowStatus = iota
	// ResumeRowStatusResumable is a checkpointed Hel record: Enter opens the
	// resume wizard.
	ResumeRowStatusResumable
	// ResumeRowStatusImportable is a native session Hel has never adopted:
	// Enter imports it.
	ResumeRowStatusImportable
	// ResumeRowStatusRestorable means only SessionWiki's copy is left: Enter
	// restores it into a new session carrying a summary of the old
	// transcript.
	ResumeRowStatusRestorable
	// ResumeRowStatusLost: the target vanished without a verified
	// checkpoint. The controller discards such a record as soon as it writes
	// it, so this row is seen only when that automatic discard failed and
	// left the record behind.
	ResumeRowStatusLost
	// ResumeRowStatusDataLoss: force-destroyed by an older build. There is
	// nothing left to restore, and, as with ResumeRowStatusLost, the row is
	// seen only when the automatic discard of the leftover record failed.
	ResumeRowStatusDataLoss
)

func (s ResumeRowStatus) isRecoverable() bool {
	switch s {
	case ResumeRowStatusRunning, ResumeRowStatusResumable, ResumeRowStatusImportable, ResumeRowStatusRestorable:
		return true
	}
	return false
}

// warning is the short marker shown in the origin column, sized to fit
// beside it.
func (s ResumeRowStatus) warning() (string, bool) {
	var what string
	switch s {
	case ResumeRowStatusLost:
		what = "lost"
	case ResumeRowStatusDataLoss:
		what = "data lost"
	default:
		return "", false
	}
	return fmt.Sprintf("%s %s", chat.ThemeGlyphs().Warning, what), true
}

// explanation is why the row cannot be resumed, in full, for the details
// line and the notice a rejected Enter leaves behind.
func (s ResumeRowStatus) explanation() (string, bool) {
	switch s {
	case ResumeRowStatusLost:
		return "lost without a verified checkpoint", true
	case ResumeRowStatusDataLoss:
		return "force-destroyed; nothing is left to restore", true
	}
	return "", false
}

// ResumeRow is one row of the resume dialog.
type ResumeRow struct {
	key       ResumeRowKey
	profileID string
	title     string
	// origin is where the session ran and the project it opened directly,
	// matching the live one-line summary. Native sessions use
	// `local/<project>` because Hel has not chosen their import destination
	// yet. A stored target missing from config is shown verbatim because its
	// kind is no longer known. A Live row carries its workspace's name
	// instead: the question about a running session is where to find it.
	origin         string
	details        string
	lastActivityMs int64
	status         ResumeRowStatus
	publication    *core.PublicationState
	// nativelyArchived is reported by the native harness. This metadata is
	// informational only; it does not affect visibility or dispatch a
	// provider write.
	nativelyArchived  bool
	unavailableReason *string
	// moveRecovery is a retained failed/cancelled Move for this record, when
	// recovery is possible. The dialog turns Enter into an explicit recovery
	// choice.
	moveRecovery *core.MoveOperation
	// wikiMatch is the SessionWiki session a search matched to this live row,
	// so the preview can show the same transcript the hit came from.
	wikiMatch *string
	// wikiRank is where the matching hit sat in the index's answer. A query
	// lists only rows that have one, in this order, so the index's ranking is
	// what the dialog shows.
	wikiRank *int
	// wikiProfile is the harness profile the index says an archived session
	// ran under, when it carries one. Restore opens the wizard on it.
	wikiProfile *string
	// wikiTarget is the target template the index says an archived session
	// ran on, when it carries one. Restore opens the wizard on it.
	wikiTarget *string
}

func (r *ResumeRow) sessionID() (string, bool) {
	switch r.key.Kind {
	case ResumeRowKeyKindLive, ResumeRowKeyKindHel:
		return r.key.ID, true
	}
	return "", false
}

// wikiID is the SessionWiki session this row previews, which an archived row
// always has and a live row has when a search matched it.
func (r *ResumeRow) wikiID() (string, bool) {
	if r.key.Kind == ResumeRowKeyKindArchive {
		return r.key.ID, true
	}
	if r.wikiMatch != nil {
		return *r.wikiMatch, true
	}
	return "", false
}

// ResumeDialog is the open resume dialog.
type ResumeDialog struct {
	discoveryID uint64
	sync        SyncEpisode
	// profiles holds the native scan results, one entry per configured
	// profile. Entries appear immediately as placeholders and fill in as
	// background scans report.
	//
	// A harness home holds thousands of sessions, and the dialog is copied
	// whenever a confirmation interrupts it, so the scan results are shared
	// rather than duplicated (replace the slice to change it).
	profiles []ImportProfileOption
	tab      ResumeTab
	// liveState is the state the Live tab is narrowed to, or nil for all.
	// Ignored on the other tabs, which list sessions that have no current
	// state.
	liveState *SessionStateFilter
	selected  *ResumeRowKey
	rowIndex  int
	search    chat.TextInput
	form      chat.Dialog[ResumeFocus]
	openedAt  time.Time
	// wiki is the newest search results, or the most recent sessions before
	// anything has been typed. Shared rather than copied: a confirmation
	// clones the whole dialog.
	wiki []client.WikiRow
	// wikiRequestID is the search this dialog last asked for. A result that
	// names an older request is stale and dropped.
	wikiRequestID uint64
	wikiSearch    WikiSearchState
	// liveSearch is the live conversation answer for the same query
	// generation. It is independent of the limited history page and its
	// failures.
	liveSearch LiveSearchState
	// previews are briefings already fetched, by SessionWiki id, for the
	// dialog's life.
	previews map[string]string
	// previewPending is the briefing being fetched now, so one selection asks
	// only once.
	previewPending *string
	// hits are matching passages already fetched, by SessionWiki id and
	// query. A different query over the same session is a different answer.
	hits map[PreviewKey]client.WikiHitTranscript
	// hitsPending is the passages being fetched now, so one selection asks
	// only once.
	hitsPending *PreviewKey
	// previewScroll is the first wrapped row the preview pane shows.
	previewScroll int
	// previewScrollbar is the pointer gesture state for the preview's shared
	// vertical scrollbar.
	previewScrollbar chat.ScrollbarDrag
	// previewHit is which hit of the shown transcript the pane is sitting on,
	// so `n` and `N` move from where the reader is.
	previewHit int
	// previewKey is the SessionWiki session and query the scroll offset
	// belongs to. Either one changing starts the pane at the top, or at the
	// new query's first hit.
	previewKey *PreviewKey
	// history is what the daemon said about stopped sessions when the dialog
	// opened. The runtime feed carries only live sessions, so the Mjolnir tab
	// and the import de-duplication wait for this answer.
	history HistoryLoad
	// opening is the Mjolnir row whose record is being fetched to open the
	// wizard.
	opening *string
}

// HistoryLoad is the dialog's request for stopped sessions.
type HistoryLoad interface {
	isHistoryLoad()
}

// HistoryLoadLoading: the daemon has not answered yet.
type HistoryLoadLoading struct{}

// HistoryLoadLoaded is the daemon's answer, shared between copies of the
// dialog.
type HistoryLoadLoaded struct{ Value *ResumeHistory }

// HistoryLoadFailed is why the daemon could not list stopped sessions.
type HistoryLoadFailed struct{ Value string }

func (HistoryLoadLoading) isHistoryLoad() {}
func (HistoryLoadLoaded) isHistoryLoad()  {}
func (HistoryLoadFailed) isHistoryLoad()  {}

// nativeSessionKey is Rust's `(HarnessKind, String)` adopted-session pair.
type nativeSessionKey struct {
	harness         core.HarnessKind
	nativeSessionID string
}

// ResumeHistory is the daemon's answer about stopped sessions, as the dialog
// keeps it.
type ResumeHistory struct {
	// candidates has one row for each inactive top-level session, by id.
	candidates map[string]client.ResumeCandidate
	// adopted are native sessions that a record held when the dialog opened,
	// live or not.
	adopted map[nativeSessionKey]struct{}
	// localCheckoutRoots are local checkouts Mjolnir created for its
	// sessions.
	localCheckoutRoots []string
	// moves are durable moves of the listed sessions, for "move needs
	// recovery" marks.
	moves map[string]core.MoveOperation
}

// resumeHistoryFrom is Rust's `From<ResumeCandidates> for ResumeHistory`.
func resumeHistoryFrom(answer client.ResumeCandidates) ResumeHistory {
	history := ResumeHistory{
		candidates:         make(map[string]client.ResumeCandidate, len(answer.Candidates)),
		adopted:            make(map[nativeSessionKey]struct{}, len(answer.AdoptedNativeSessions)),
		localCheckoutRoots: answer.LocalCheckoutRoots,
		moves:              make(map[string]core.MoveOperation, len(answer.Moves)),
	}
	for _, candidate := range answer.Candidates {
		history.candidates[candidate.SessionID] = candidate
	}
	for _, pair := range answer.AdoptedNativeSessions {
		history.adopted[nativeSessionKey{harness: pair.First, nativeSessionID: pair.Second}] = struct{}{}
	}
	for _, operation := range answer.Moves {
		history.moves[operation.Selection.SessionID] = operation
	}
	return history
}

// WikiSearchState keeps prior rows while visibly refreshing the same query
// after a sync.
type WikiSearchState interface {
	isWikiSearchState()
}

type WikiSearchStatePending struct{}
type WikiSearchStateAnswered struct{ Value client.WikiStatus }
type WikiSearchStateRefreshing struct{ Value client.WikiStatus }
type WikiSearchStateFailed struct{ Value string }

func (WikiSearchStatePending) isWikiSearchState()    {}
func (WikiSearchStateAnswered) isWikiSearchState()   {}
func (WikiSearchStateRefreshing) isWikiSearchState() {}
func (WikiSearchStateFailed) isWikiSearchState()     {}

// wikiSearchStateStatus is Rust's `WikiSearchState::status`.
func wikiSearchStateStatus(state WikiSearchState) (client.WikiStatus, bool) {
	switch state := state.(type) {
	case WikiSearchStateAnswered:
		return state.Value, true
	case WikiSearchStateRefreshing:
		return state.Value, true
	}
	return client.WikiStatus{}, false
}

// LiveSearchState is the live conversation search for the dialog's query.
// The sets hold matching session ids (Rust's BTreeSet<String>).
type LiveSearchState interface {
	isLiveSearchState()
}

type LiveSearchStatePending struct{}
type LiveSearchStateAnswered struct{ Value map[string]struct{} }
type LiveSearchStateRefreshing struct{ Value map[string]struct{} }
type LiveSearchStateFailed struct{ Value string }

func (LiveSearchStatePending) isLiveSearchState()    {}
func (LiveSearchStateAnswered) isLiveSearchState()   {}
func (LiveSearchStateRefreshing) isLiveSearchState() {}
func (LiveSearchStateFailed) isLiveSearchState()     {}

// liveSearchStateMatches is Rust's `LiveSearchState::matches`.
func liveSearchStateMatches(state LiveSearchState, sessionID string) bool {
	switch state := state.(type) {
	case LiveSearchStateAnswered:
		_, ok := state.Value[sessionID]
		return ok
	case LiveSearchStateRefreshing:
		_, ok := state.Value[sessionID]
		return ok
	}
	return false
}

// resumeDialog is the open resume dialog, including one held under the
// destroy confirmation it opened; nil otherwise.
func resumeDialog(mode Mode) *ResumeDialog {
	switch m := mode.(type) {
	case ModeResumeDialog:
		return m.ResumeDialog
	case ModeConfirm:
		if m.ConfirmDialog == nil {
			return nil
		}
		if destroy, ok := m.confirmation.(ConfirmationDestroyStopped); ok {
			return destroy.Reopen
		}
	}
	return nil
}

// resumeDialogMut is resumeDialog for a caller that changes the dialog in
// place.
func resumeDialogMut(mode *Mode) *ResumeDialog {
	return resumeDialog(*mode)
}

// PreviewKey is what the preview pane is showing: a SessionWiki session, and
// the query whose passages are shown in it (Rust's `(String, String)`). The
// query is empty when the pane is showing the briefing instead.
type PreviewKey struct {
	WikiID string
	Query  string
}

func (d *ResumeDialog) focused() ResumeFocus {
	if focused, ok := d.form.Focused(); ok {
		return focused
	}
	return ResumeFocusSessions
}

func (d *ResumeDialog) prepare(rows []ResumeRow) {
	form := &d.form
	form.BeginUpdate()
	form.DeclareWithEnabled(
		ResumeFocusTabs,
		chat.ControlKindTabs{Len: resumeTabCount, Selected: d.tab.index()},
		true,
	)
	form.DeclareWithEnabled(ResumeFocusSearch, chat.ControlKindTextField{}, d.searchEnabled())
	form.DeclareWithEnabled(
		ResumeFocusSessions,
		chat.ControlKindChoiceList{Len: len(rows), Selected: d.rowIndex},
		len(rows) > 0,
	)
	hitCount := 0
	if _, hits, ok := d.previewBody(rows); ok {
		hitCount = len(hits)
	}
	form.DeclareWithEnabled(
		ResumeFocusPreviousHit,
		chat.ControlKindButton{},
		d.previewHit > 0 && hitCount > 0,
	)
	form.DeclareWithEnabled(
		ResumeFocusNextHit,
		chat.ControlKindButton{},
		d.previewHit+1 < hitCount,
	)
	form.DeclareWithEnabled(ResumeFocusCancel, chat.ControlKindButton{}, true)
	if d.tab == ResumeTabHel {
		form.DeclareWithEnabled(ResumeFocusDestroy, chat.ControlKindButton{}, d.canDestroy(rows))
	}
	form.DeclareWithEnabled(ResumeFocusOpen, chat.ControlKindButton{}, d.canOpen(rows))
	keys := make([]string, len(rows))
	for index := range rows {
		keys[index] = rows[index].key.GoString()
	}
	form.SetListIdentity(ResumeFocusSessions, "["+strings.Join(keys, ", ")+"]")
	form.EndFrame(ResumeFocusSessions)
}

// searchCaretAtEdge reports whether an arrow key pressed in the search box
// has no caret movement left to make, so it belongs to the tab strip
// instead. An empty box sits at both ends at once.
func (d *ResumeDialog) searchCaretAtEdge(code rune) bool {
	switch code {
	case tea.KeyLeft:
		return d.search.Cursor() == 0
	case tea.KeyRight:
		return d.search.Cursor() == len(d.search.Value())
	}
	return false
}

// searchEnabled: searches can use already indexed sessions while a build is
// running. Only an incompatible index prevents searching the history tabs.
func (d *ResumeDialog) searchEnabled() bool {
	if d.tab == ResumeTabLive {
		return true
	}
	status, ok := wikiSearchStateStatus(d.wikiSearch)
	return !ok || status.State != client.WikiIndexStateVersionMismatch
}

// searchPlaceholder is what stands in the search box while it cannot be
// typed into.
func (d *ResumeDialog) searchPlaceholder() (string, bool) {
	if d.tab == ResumeTabLive {
		return "", false
	}
	status, ok := wikiSearchStateStatus(d.wikiSearch)
	if ok && status.State == client.WikiIndexStateVersionMismatch {
		return "SessionWiki index is from a newer version", true
	}
	return "", false
}

func (d *ResumeDialog) canOpen(rows []ResumeRow) bool {
	if d.rowIndex >= len(rows) {
		return false
	}
	row := &rows[d.rowIndex]
	_, unresumable := row.status.explanation()
	return !unresumable && row.unavailableReason == nil
}

// canDestroy: only rows with a Mjolnir session record can be destroyed.
func (d *ResumeDialog) canDestroy(rows []ResumeRow) bool {
	if d.tab != ResumeTabHel || d.rowIndex >= len(rows) {
		return false
	}
	_, ok := rows[d.rowIndex].sessionID()
	return ok
}

func (d *ResumeDialog) isScanning() bool {
	for _, profile := range d.profiles {
		if profile.Error == nil &&
			(profile.ScanProgress == nil || profile.ScanProgress[0] < profile.ScanProgress[1]) {
			return true
		}
	}
	return false
}

// scanProgress is the scanned and total counts summed across every profile
// still loading.
func (d *ResumeDialog) scanProgress() (scanned int, total int) {
	for _, profile := range d.profiles {
		if profile.ScanProgress != nil {
			scanned += profile.ScanProgress[0]
			total += profile.ScanProgress[1]
		}
	}
	return scanned, total
}

// previewWikiID is the SessionWiki session the preview pane is showing,
// which is the one behind the selected row. False when the row has none, and
// the pane is then not drawn at all.
func (d *ResumeDialog) previewWikiID(rows []ResumeRow) (string, bool) {
	index, ok := selectedIndex(d, len(rows))
	if !ok {
		return "", false
	}
	return rows[index].wikiID()
}

// hasPreview reports whether the dialog has a preview pane this frame.
func (d *ResumeDialog) hasPreview(rows []ResumeRow) bool {
	_, ok := d.previewWikiID(rows)
	return ok
}

// activeQuery is the query the pane is previewing passages for, or false
// when there is nothing typed and the pane shows the briefing instead.
func (d *ResumeDialog) activeQuery() (string, bool) {
	query := d.search.String()
	if strings.TrimSpace(query) == "" {
		return "", false
	}
	return query, true
}

// previewBody is the preview body as logical lines of spans, before
// wrapping, together with the logical line each hit starts on so the pane
// can be opened on a hit before the lines are wrapped.
//
// The body is the query's matching passages while a query is active, and
// the cached briefing otherwise, or one line saying it is on its way. Spans
// rather than a string because a hit preview highlights the matched words
// inside a line. ok is false when the selected row has no preview.
func (d *ResumeDialog) previewBody(rows []ResumeRow) (lines []chat.Line, hitLines []int, ok bool) {
	wikiID, ok := d.previewWikiID(rows)
	if !ok {
		return nil, nil, false
	}
	muted := chat.Style{}.Foreground(chat.ThemePalette().Muted)
	query, ok := d.activeQuery()
	if !ok {
		brief, ok := d.previews[wikiID]
		if !ok {
			return []chat.Line{chat.StyledLine("Loading the archived transcript…", muted)}, nil, true
		}
		// The briefing's first line is the SessionWiki crate's own
		// `# Previous session: <title>` heading, which repeats the title of
		// the row the pane sits under.
		zone := displayNow().Location()
		for index, line := range resumeStrLines(brief) {
			if index == 0 {
				continue
			}
			lines = append(lines, chat.RawLine(localizeBriefDate(line, zone)))
		}
		return lines, nil, true
	}
	transcript, ok := d.hits[PreviewKey{WikiID: wikiID, Query: query}]
	if !ok {
		return []chat.Line{chat.StyledLine("Loading the matching passages…", muted)}, nil, true
	}
	lines, hitLines = hitTranscriptLines(&transcript)
	return lines, hitLines, true
}

func (d *ResumeDialog) errors() []string {
	var errors []string
	for _, profile := range d.profiles {
		if profile.Error != nil {
			errors = append(errors, fmt.Sprintf("%s: %s", profile.ProfileID, *profile.Error))
		}
	}
	return errors
}

// localizeBriefDate is the briefing's metadata line with its date moved to
// zone. SessionWiki writes `- Tool: … | Date: YYYY-MM-DD HH:MM` in UTC, while
// every other time on screen is local. Any other line comes back unchanged.
func localizeBriefDate(line string, zone *time.Location) string {
	const marker = " | Date: "
	const format = "2006-01-02 15:04"
	if !strings.HasPrefix(line, "- Tool: ") {
		return line
	}
	at := strings.LastIndex(line, marker)
	if at < 0 {
		return line
	}
	start := at + len(marker)
	// Rust's `str::get(start..start + 16)`: in range and on character
	// boundaries.
	if start+16 > len(line) || (start+16 < len(line) && !utf8.RuneStart(line[start+16])) {
		return line
	}
	utc, err := time.ParseInLocation(format, line[start:start+16], time.UTC)
	if err != nil {
		return line
	}
	return line[:start] + utc.In(zone).Format(format) + line[start+16:]
}

// selectedIndex is the row the dialog points at, clamped to the list it
// actually has. A state reload can shrink the list under a selection that
// was valid a moment ago.
func selectedIndex(dialog *ResumeDialog, length int) (int, bool) {
	if length == 0 {
		return 0, false
	}
	return min(dialog.rowIndex, length-1), true
}

// tabStep is how far along the tab strip one arrow key moves, counted
// forward so the caller's % wraps Left around the left end of the strip.
func tabStep(code rune) (int, bool) {
	switch code {
	case tea.KeyLeft:
		return resumeTabCount - 1, true
	case tea.KeyRight:
		return 1, true
	}
	return 0, false
}

// timestampMs is epoch milliseconds for an RFC 3339 timestamp, or false
// when it cannot be parsed. An unparseable timestamp must not silently sort
// as "now".
func timestampMs(timestamp string) (int64, bool) {
	parsed, ok := core.ParseChronoRFC3339(timestamp)
	if !ok {
		return 0, false
	}
	return parsed.UnixMilli(), true
}

// helRow is the Mjolnir tab's row for one session.
func helRow(candidate client.ResumeCandidate) ResumeRow {
	status := helRowStatus(candidate.State)
	var details string
	if reason, ok := status.explanation(); ok {
		details = fmt.Sprintf("%s · %s", reason, candidate.Project)
	} else if !candidate.HasCheckpoint {
		details = fmt.Sprintf("no checkpoint · %s", candidate.Project)
	} else {
		details = candidate.Project
	}
	if candidate.Publication != nil {
		switch *candidate.Publication {
		case core.PublicationStateUnpublished:
			details += " · Unpublished work in recovery copy"
		case core.PublicationStateUnknown:
			details += " · Publication status unknown"
		}
	}
	var lastActivityMs int64
	if candidate.LastActivityMs != nil {
		lastActivityMs = *candidate.LastActivityMs
	}
	return ResumeRow{
		key:            resumeRowKeyHel(candidate.SessionID),
		profileID:      candidate.LastProfile,
		title:          candidate.Title,
		origin:         candidate.Origin,
		details:        details,
		lastActivityMs: lastActivityMs,
		status:         status,
		publication:    candidate.Publication,
	}
}

func helRowStatus(state core.SessionState) ResumeRowStatus {
	switch state {
	case core.SessionStateLost:
		return ResumeRowStatusLost
	case core.SessionStateDestroyedWithDataLoss:
		return ResumeRowStatusDataLoss
	}
	return ResumeRowStatusResumable
}

// unresumableReason is why a stopped record cannot be resumed at all, for
// the Sessions row that lists a stopped failure as well as for the dialog.
func unresumableReason(state core.SessionState) (string, bool) {
	return helRowStatus(state).explanation()
}

func unresumableNotice(reason string) string {
	return fmt.Sprintf("This session was %s. Use Destroy to remove its record.", reason)
}

// displayZone is the zone resume times display in: local time, except UTC
// under test so goldens do not follow the machine's time zone. It stands for
// Rust's `DisplayZone` type (chrono::Local, or chrono::Utc under cfg(test)).
// testing.Testing is true in any Go test binary, where Rust's cfg(test)
// covered only mj-tui's own tests.
func displayZone() *time.Location {
	if testing.Testing() {
		return time.UTC
	}
	return time.Local
}

func displayNow() time.Time {
	return time.Now().In(displayZone())
}

const sevenDaysMs int64 = 7 * 24 * 60 * 60 * 1_000

func formatLastActive(now time.Time, thenMs int64) string {
	if thenMs <= 0 {
		return "unknown"
	}
	// chrono's from_timestamp_millis covers years -262143 to 262142; past
	// that the instant is unknown, as it is in Rust.
	if thenMs > maxChronoTimestampMs {
		return "unknown"
	}
	then := time.UnixMilli(thenMs)
	nowMs := now.UnixMilli()
	elapsedMs := int64(0)
	if thenMs <= nowMs {
		elapsedMs = nowMs - thenMs
	}
	if elapsedMs > sevenDaysMs {
		return then.In(now.Location()).Format("Jan 2, 2006")
	}

	seconds := elapsedMs / 1_000
	switch {
	case seconds < 60:
		return "just now"
	case seconds < 3_600:
		return relativeTime(seconds/60, "minute")
	case seconds < 86_400:
		return relativeTime(seconds/3_600, "hour")
	default:
		return relativeTime(seconds/86_400, "day")
	}
}

func relativeTime(value int64, unit string) string {
	return fmt.Sprintf("%s ago", counted(value, unit, unit+"s"))
}

// harnessOfHit is the harness an indexed row ran, from the SessionWiki tool
// name or, for a Mjolnir row, from the harness kind the index carries.
//
// A Mjolnir row's tool name is `mjolnir`, which names no harness of its own;
// the kind comes from the `mj-harness:` tag the sync writes instead.
func harnessOfHit(hit *client.WikiRow) (core.HarnessKind, bool) {
	if kind, ok := harnessOfTool(hit.Tool); ok {
		return kind, true
	}
	if hit.Harness == nil {
		return "", false
	}
	kind, err := core.ParseHarnessKind(*hit.Harness)
	if err != nil {
		return "", false
	}
	return kind, true
}

// harnessOfTool is the Mjolnir harness a SessionWiki tool name stands for,
// when one does. Tools Mjolnir cannot run have no harness and are never
// deduplicated against an import row.
func harnessOfTool(tool string) (core.HarnessKind, bool) {
	switch tool {
	case "claude-code":
		return core.HarnessKindClaude, true
	case "codex":
		return core.HarnessKindCodex, true
	}
	return "", false
}

// mergedResumeRows merges Hel's non-live records with the scanned native
// sessions into one list, newest first. Rows are returned unfiltered; the
// dialog applies the tab and search on top.
//
// Dedupe rule: a Hel record whose native_session_id matches a scanned native
// session of the same harness replaces that native row entirely.
func mergedResumeRows(config *core.Config, state *core.StateState, history HistoryLoad, profiles []ImportProfileOption, wiki []client.WikiRow) []ResumeRow {
	// Until the daemon answers, nothing says which stopped sessions exist or
	// which native sessions Mjolnir already holds, so the Mjolnir and Import
	// rows wait for it. Archived search hits do not.
	var loaded *ResumeHistory
	if answer, ok := history.(HistoryLoadLoaded); ok {
		loaded = answer.Value
	}
	adopted := map[nativeSessionKey]struct{}{}
	var ownCheckouts []string
	if loaded != nil {
		for key := range loaded.adopted {
			adopted[key] = struct{}{}
		}
		ownCheckouts = append(ownCheckouts, loaded.localCheckoutRoots...)
	}
	// Live records joined after the dialog opened can adopt more.
	for session := range state.Sessions.Values() {
		// Every record adopts its native session, live ones included: the
		// native file of a session Hel is running now must not be offered as
		// a second import.
		if session.NativeSessionID != nil {
			adopted[nativeSessionKey{harness: session.HarnessKind, nativeSessionID: *session.NativeSessionID}] = struct{}{}
		}
		// The record names one native thread, but a `/clear` starts another
		// and the store records it only at the next checkpoint. Every thread
		// that ran in the session's own checkout is the session's (R10-3).
		if checkout, err := state.Checkout(session.ID); err == nil {
			if managed, ok := checkout.(core.CheckoutManagedWorktree); ok && managed.Worktree != nil {
				if _, local := managed.Worktree.Target.(core.ManagedWorktreeTargetLocal); local {
					ownCheckouts = append(ownCheckouts, managed.Worktree.WorktreeRoot)
				}
			}
		}
	}
	var rows []ResumeRow
	if loaded == nil {
		profiles = nil
	}
	// The daemon's rows, plus inactive records the feed itself carries (a
	// session that failed after the dialog opened). The feed's record is
	// newer when there is one: a session resumed since the dialog opened is
	// active now, and the Live tab lists it. A sub-agent is resumed through
	// its parent.
	if loaded != nil {
		ids := make([]string, 0, len(loaded.candidates))
		for id := range loaded.candidates {
			ids = append(ids, id)
		}
		slices.Sort(ids)
		for _, id := range ids {
			candidate := loaded.candidates[id]
			if state.Sessions.ContainsKey(candidate.SessionID) {
				continue
			}
			rows = append(rows, helRow(candidate))
		}
	}
	for session := range state.Sessions.Values() {
		if session.State.IsActive() || state.IsSubagentSession(session.ID) {
			continue
		}
		rows = append(rows, helRow(client.ResumeCandidateOf(session, config)))
	}
	for _, profile := range profiles {
		for _, native := range profile.Sessions {
			key := nativeSessionKey{harness: profile.HarnessKind, nativeSessionID: native.NativeSessionID}
			if _, held := adopted[key]; held {
				continue
			}
			if slices.ContainsFunc(ownCheckouts, func(checkout string) bool {
				return resumePathStartsWith(native.Cwd, checkout)
			}) {
				continue
			}
			rows = append(rows, ResumeRow{
				key:               resumeRowKeyNative(profile.HarnessKind, native.NativeSessionID),
				profileID:         profile.ProfileID,
				title:             native.Title,
				origin:            nativeProjectTarget(native.ProjectDirectory),
				details:           native.Details,
				lastActivityMs:    native.LastActivityMs,
				status:            ResumeRowStatusImportable,
				nativelyArchived:  native.NativelyArchived,
				unavailableReason: native.UnavailableReason,
			})
		}
	}
	// SessionWiki rows either annotate a row already here or become archived
	// rows of their own. A row is archived only when nothing on this machine
	// still holds the session: no Mjolnir record, and no native file an
	// import could adopt.
	for rank := range wiki {
		hit := &wiki[rank]
		var native *ResumeRowKey
		if hit.NativeID != nil {
			if harness, ok := harnessOfHit(hit); ok {
				key := resumeRowKeyNative(harness, *hit.NativeID)
				native = &key
			}
		}
		listed := func(key ResumeRowKey) bool {
			return slices.ContainsFunc(rows, func(row ResumeRow) bool { return row.key == key })
		}
		var existing *ResumeRowKey
		if hit.HelSessionID != nil {
			if key := resumeRowKeyHel(*hit.HelSessionID); listed(key) {
				existing = &key
			}
		}
		if existing == nil && native != nil && listed(*native) {
			existing = native
		}
		if existing != nil {
			if index := slices.IndexFunc(rows, func(row ResumeRow) bool { return row.key == *existing }); index >= 0 {
				row := &rows[index]
				id, place := hit.ID, rank
				row.wikiMatch = &id
				row.wikiRank = &place
				if snippet, ok := snippetText(hit); ok {
					row.details = fmt.Sprintf("%s · %s", row.details, snippet)
				}
			}
			continue
		}
		if !hit.Archived {
			continue
		}
		// The index carries the profile for a Mjolnir row. Every other row
		// has only the tool that wrote it.
		profileID := hit.Tool
		if hit.Profile != nil {
			profileID = *hit.Profile
		}
		title := hit.Title
		if strings.TrimSpace(hit.Title) == "" {
			title = hit.ID
		}
		var lastActivityMs int64
		if hit.Started != nil {
			if started, ok := timestampMs(*hit.Started); ok {
				lastActivityMs = started
			}
		}
		id, place := hit.ID, rank
		rows = append(rows, ResumeRow{
			key:            resumeRowKeyArchive(hit.ID),
			profileID:      profileID,
			title:          title,
			origin:         archiveOriginOf(config, hit),
			details:        archiveDetails(hit),
			lastActivityMs: lastActivityMs,
			status:         ResumeRowStatusRestorable,
			wikiMatch:      &id,
			wikiRank:       &place,
			wikiProfile:    hit.Profile,
			wikiTarget:     hit.Target,
		})
	}
	// Newest first across the whole merged list; the key breaks ties so the
	// order is stable between incremental scan updates.
	sortResumeRowsNewestFirst(rows)
	return rows
}

// snippetText is the matching text a search returned, on one line and
// without the markers SessionWiki wraps a hit in.
func snippetText(hit *client.WikiRow) (string, bool) {
	if hit.Snippet == nil {
		return "", false
	}
	text := strings.NewReplacer("\n", " ", "\r", " ", "\u0002", "", "\u0003", "").Replace(*hit.Snippet)
	text = strings.Join(strings.Fields(text), " ")
	if text == "" {
		return "", false
	}
	return text, true
}

// archiveOriginOf is where an archived session ran, in the same shape the
// other tabs use.
//
// The index carries the real target for a Mjolnir session, so the row can
// name it the way the live session summary does. Without one there is
// nothing to go on but the project path, which is shown as a local origin.
func archiveOriginOf(config *core.Config, hit *client.WikiRow) string {
	project := sourceProjectPath(hit.Project)
	if hit.Target == nil {
		return archiveOrigin(project)
	}
	var projectPath *string
	if strings.TrimSpace(hit.Project) != "" {
		projectPath = &project
	}
	return core.TargetLabel(config, *hit.Target, projectPath)
}

// sourceProjectPath is the project a recorded directory stands for. A
// managed worktree lives at `<repository>/.mj/worktrees/<session id>/<relative>`;
// the project it works on is `<repository>/<relative>`, which is what the
// other tabs name.
func sourceProjectPath(path string) string {
	parts := resumePathComponents(path)
	marker := -1
	for index := 0; index+3 <= len(parts); index++ {
		if parts[index] == ".mj" && parts[index+1] == "worktrees" {
			marker = index
			break
		}
	}
	if marker < 0 {
		return path
	}
	return resumeJoinComponents(append(slices.Clone(parts[:marker]), parts[marker+3:]...))
}

func archiveOrigin(project string) string {
	name, ok := resumePathFileName(project)
	if !ok {
		return localOrigin
	}
	return localOrigin + "/" + name
}

func archiveDetails(hit *client.WikiRow) string {
	details := fmt.Sprintf("archived · %d messages", hit.Msgs)
	if snippet, ok := snippetText(hit); ok {
		details += " · " + snippet
	} else if hit.Preview != nil && strings.TrimSpace(*hit.Preview) != "" {
		details += " · " + strings.TrimSpace(*hit.Preview)
	}
	return details
}

// buildResumeRows is the rows the three history tabs show: the merged
// sources split by ownership, with checkpoint sizes appended, and each tab's
// hit count.
//
// The dialog has two search paths, and this is the index's. With an empty
// query a tab lists everything it owns, newest first. With a query it lists
// only the rows the index returned, in the order the index ranked them, so
// what the dialog shows and what SessionWiki found are the same thing. The
// Live tab takes the other path, DashboardState.liveResumeRows, so metadata
// matches appear immediately and conversation matches are not restricted to
// the history page.
func buildResumeRows(config *core.Config, state *core.StateState, dialog *ResumeDialog, checkpointArchiveSizes map[string]*uint64) ([]ResumeRow, [resumeTabCount]int) {
	searching := !dialog.search.IsEmpty()
	merged := mergedResumeRows(config, state, dialog.history, dialog.profiles, dialog.wiki)
	// Counted before the tab filter: the other tabs' hits are already ranked
	// here, and throwing them away is what hid where a query matched.
	var hits [resumeTabCount]int
	if searching {
		for index := range merged {
			row := &merged[index]
			if row.wikiRank == nil {
				continue
			}
			for _, tab := range []ResumeTab{ResumeTabHel, ResumeTabImport, ResumeTabArchive} {
				if tab.includes(row) {
					hits[tab.index()]++
				}
			}
		}
	}
	var rows []ResumeRow
	for _, row := range merged {
		if !dialog.tab.includes(&row) || (searching && row.wikiRank == nil) {
			continue
		}
		// The checkpoint's size is loaded in the background, so it is
		// appended here rather than folded into the pure merge. Only a
		// settled record is described by its checkpoint; on a running
		// session the size belongs to whatever it was resumed from.
		if row.key.Kind == ResumeRowKeyKindHel {
			if size, ok := checkpointArchiveSizes[row.key.ID]; ok && size != nil {
				row.details += fmt.Sprintf(" · %s", formatResourceBytes(*size))
			}
		}
		if id, ok := row.sessionID(); ok {
			if record := state.Sessions.GetShared(id); record != nil && record.LastError != nil {
				row.details += fmt.Sprintf(" · %s · d: dismiss notification", *record.LastError)
			}
		}
		rows = append(rows, row)
	}
	if searching {
		// The index ranked these; the newest-first order the merge applied
		// is not the answer's order and would hide the best hit below the
		// rest.
		slices.SortStableFunc(rows, func(left, right ResumeRow) int {
			return cmp.Compare(resumeWikiRankKey(&left), resumeWikiRankKey(&right))
		})
	}
	return rows, hits
}

// liveResumeRows is every session listed in a workspace, newest activity
// first, narrowed by the search box's text when there is any.
//
// Built here rather than in buildResumeRows because what the Sessions pane
// lists, and what each workspace is called, are this type's own knowledge.
func (d *DashboardState) liveResumeRows(dialog *ResumeDialog) []ResumeRow {
	query := strings.ToLower(strings.TrimSpace(dialog.search.String()))
	config := d.config.Get()
	var rows []ResumeRow
	for session := range d.state.Get().Sessions.Values() {
		// Use the pane's visibility owner across every workspace, including
		// stopped sessions that still need a person's attention.
		if !d.isOpenableWorkspaceSession(session) {
			continue
		}
		// The state filter reads the same attention level the Sessions
		// pane's letters read, so both narrow one list the same way. It sits
		// here, where the session id is still in hand.
		if dialog.liveState != nil && !dialog.liveState.admits(d.AttentionLevel(session.ID)) {
			continue
		}
		if query != "" &&
			!d.sessionMatchesMetadata(session, query) &&
			!strings.Contains(strings.ToLower(d.workspaceDisplayName(session.WorkspaceID)), query) &&
			!liveSearchStateMatches(dialog.liveSearch, session.ID) {
			continue
		}
		workspace := d.workspaceDisplayName(session.WorkspaceID)
		details := session.ProjectName(config)
		publication, published := session.PublicationState()
		if published {
			details += " · Publication status unknown"
		}
		// The session record's `updated_at` changes only with the record, so
		// for a running session it is often the creation time. The
		// projection's last activity is what the Sessions pane orders by
		// (I1-16).
		var lastActivityMs int64
		if at, ok := d.SessionActivityAtMs(session.ID); ok && at <= math.MaxInt64 {
			lastActivityMs = int64(at)
		} else if updated, ok := timestampMs(session.UpdatedAt); ok {
			lastActivityMs = updated
		}
		var publicationState *core.PublicationState
		if published {
			publicationState = &publication
		}
		var wikiMatch *string
		if liveSearchStateMatches(dialog.liveSearch, session.ID) {
			id := session.ID
			wikiMatch = &id
		}
		rows = append(rows, ResumeRow{
			key:            resumeRowKeyLive(session.ID),
			profileID:      session.LastProfile,
			title:          session.ListedTitle(),
			origin:         workspace,
			details:        details,
			lastActivityMs: lastActivityMs,
			status:         ResumeRowStatusRunning,
			publication:    publicationState,
			wikiMatch:      wikiMatch,
		})
	}
	// Newest first, with the key breaking ties so the order holds still
	// between rebuilds.
	sortResumeRowsNewestFirst(rows)
	return rows
}

// RebuildResumeRows rebuilds the open dialog's rows from what they are
// derived from: the Hel records, the scanned native sessions, the checkpoint
// sizes, and the newest search answer. Every mutation of those inputs calls
// this. Moving the selection only reads the rows.
func (d *DashboardState) RebuildResumeRows() {
	dialog := resumeDialog(d.mode)
	if dialog == nil {
		d.resumeRowsField = nil
		d.resumeHitCounts = make([]int, resumeTabCount)
		return
	}
	// The Live list answers immediately from state, but the other tabs' hit
	// counts still come from the index while Live is selected.
	historyRows, hits := buildResumeRows(d.config.Get(), d.state.Get(), dialog, d.checkpointArchiveSizes)
	rows := historyRows
	if dialog.tab == ResumeTabLive {
		rows = d.liveResumeRows(dialog)
	}
	kept := make([]ResumeRow, 0, len(rows))
	for _, row := range rows {
		if id, ok := row.sessionID(); ok && d.TransitionKind(id) != nil {
			continue
		}
		kept = append(kept, row)
	}
	d.resumeRowsField = kept
	d.resumeHitCounts = hits[:]
	for index := range d.resumeRowsField {
		row := &d.resumeRowsField[index]
		// Recovery is the settled record's offer; Enter on a running session
		// goes to it, so a mark inviting a recovery here would not act.
		var operation *core.MoveOperation
		switch row.key.Kind {
		case ResumeRowKeyKindHel:
			if loaded, ok := dialog.history.(HistoryLoadLoaded); ok {
				if retained, ok := loaded.Value.moves[row.key.ID]; ok {
					operation = &retained
				} else if retained, ok := d.moveOperations.Get(row.key.ID); ok {
					operation = &retained
				}
			}
		case ResumeRowKeyKindLive:
			if session := d.state.Get().Sessions.GetShared(row.key.ID); session != nil && !session.State.IsActive() {
				if retained, ok := d.moveOperations.Get(row.key.ID); ok {
					operation = &retained
				}
			}
		}
		if operation != nil && !operation.OffersRetry() {
			operation = nil
		}
		row.moveRecovery = operation
	}
	dialog.prepare(d.resumeRowsField)
	// Background state updates can remove the selected row. Repair the key
	// and index together so the form never points outside the freshly
	// rebuilt list.
	d.resyncResumeSelection()
}

// resumeRows are the rows the open dialog shows; empty when no dialog is
// open. lib.rs's field of the same name is resumeRowsField in Go.
func (d *DashboardState) resumeRows() []ResumeRow {
	return d.resumeRowsField
}

// NeedsFastTick reports whether anything on screen animates on its own and
// so needs a redraw faster than the one-second clock: loading dialogs,
// session activity, or an in-flight lifecycle transition.
func (d *DashboardState) NeedsFastTick() bool {
	dialogAnimates := false
	switch mode := d.mode.(type) {
	case ModeImporting:
		dialogAnimates = true
	case ModeTargetActions:
		dialogAnimates = mode.TargetActionsDialog != nil && mode.testing != nil
	case ModeResumeDialog:
		// The scan notice lives on the Import tab, so it animates only while
		// that tab is the one on screen.
		dialog := mode.ResumeDialog
		_, wikiPending := dialog.wikiSearch.(WikiSearchStatePending)
		_, wikiRefreshing := dialog.wikiSearch.(WikiSearchStateRefreshing)
		_, livePending := dialog.liveSearch.(LiveSearchStatePending)
		_, liveRefreshing := dialog.liveSearch.(LiveSearchStateRefreshing)
		_, historyLoading := dialog.history.(HistoryLoadLoading)
		dialogAnimates = (dialog.isScanning() && dialog.tab == ResumeTabImport) ||
			dialog.sync.Scanning() ||
			wikiPending || wikiRefreshing ||
			(dialog.tab == ResumeTabLive && (livePending || liveRefreshing)) ||
			historyLoading
	}
	if dialogAnimates {
		return true
	}
	if sync := d.sessionsText.Get().sync; sync != nil && sync.Scanning() {
		return true
	}
	if d.openingSession != nil {
		return true
	}
	operations := *d.sessionOperations.Get()
	reviews := *d.sessionReviewsField.Get()
	for _, session := range d.orderedSessions() {
		if _, ok := operations[session.ID]; ok {
			return true
		}
		if session.LastError == nil {
			switch session.State {
			case core.SessionStateProvisioning, core.SessionStateCheckpointing,
				core.SessionStateClosing, core.SessionStateDestroying:
				return true
			}
		}
		if d.sessionIsWorking(session.ID) {
			return true
		}
		if views, ok := reviews[session.ID]; ok && client.AnyWorking(resumeReviewViews(views)) {
			return true
		}
	}
	return false
}

func (d *DashboardState) resumeCommandSessionID() (string, bool) {
	dialog := resumeDialog(d.mode)
	if dialog == nil {
		return "", false
	}
	rows := d.resumeRows()
	index, ok := selectedIndex(dialog, len(rows))
	if !ok {
		return "", false
	}
	return rows[index].sessionID()
}

func (d *DashboardState) ShowResumeDialog(discoveryID uint64, profiles []ImportProfileOption) {
	d.wikiSearchGeneration++
	d.mode = ModeResumeDialog{&ResumeDialog{
		discoveryID: discoveryID,
		sync:        SyncEpisode{ID: discoveryID},
		profiles:    profiles,
		// The sessions a person is most likely looking for are the ones
		// running now, so that is the tab the dialog opens on.
		tab:           ResumeTabLive,
		search:        chat.NewTextInput(),
		form:          chat.NewDialog[ResumeFocus](),
		openedAt:      time.Now(),
		wikiRequestID: d.wikiSearchGeneration,
		wikiSearch:    WikiSearchStatePending{},
		liveSearch:    LiveSearchStateAnswered{Value: map[string]struct{}{}},
		previews:      map[string]string{},
		hits:          map[PreviewKey]client.WikiHitTranscript{},
		history:       HistoryLoadLoading{},
	}}
	d.RebuildResumeRows()
	// Record which row the initial selection lands on, so the first
	// incremental scan result cannot slide the selection out from under it.
	d.resyncResumeSelection()
	// The dialog opens on Live with nothing focused yet, so the list gets the
	// default focus EndFrame hands out; `/` or a click moves it to the search
	// box from there.
}

// SetResumeFilter seeds Open with the workspace filter before its first
// background search.
func (d *DashboardState) SetResumeFilter(query string, state *SessionStateFilter) {
	if dialog := resumeDialogMut(&d.mode); dialog != nil {
		dialog.search = chat.TextInputFromValue(query)
		if state != nil {
			filter := *state
			dialog.liveState = &filter
		} else {
			dialog.liveState = nil
		}
	}
	d.RebuildResumeRows()
	d.resyncResumeSelection()
}

// SessionRecord is one session's record: the live one from the runtime feed,
// or else a stopped one loaded on demand for the resume dialog, `mj go`, or
// an import. The feed carries only live sessions.
func (d *DashboardState) SessionRecord(sessionID string) *core.SessionRecord {
	if session := d.state.Get().Sessions.GetShared(sessionID); session != nil {
		return session
	}
	if cached := d.stoppedRecords.GetShared(sessionID); cached != nil {
		return &cached.Record
	}
	return nil
}

// SetFailedLaunchRetries replaces what new-session defaults are chosen from.
func (d *DashboardState) SetFailedLaunchRetries(sessions map[string]struct{}) {
	d.failedLaunchRetries = sessions
}

func (d *DashboardState) SetContainerUpgradeSessions(sessions map[string]struct{}) {
	d.containerUpgradeSessions = sessions
}

func (d *DashboardState) SetRecentLaunch(recent *core.RecentLaunch) {
	d.recentLaunch = recent
}

func (d *DashboardState) SetLaunchRecency(recency []client.LaunchRecency) {
	d.launchRecency = recency
}

// ResumeCheckpointArchives is the checkpoint archives of the open dialog's
// Mjolnir rows, whose sizes the rows show.
func (d *DashboardState) ResumeCheckpointArchives() core.SnapshotMap[string, string] {
	archives := core.NewSnapshotMap[string, string]()
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return archives
	}
	loaded, ok := mode.history.(HistoryLoadLoaded)
	if !ok {
		return archives
	}
	for _, candidate := range loaded.Value.candidates {
		if candidate.CheckpointArchive != nil {
			archives.Insert(candidate.SessionID, *candidate.CheckpointArchive)
		}
	}
	return archives
}

// RememberStoppedRecord keeps one stopped record that something loaded on
// demand: the session `mj go` starts in, or a session just imported.
func (d *DashboardState) RememberStoppedRecord(session client.SessionRecordWithVerdict) {
	d.stoppedRecords.Insert(session.Record.ID, session)
}

// ApplyResumeRecord opens the resume wizard on a record fetched for the row
// picked in the open dialog. An answer for a dialog that has since closed,
// or for a row no longer being opened, is dropped. record is nil when the
// session no longer exists; err is set when it could not be loaded.
func (d *DashboardState) ApplyResumeRecord(sessionID string, record *client.SessionRecordWithVerdict, err error) DashboardAction {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return DashboardActionNone{}
	}
	dialog := mode.ResumeDialog
	if dialog.opening == nil || *dialog.opening != sessionID {
		return DashboardActionNone{}
	}
	dialog.opening = nil
	switch {
	case err != nil:
		d.notices.Set(fmt.Sprintf("Could not load the session to resume: %s", err))
		return DashboardActionNone{}
	case record == nil:
		d.notices.Set("That session no longer exists.")
		return DashboardActionNone{}
	}
	d.RememberStoppedRecord(*record)
	d.CancelModal()
	return d.BeginResumeFor(sessionID)
}

// ApplyResumeCandidates installs the daemon's answer about stopped sessions
// in the open dialog. Returns whether it was installed; an answer for a
// dialog that has since closed or reopened is dropped.
func (d *DashboardState) ApplyResumeCandidates(discoveryID uint64, candidates client.ResumeCandidates, err error) bool {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return false
	}
	dialog := mode.ResumeDialog
	if dialog.discoveryID != discoveryID {
		return false
	}
	if err != nil {
		dialog.history = HistoryLoadFailed{Value: err.Error()}
	} else {
		history := resumeHistoryFrom(candidates)
		dialog.history = HistoryLoadLoaded{Value: &history}
	}
	d.RebuildResumeRows()
	d.resyncResumeSelection()
	return true
}

// ApplyResumeProfile folds one profile's scan result into the open dialog,
// keeping the selection on the same row.
func (d *DashboardState) ApplyResumeProfile(discoveryID uint64, profile ImportProfileOption) {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return
	}
	dialog := mode.ResumeDialog
	if dialog.discoveryID != discoveryID {
		return
	}
	index := slices.IndexFunc(dialog.profiles, func(candidate ImportProfileOption) bool {
		return candidate.ProfileID == profile.ProfileID
	})
	if index >= 0 && importProfileOptionEqual(&dialog.profiles[index], &profile) {
		return
	}
	// The slice may be shared with a copy of the dialog, so it is replaced
	// rather than changed in place (Rust's Arc::make_mut).
	profiles := slices.Clone(dialog.profiles)
	if index >= 0 {
		profiles[index] = profile
	} else {
		profiles = append(profiles, profile)
	}
	dialog.profiles = profiles
	d.RebuildResumeRows()
	d.resyncResumeSelection()
}

// ResumeSearchRequestID: the query remains active while a temporary overlay
// covers its dialog.
func (d *DashboardState) ResumeSearchRequestID() (uint64, bool) {
	dialog := resumeDialog(d.mode)
	if dialog == nil {
		return 0, false
	}
	return dialog.wikiRequestID, true
}

func (d *DashboardState) ApplyWikiSearch(requestID uint64, page client.WikiSearchPage) {
	d.ApplyWikiSearchResult(requestID, page, nil)
}

// ApplyResumeTextSearchResult: both searches belong to the dialog's one
// query generation; a delayed answer can never put an older query's rows or
// previews into the list.
func (d *DashboardState) ApplyResumeTextSearchResult(requestID uint64, matches []client.SessionTextMatch, err error) bool {
	dialog := resumeDialogMut(&d.mode)
	if dialog == nil || dialog.wikiRequestID != requestID {
		return false
	}
	if err != nil {
		if _, refreshing := dialog.liveSearch.(LiveSearchStateRefreshing); refreshing {
			message := err.Error()
			dialog.sync.Status = &client.DaemonResult[client.WikiSyncStatus]{Err: &message}
		}
		dialog.liveSearch = LiveSearchStateFailed{Value: err.Error()}
	} else {
		found := make(map[string]struct{}, len(matches))
		for _, hit := range matches {
			found[hit.SessionID] = struct{}{}
		}
		dialog.liveSearch = LiveSearchStateAnswered{Value: found}
	}
	d.RebuildResumeRows()
	return true
}

// ApplyWikiSearchResult: successes and failures share one admission point,
// including refreshes.
func (d *DashboardState) ApplyWikiSearchResult(requestID uint64, page client.WikiSearchPage, err error) bool {
	dialog := resumeDialogMut(&d.mode)
	if dialog == nil || dialog.wikiRequestID != requestID {
		return false
	}
	if err != nil {
		if _, refreshing := dialog.wikiSearch.(WikiSearchStateRefreshing); refreshing {
			message := err.Error()
			dialog.sync.Status = &client.DaemonResult[client.WikiSyncStatus]{Err: &message}
		}
		dialog.wikiSearch = WikiSearchStateFailed{Value: err.Error()}
	} else {
		dialog.wikiSearch = WikiSearchStateAnswered{Value: page.Status}
		if !wikiRowsEqual(dialog.wiki, page.Rows) {
			dialog.wiki = page.Rows
		}
	}
	d.RebuildResumeRows()
	return true
}

// NextWikiPreview is the preview the open dialog still needs for the row
// under its selection: the query's matching passages while a query is
// active, and the briefing otherwise. DashboardActionNone when it has it
// already, or has asked for it.
//
// Moving the selection asks for one. A search answer can also put a
// different row under an unmoved selection, and that row's transcript is
// what the preview pane is already promising, so it is asked for here.
func (d *DashboardState) NextWikiPreview() DashboardAction {
	var wikiID string
	haveRow := false
	if row := d.selectedResumeRow(); row != nil {
		wikiID, haveRow = row.wikiID()
	}
	dialog := resumeDialogMut(&d.mode)
	if dialog == nil || !haveRow {
		return DashboardActionNone{}
	}
	query, ok := dialog.activeQuery()
	if !ok {
		if _, cached := dialog.previews[wikiID]; cached ||
			(dialog.previewPending != nil && *dialog.previewPending == wikiID) {
			return DashboardActionNone{}
		}
		pending := wikiID
		dialog.previewPending = &pending
		return DashboardActionLoadArchivedBrief{WikiID: wikiID}
	}
	key := PreviewKey{WikiID: wikiID, Query: query}
	if _, cached := dialog.hits[key]; cached || (dialog.hitsPending != nil && *dialog.hitsPending == key) {
		return DashboardActionNone{}
	}
	dialog.hitsPending = &key
	return DashboardActionLoadArchivedHits{WikiID: wikiID, Query: query}
}

// ApplyWikiBrief folds one fetched briefing into the open dialog's preview
// cache.
func (d *DashboardState) ApplyWikiBrief(wikiID string, markdown string) {
	dialog := resumeDialogMut(&d.mode)
	if dialog == nil {
		return
	}
	if dialog.previewPending != nil && *dialog.previewPending == wikiID {
		dialog.previewPending = nil
	}
	previews := maps.Clone(dialog.previews)
	if previews == nil {
		previews = map[string]string{}
	}
	previews[wikiID] = markdown
	dialog.previews = previews
}

// ApplyWikiHits folds one query's matching passages into the open dialog's
// cache. A nil transcript means the index no longer holds that session,
// which the pane shows the same way as a session with no match.
func (d *DashboardState) ApplyWikiHits(wikiID string, query string, transcript *client.WikiHitTranscript) {
	dialog := resumeDialogMut(&d.mode)
	if dialog == nil {
		return
	}
	key := PreviewKey{WikiID: wikiID, Query: query}
	if dialog.hitsPending != nil && *dialog.hitsPending == key {
		dialog.hitsPending = nil
	}
	showing := dialog.previewKey != nil && *dialog.previewKey == key
	hits := maps.Clone(dialog.hits)
	if hits == nil {
		hits = map[PreviewKey]client.WikiHitTranscript{}
	}
	if transcript != nil {
		hits[key] = *transcript
	} else {
		hits[key] = client.WikiHitTranscript{}
	}
	dialog.hits = hits
	if showing {
		// The passages are what the pane is already showing, so open it on
		// the first match rather than at the top of the excerpt.
		d.focusPreviewHit(0)
	}
}

// focusPreviewHit moves the preview pane onto hit index, reporting whether
// there was such a hit. The lines are wrapped here with the width the pane
// was last drawn at, so the stored offset is the row the reader will see.
func (d *DashboardState) focusPreviewHit(index int) bool {
	var surface *chat.SurfaceFrame
	if registered := d.frameSurfaces.Surface(chat.SurfaceIdResumePreview{}); registered != nil {
		copied := *registered
		surface = &copied
	}
	dialog := resumeDialog(d.mode)
	if dialog == nil {
		return false
	}
	lines, hitLines, ok := dialog.previewBody(d.resumeRowsField)
	if !ok || index < 0 || index >= len(hitLines) {
		return false
	}
	logical := hitLines[index]
	scroll := 0
	if surface != nil {
		width := surface.Rect.Dx()
		viewport := surface.Rect.Dy()
		total := len(wrapPreviewLines(lines, width))
		scroll = min(len(wrapPreviewLines(lines[:logical], width)), max(total-viewport, 0))
	}
	// Without a surface the pane has not been drawn yet, so its width is
	// unknown; the next frame shows the excerpt from its top.
	dialog = resumeDialogMut(&d.mode)
	if dialog == nil {
		return false
	}
	dialog.previewHit = index
	dialog.previewScroll = scroll
	return true
}

// stepPreviewHit moves step hits from the one the pane is on, stopping at
// either end, and reports whether the keystroke belonged to the pane.
func (d *DashboardState) stepPreviewHit(step int) bool {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return false
	}
	dialog := mode.ResumeDialog
	_, hitLines, ok := dialog.previewBody(d.resumeRowsField)
	if !ok || len(hitLines) == 0 {
		return false
	}
	next := min(max(dialog.previewHit+step, 0), len(hitLines)-1)
	return d.focusPreviewHit(next)
}

// wikiSearchAction asks for a new search after the query changed. The
// debounce and the dropping of stale answers live with the request id, not
// with a timer here.
func (d *DashboardState) wikiSearchAction() DashboardAction {
	requestID, query, ok := d.NextWikiSearch()
	if !ok {
		return DashboardActionNone{}
	}
	return DashboardActionSearchArchivedSessions{RequestID: requestID, Query: query}
}

// NextWikiSearch claims the next search request id for the open dialog, with
// the query it should run. ok is false when no dialog is open.
func (d *DashboardState) NextWikiSearch() (requestID uint64, query string, ok bool) {
	dialog := resumeDialogMut(&d.mode)
	if dialog == nil {
		return 0, "", false
	}
	d.wikiSearchGeneration++
	dialog.wikiRequestID = d.wikiSearchGeneration
	dialog.wikiSearch = WikiSearchStatePending{}
	if strings.TrimSpace(dialog.search.Value()) == "" {
		dialog.liveSearch = LiveSearchStateAnswered{Value: map[string]struct{}{}}
	} else {
		dialog.liveSearch = LiveSearchStatePending{}
	}
	dialog.wiki = nil
	requestID, query = dialog.wikiRequestID, dialog.search.String()
	d.RebuildResumeRows()
	return requestID, query, true
}

// RefreshWikiSearch refreshes the same query without removing the results
// under the selection.
func (d *DashboardState) RefreshWikiSearch() (requestID uint64, query string, ok bool) {
	dialog := resumeDialogMut(&d.mode)
	if dialog == nil {
		return 0, "", false
	}
	d.wikiSearchGeneration++
	dialog.wikiRequestID = d.wikiSearchGeneration
	status, _ := wikiSearchStateStatus(dialog.wikiSearch)
	dialog.wikiSearch = WikiSearchStateRefreshing{Value: status}
	switch live := dialog.liveSearch.(type) {
	case LiveSearchStateAnswered:
		dialog.liveSearch = LiveSearchStateRefreshing{Value: maps.Clone(live.Value)}
	case LiveSearchStateRefreshing:
		dialog.liveSearch = LiveSearchStateRefreshing{Value: maps.Clone(live.Value)}
	default:
		dialog.liveSearch = LiveSearchStateRefreshing{Value: map[string]struct{}{}}
	}
	return dialog.wikiRequestID, dialog.search.String(), true
}

// resyncResumeSelection keeps rowIndex pointed at the selected row after the
// list changed.
func (d *DashboardState) resyncResumeSelection() {
	dialog := resumeDialog(d.mode)
	if dialog == nil {
		return
	}
	rows := d.resumeRows()
	index := -1
	if dialog.selected != nil {
		index = slices.IndexFunc(rows, func(row ResumeRow) bool { return row.key == *dialog.selected })
	}
	if index < 0 {
		index = min(dialog.rowIndex, max(len(rows)-1, 0))
	}
	var key *ResumeRowKey
	if index < len(rows) {
		selected := rows[index].key
		key = &selected
	}
	dialog.rowIndex = index
	dialog.selected = key
	dialog.prepare(d.resumeRowsField)
	d.syncResumePreviewKey()
}

// syncResumePreviewKey keeps the preview's scroll offset with the excerpt it
// was made for. A different transcript, or the same one under a new query,
// opens at its top and then on its first hit.
func (d *DashboardState) syncResumePreviewKey() {
	dialog := resumeDialog(d.mode)
	if dialog == nil {
		return
	}
	var key *PreviewKey
	if wikiID, ok := dialog.previewWikiID(d.resumeRowsField); ok {
		query, _ := dialog.activeQuery()
		key = &PreviewKey{WikiID: wikiID, Query: query}
	}
	if (dialog.previewKey == nil) == (key == nil) && (key == nil || *dialog.previewKey == *key) {
		return
	}
	dialog.previewKey = key
	dialog.previewScroll = 0
	dialog.previewHit = 0
	d.focusPreviewHit(0)
}

// switchResumeTab moves the dialog to one tab, and answers with what the new
// tab needs fetched: the arrow keys and a click on the strip both come
// through here.
//
// The Live tab answers its own list immediately, while its search also
// updates the index results for the other tabs in the background.
func (d *DashboardState) switchResumeTab(tab ResumeTab) DashboardAction {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return DashboardActionNone{}
	}
	dialog := mode.ResumeDialog
	if dialog.tab != tab {
		dialog.tab = tab
		dialog.selected = nil
		dialog.rowIndex = 0
		d.RebuildResumeRows()
		d.resyncResumeSelection()
		d.settleResumeFocus()
	}
	return d.NextWikiPreview()
}

// settleResumeFocus puts the keyboard where the new tab's keys work: on the
// list, or on the tab strip when the list has no row to hold the focus.
//
// Without this a tab reached by arrow leaves the focus on the strip, where
// Enter is spent moving to the list rather than opening the selected row,
// and an empty list hands the focus to whichever button comes next. A person
// typing in the search box keeps it.
func (d *DashboardState) settleResumeFocus() {
	target := d.resumeFocusOutsideSearch()
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return
	}
	form := &mode.form
	if form.IsFocused(ResumeFocusSearch) {
		return
	}
	form.Focus(target)
}

// resumeFocusOutsideSearch is where the keyboard belongs once it leaves the
// search box: the list, or the tab strip when the list has no row that could
// take the focus. Asking a disabled list for it would leave the box holding
// the keyboard instead.
func (d *DashboardState) resumeFocusOutsideSearch() ResumeFocus {
	if len(d.resumeRows()) == 0 {
		return ResumeFocusTabs
	}
	return ResumeFocusSessions
}

// stepResumeTab moves the dialog step tabs along the strip, wrapping around
// it.
func (d *DashboardState) stepResumeTab(step int) DashboardAction {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return DashboardActionNone{}
	}
	next := resumeTabFromIndex((mode.tab.index() + step) % resumeTabCount)
	return d.switchResumeTab(next)
}

// clearResumeSearch empties the search box and puts the unfiltered list
// back, the same rebuild deleting the last character would do.
func (d *DashboardState) clearResumeSearch() DashboardAction {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return DashboardActionNone{}
	}
	mode.search.Clear()
	action := d.wikiSearchAction()
	d.selectResumeRow(0)
	return action
}

func (d *DashboardState) selectResumeRow(index int) {
	var key *ResumeRowKey
	if rows := d.resumeRows(); index >= 0 && index < len(rows) {
		selected := rows[index].key
		key = &selected
	}
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return
	}
	dialog := mode.ResumeDialog
	dialog.rowIndex = index
	dialog.selected = key
	dialog.prepare(d.resumeRowsField)
	d.syncResumePreviewKey()
}

// selectedResumeRow is a copy of the row the open dialog points at, or nil.
func (d *DashboardState) selectedResumeRow() *ResumeRow {
	dialog := resumeDialog(d.mode)
	if dialog == nil {
		return nil
	}
	rows := d.resumeRows()
	index, ok := selectedIndex(dialog, len(rows))
	if !ok {
		return nil
	}
	row := rows[index]
	return &row
}

// scrollResumePreview moves the preview pane when a wheel notch lands on it
// or a page key is pressed with the list focused, and reports whether it
// took the event.
//
// The pane registers a scrollable surface every frame, so the last frame's
// registration carries both the hitbox and the wrapped line count the offset
// clamps against.
func (d *DashboardState) scrollResumePreview(event tea.Msg, focused ResumeFocus) bool {
	showing := false
	// The registration is last frame's, so a selection that has just moved
	// to a row without a transcript must not keep the pane's keys and
	// hitbox until the next frame.
	if mode, ok := d.mode.(ModeResumeDialog); ok {
		showing = mode.hasPreview(d.resumeRowsField)
	}
	if !showing {
		return false
	}
	registered := d.frameSurfaces.Surface(chat.SurfaceIdResumePreview{})
	if registered == nil {
		return false
	}
	surface := *registered
	viewport := surface.Rect.Dy()
	var step int
	switch event := event.(type) {
	case tea.MouseWheelMsg:
		mouse := event.Mouse()
		if !rectContains(surface.Rect, mouse.X, mouse.Y) {
			return false
		}
		switch mouse.Button {
		case tea.MouseWheelUp:
			step = -previewScrollRows
		case tea.MouseWheelDown:
			step = previewScrollRows
		default:
			return false
		}
	case tea.KeyPressMsg:
		if event.IsRepeat || resumeKeyModifiers(event.Mod) != 0 || focused != ResumeFocusSessions {
			return false
		}
		switch chat.KeyPressCode(event) {
		case tea.KeyPgUp:
			step = -max(viewport, 1)
		case tea.KeyPgDown:
			step = max(viewport, 1)
		default:
			return false
		}
	default:
		return false
	}
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return false
	}
	dialog := mode.ResumeDialog
	maximum := max(surface.TotalRows-viewport, 0)
	next := min(max(dialog.previewScroll+step, 0), maximum)
	moved := next != dialog.previewScroll
	dialog.previewScroll = next
	// The gesture belongs to the pane whether or not it had room to move, so
	// a wheel at the end of the text does not fall through to the list.
	d.lastEventConsumed = moved
	return true
}

func (d *DashboardState) resumeScrollbarHandlesMouse(mouse tea.MouseMsg) bool {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return false
	}
	pointer := mode.previewScrollbar
	if pointer.IsDragging() {
		return true
	}
	click, ok := mouse.(tea.MouseClickMsg)
	if !ok || click.Button != tea.MouseLeft {
		return false
	}
	geometry, ok := pointer.Geometry()
	return ok && geometry.MaxScroll > 0 &&
		click.X == geometry.Track.Min.X &&
		click.Y >= geometry.Track.Min.Y &&
		click.Y < geometry.Track.Max.Y
}

func (d *DashboardState) handleResumeScrollbarMouse(event tea.Msg) bool {
	mouse, ok := event.(tea.MouseMsg)
	if !ok {
		return false
	}
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return false
	}
	dialog := mode.ResumeDialog
	switch outcome := dialog.previewScrollbar.HandleMouse(mouse).(type) {
	case chat.ScrollbarPointerConsumed:
		return true
	case chat.ScrollbarPointerScrollTo:
		dialog.previewScroll = outcome.Offset
		return true
	}
	return false
}

func (d *DashboardState) handleResumeDialogEvent(event tea.Msg) DashboardAction {
	mode, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return DashboardActionNone{}
	}
	focused := mode.focused()
	if press, ok := event.(tea.KeyPressMsg); ok && !press.IsRepeat &&
		chat.KeyPressCode(press) == 'd' && resumeKeyModifiers(press.Mod) == 0 &&
		focused != ResumeFocusSearch {
		return d.beginDismissNotification()
	}
	if d.handleResumeScrollbarMouse(event) {
		return DashboardActionNone{}
	}
	if d.scrollResumePreview(event, focused) {
		return DashboardActionNone{}
	}
	// Moving between the query's matches, with the list focused. `N` comes
	// with Shift, so these cannot sit under the modifier-free block below.
	if press, ok := event.(tea.KeyPressMsg); ok && !press.IsRepeat && focused == ResumeFocusSessions {
		modifiers := resumeKeyModifiers(press.Mod)
		if modifiers == 0 || modifiers == tea.ModShift {
			step := 0
			switch chat.KeyPressCode(press) {
			case 'n', ']':
				step = 1
			case 'N', '[':
				step = -1
			}
			if step != 0 && d.stepPreviewHit(step) {
				d.lastEventConsumed = true
				return DashboardActionNone{}
			}
		}
	}
	mode, ok = d.mode.(ModeResumeDialog)
	if !ok {
		return DashboardActionNone{}
	}
	dialog := mode.ResumeDialog
	if press, ok := event.(tea.KeyPressMsg); ok && !press.IsRepeat && resumeKeyModifiers(press.Mod) == 0 {
		code := chat.KeyPressCode(press)
		// Escape peels one layer at a time: the query first, then the box's
		// hold on the keyboard, and only then the dialog. This is the order
		// the Sessions pane's own filter already uses.
		if code == tea.KeyEscape {
			if !dialog.search.IsEmpty() {
				return d.clearResumeSearch()
			}
			if focused == ResumeFocusSearch {
				target := d.resumeFocusOutsideSearch()
				if mode, ok := d.mode.(ModeResumeDialog); ok {
					mode.form.Focus(target)
				}
				return DashboardActionNone{}
			}
		}
		if focused == ResumeFocusSearch {
			if code == tea.KeyDown {
				dialog.form.Focus(ResumeFocusSessions)
				return DashboardActionNone{}
			}
			// Readline first: the arrows walk the caret through the query,
			// and reach the tab strip only when pressed against the end the
			// caret is already sitting on.
			if step, ok := tabStep(code); ok && dialog.searchCaretAtEdge(code) {
				return d.stepResumeTab(step)
			}
		} else {
			// Outside the box the strip is the dialog's left-to-right axis,
			// so the arrows reach it from the list, the strip and the
			// buttons alike, including from a list too empty to hold focus.
			if step, ok := tabStep(code); ok {
				return d.stepResumeTab(step)
			}
			switch {
			case code == '/':
				// A disabled box cannot take the focus, and asking for it
				// would leave the focus pending until it can.
				if dialog.searchEnabled() {
					dialog.form.Focus(ResumeFocusSearch)
				}
				return DashboardActionNone{}
			case code == tea.KeyDelete && focused == ResumeFocusSessions:
				return d.destroySelectedResumeRow()
			// The state letters narrow what is running, so they belong to
			// the Live tab alone: the other tabs list sessions that have no
			// current state, where every letter but `a` would match nothing.
			case chat.IsCharKeyCode(code) && dialog.tab == ResumeTabLive:
				if state, ok := sessionStateFilterFromLetter(code); ok {
					dialog.liveState = state
					d.RebuildResumeRows()
					d.selectResumeRow(0)
					return DashboardActionNone{}
				}
			}
		}
	}
	result := dialog.form.Handle(event)
	d.lastEventConsumed = result.Consumed
	switch interaction := result.Action.(type) {
	case chat.InteractionCancel[ResumeFocus]:
		d.CancelModal()
	case chat.InteractionEdit[ResumeFocus]:
		if interaction.ID == ResumeFocusSearch {
			chat.TextFieldApply(&dialog.search, interaction.Edit)
			action := d.wikiSearchAction()
			d.selectResumeRow(0)
			return action
		}
	case chat.InteractionSelect[ResumeFocus]:
		switch interaction.ID {
		case ResumeFocusTabs:
			return d.switchResumeTab(resumeTabFromIndex(interaction.Index))
		case ResumeFocusSessions:
			d.selectResumeRow(interaction.Index)
			return d.NextWikiPreview()
		}
	case chat.InteractionActivate[ResumeFocus]:
		switch interaction.ID {
		case ResumeFocusCancel:
			d.CancelModal()
		case ResumeFocusPreviousHit, ResumeFocusNextHit:
			step := -1
			if interaction.ID == ResumeFocusNextHit {
				step = 1
			}
			d.stepPreviewHit(step)
			if mode, ok := d.mode.(ModeResumeDialog); ok {
				mode.form.Focus(ResumeFocusSessions)
			}
			return DashboardActionNone{}
		// Enter on a query acts on its selected match, the same as Enter
		// on the list. With nothing matched it only hands the list the
		// focus.
		case ResumeFocusSearch:
			if row := d.selectedResumeRow(); row != nil {
				return d.activateSelectedResumeRow(row)
			}
			if mode, ok := d.mode.(ModeResumeDialog); ok {
				mode.form.Focus(ResumeFocusSessions)
			}
		case ResumeFocusTabs:
			dialog.form.Focus(ResumeFocusSessions)
		case ResumeFocusSessions, ResumeFocusOpen:
			return d.activateSelectedResumeRow(d.selectedResumeRow())
		case ResumeFocusDestroy:
			return d.destroySelectedResumeRow()
		case ResumeFocusCopyID:
			if row := d.selectedResumeRow(); row != nil &&
				row.unavailableReason != nil && row.key.Kind == ResumeRowKeyKindNative {
				return DashboardActionCopyNativeSessionId{NativeSessionID: row.key.ID}
			}
		}
	}
	return DashboardActionNone{}
}

// destroySelectedResumeRow asks for confirmation before destroying the
// selected row's session record.
func (d *DashboardState) destroySelectedResumeRow() DashboardAction {
	row := d.selectedResumeRow()
	if row == nil {
		return DashboardActionNone{}
	}
	// Destroy here removes a settled record. Ending a session that is still
	// running has its own confirmations, and they belong to the pane that
	// owns the session.
	if row.key.Kind == ResumeRowKeyKindLive {
		d.notices.Set("Stop or delete a running session from the Sessions pane.")
		return DashboardActionNone{}
	}
	sessionID, ok := row.sessionID()
	if !ok {
		d.notices.Set("Mjolnir never destroys a harness's own session.")
		return DashboardActionNone{}
	}
	d.CancelComponentPointer()
	mode, ok := d.mode.(ModeResumeDialog)
	d.mode = ModeDashboard{}
	if !ok {
		return DashboardActionNone{}
	}
	// The row is named as listed; the dashboard may not hold the record.
	confirm := newConfirmDialog(ConfirmationDestroyStopped{
		SessionID: sessionID,
		Reopen:    mode.ResumeDialog,
	}).namingSession(row.title)
	d.mode = ModeConfirm{&confirm}
	d.RebuildResumeRows()
	return DashboardActionNone{}
}

func (d *DashboardState) activateSelectedResumeRow(row *ResumeRow) DashboardAction {
	if row == nil {
		return DashboardActionNone{}
	}
	if reason, ok := row.status.explanation(); ok {
		d.notices.Set(unresumableNotice(reason))
		return DashboardActionNone{}
	}
	if row.unavailableReason != nil {
		action := "resume"
		if row.key.Kind == ResumeRowKeyKindNative {
			action = "import"
		}
		d.notices.Set(fmt.Sprintf("Cannot %s: %s", action, *row.unavailableReason))
		return DashboardActionNone{}
	}
	if row.moveRecovery != nil {
		confirm := newConfirmDialog(ConfirmationRecoverMove{Operation: *row.moveRecovery})
		d.mode = ModeConfirm{&confirm}
		return DashboardActionNone{}
	}
	switch row.key.Kind {
	case ResumeRowKeyKindLive:
		sessionID := row.key.ID
		session := d.state.Get().Sessions.GetShared(sessionID)
		if session == nil {
			d.notices.Set("That session is no longer running.")
			return DashboardActionNone{}
		}
		workspaceID, active := session.WorkspaceID, session.State.IsActive()
		d.CancelModal()
		if !active {
			return d.beginResumeInWorkspace(sessionID, workspaceID)
		}
		return d.focusSessionAnywhere(workspaceID, sessionID)
	case ResumeRowKeyKindHel:
		sessionID := row.key.ID
		if d.SessionRecord(sessionID) == nil {
			// The dialog lists rows, not records. Fetch the one picked and
			// open the wizard when it arrives.
			if mode, ok := d.mode.(ModeResumeDialog); ok {
				opening := sessionID
				mode.opening = &opening
			}
			return DashboardActionLoadResumeRecord{SessionID: sessionID}
		}
		d.CancelModal()
		return d.BeginResumeFor(sessionID)
	case ResumeRowKeyKindArchive:
		d.CancelModal()
		return d.beginArchiveRestore(row.key.ID, row.title, row.wikiProfile, row.wikiTarget)
	default:
		d.CancelModal()
		return DashboardActionImportSession{
			ProfileID:       row.profileID,
			NativeSessionID: row.key.ID,
			DisplayTitle:    row.title,
		}
	}
}

// RowLayout is the column widths for the row text, derived from the pane
// width.
type RowLayout struct {
	title    int
	profile  int
	origin   int
	activity int
}

// minTitleCells is room for a short title and the longest status mark,
// `  [unavailable]`.
const minTitleCells = 20

func rowLayout(width int, tab ResumeTab) RowLayout {
	// The Live tab has no profile column, so the title also gets back the
	// two-space gap that would have separated it from the origin cell.
	profile := 0
	if tab != ResumeTabLive {
		profile = max(min(14, width/5), 6)
	}
	origin := max(min(24, width/3), 8)
	activity := max(min(14, width/4), 8)
	gaps := 8
	if profile == 0 {
		gaps = 6
	}
	// The title keeps room for a status mark such as "[unavailable]". On a
	// narrow list the other columns give up cells for it, down to their
	// minimums, instead of pushing the mark past the list's edge.
	short := max(profile+origin+activity+gaps+minTitleCells-width, 0)
	for _, column := range []struct {
		cells   *int
		minimum int
	}{{&origin, 8}, {&profile, 6}, {&activity, 8}} {
		if *column.cells == 0 {
			continue
		}
		give := min(short, max(*column.cells-column.minimum, 0))
		*column.cells -= give
		short -= give
	}
	reserved := profile + origin + activity + gaps
	return RowLayout{
		title:    max(width-reserved, 10),
		profile:  profile,
		origin:   origin,
		activity: activity,
	}
}

func nativeProjectTarget(projectDirectory string) string {
	name, ok := resumePathFileName(projectDirectory)
	if !ok {
		return localOrigin
	}
	return localOrigin + "/" + name
}

// resumePathFileName is Rust's `Path::file_name`: the last normal component,
// ignoring trailing separators and `.` components; false when the path ends
// in `..`, a root, or nothing.
func resumePathFileName(path string) (string, bool) {
	parts := strings.Split(filepath.ToSlash(path[len(filepath.VolumeName(path)):]), "/")
	for index := len(parts) - 1; index >= 0; index-- {
		switch parts[index] {
		case "", ".":
			continue
		case "..":
			return "", false
		default:
			return parts[index], true
		}
	}
	return "", false
}

// previewMinimumHeight is the least room the preview pane is worth drawing
// in: a border plus three rows of text, enough to show text past the
// briefing's blank second line. Below this the preview is hidden rather than
// drawn empty.
const previewMinimumHeight = 5

// previewRoom is the rows left for the preview once the fixed bands and the
// list's five session rows have their share.
func previewRoom(inner uv.Rectangle) int {
	return max(inner.Dy()-(resumeFixedRows+listMinimumRows+listChromeRows), 0)
}

// previewFits reports whether the dialog has room for a preview beside a
// list of five session rows. The list outranks the transcript: a terminal
// too short for both shows only the list, never an empty bordered box.
func previewFits(inner uv.Rectangle) bool {
	return previewRoom(inner) >= previewMinimumHeight
}

// previewHeight is how tall the preview pane under the list is when it
// fits: two fifths of the body, capped so the list keeps listMinimumRows
// session rows.
func previewHeight(inner uv.Rectangle) int {
	return max(min(inner.Dy()*2/5, previewRoom(inner)), previewMinimumHeight)
}

const (
	// listPanelFloor is the least the list panel keeps on a very short
	// terminal: its border, its header and two sessions. The fixed bands
	// around it stay on screen.
	listPanelFloor = 5
	// listChromeRows is the rows the list panel spends on its border and
	// column header.
	listChromeRows = 3
	// listMinimumRows is the session rows the list keeps whenever the
	// terminal is tall enough.
	listMinimumRows = 5
	// resumeFixedRows is the rows every band but the list and the preview
	// takes: tabs, search, footer and the four gaps between them and the
	// preview.
	resumeFixedRows = 10
	// resumeDialogHeight is the dialog's requested height: the full list, the
	// preview's minimum and the fixed bands, plus the dialog border and
	// padding.
	resumeDialogHeight = 28
)

// resumeBands is the dialog body split into its bands: tabs, search, list,
// preview, footer. The preview band is present only when the dialog has
// something to show in it, so the list keeps the whole height when it does
// not.
func resumeBands(inner uv.Rectangle, preview bool) []uv.Rectangle {
	preview = preview && previewFits(inner)
	constraints := []chat.Constraint{
		chat.ConstraintLength(1), chat.ConstraintLength(1), // tabs, gap
		chat.ConstraintLength(1), chat.ConstraintLength(1), // search, gap
		chat.ConstraintMin(listPanelFloor), chat.ConstraintLength(1), // list, gap
	}
	if preview {
		constraints = append(constraints, chat.ConstraintLength(previewHeight(inner)), chat.ConstraintLength(1))
	}
	constraints = append(constraints, chat.ConstraintLength(4))
	bands := chat.VerticalLayout(constraints...).Split(inner)
	var stepped []uv.Rectangle
	for index := 0; index < len(bands); index += 2 {
		stepped = append(stepped, bands[index])
	}
	return stepped
}

func resumeSessionsPane(area uv.Rectangle, preview bool) uv.Rectangle {
	popup := centeredRect(84, resumeDialogHeight, area)
	return resumeBands(chat.DialogShellPaddedInner(popup), preview)[2]
}

func renderResumeDialog(frame *chat.Frame, area uv.Rectangle, dashboard *DashboardState, dialog *ResumeDialog, surfaces *chat.FrameSurfaces) {
	popup := centeredModal(frame, surfaces, 84, resumeDialogHeight, area)
	inner := chat.DialogShellPaddedInner(popup)
	previewLines, hitLines, hasPreview := dialog.previewBody(dashboard.resumeRows())
	hasPreview = hasPreview && previewFits(inner)
	rows := resumeBands(inner, hasPreview)
	// The footer is the last band whether or not a preview sits above it.
	footerBand := rows[len(rows)-1]

	form := &dialog.form
	form.BeginFrame()
	titleLine := dismissibleModalTitle(&form.Form, popup, "Sessions", chat.Title(true), true)
	frame.RenderWidget(chat.Modal().WithTitle(titleLine), popup)
	tabLabels := resumeTabLabels(dashboard, dialog)
	chat.TabStripRender(frame, rows[0], tabLabels, dialog.tab.index(), &form.Form, ResumeFocusTabs)
	searchFocused := form.IsFocused(ResumeFocusSearch)
	searchArea := uv.Rect(rows[1].Min.X, rows[1].Min.Y, rows[1].Dx(), min(rows[1].Dy(), 1))
	labelWidth := min(8, searchArea.Dx())
	frame.RenderWidget(
		chat.RawLine("Search: "),
		uv.Rect(searchArea.Min.X, searchArea.Min.Y, labelWidth, searchArea.Dy()),
	)
	fieldArea := uv.Rect(
		searchArea.Min.X+labelWidth,
		searchArea.Min.Y,
		searchArea.Dx()-labelWidth,
		searchArea.Dy(),
	)
	// An incompatible index cannot answer history searches.
	if placeholder, ok := dialog.searchPlaceholder(); ok {
		form.Register(ResumeFocusSearch, chat.ControlKindTextField{}, fieldArea, false)
		frame.RenderWidget(
			chat.NewParagraph(chat.TextFrom(chat.StyledLine(
				truncateToCells(placeholder, fieldArea.Dx(), TruncateSummary),
				chat.Style{}.Foreground(chat.ThemePalette().Muted),
			))),
			fieldArea,
		)
	} else {
		chat.TextFieldRender(frame, fieldArea, &dialog.search, &form.Form, ResumeFocusSearch)
	}
	listRows := dashboard.resumeRows()
	sessionsFocused := form.IsFocused(ResumeFocusSessions)
	block := chat.Panel(sessionsFocused || searchFocused).WithTitle(resumeListTitle(
		dashboard,
		dialog,
		len(listRows),
		rows[2].Dx(),
	))
	listArea := block.Inner(rows[2])
	// Registered after the dialog body so a drag over the rows selects the
	// list rather than the popup around it.
	surfaces.Push(chat.SurfaceFrameFixed(chat.SurfaceIdResumeList{}, listArea))
	frame.RenderWidget(block, rows[2])
	tableRows := chat.VerticalLayout(chat.ConstraintLength(1), chat.ConstraintMin(1)).Split(listArea)
	headerArea := uv.Rect(
		tableRows[0].Min.X+2,
		tableRows[0].Min.Y,
		max(tableRows[0].Dx()-2, 0),
		tableRows[0].Dy(),
	)
	listArea = tableRows[1]
	layout := rowLayout(max(listArea.Dx()-2, 0), dialog.tab)
	frame.RenderWidget(chat.NewParagraph(chat.TextFrom(resumeHeaderLine(&layout, dialog.tab))), headerArea)
	now := displayNow()
	if len(listRows) == 0 {
		var message string
		searchEmpty := dialog.search.IsEmpty()
		switch {
		case dialog.tab == ResumeTabImport && dialog.isScanning():
			message = "Scanning native sessions…"
		// Under a state filter the filter emptied the list, so say which.
		case dialog.tab == ResumeTabLive && searchEmpty:
			if dialog.liveState != nil {
				message = fmt.Sprintf("No %s sessions", dialog.liveState.label())
			} else {
				message = "No live sessions"
			}
		case dialog.tab == ResumeTabHel && searchEmpty:
			message = "No stopped Mjolnir sessions"
		case dialog.tab == ResumeTabImport && searchEmpty:
			message = "No importable sessions"
		case dialog.tab == ResumeTabArchive && searchEmpty:
			message = "No archived sessions"
		default:
			message = emptySearchMessage(dashboard, dialog)
		}
		frame.RenderWidget(chat.RawLine(message), listArea)
		form.Register(ResumeFocusSessions, chat.ControlKindChoiceList{Len: 0, Selected: 0}, listArea, false)
	} else {
		items := make([]chat.Line, len(listRows))
		for index := range listRows {
			items[index] = resumeRowLine(&listRows[index], &layout, now)
		}
		chat.ChoiceListRender(frame, listArea, items, dialog.rowIndex, &form.Form, ResumeFocusSessions)
	}
	renderSessionScrollbar(
		frame,
		rows[2],
		len(listRows),
		form.ListOffset(ResumeFocusSessions),
		max(listArea.Dy(), 1),
	)

	if hasPreview {
		previewBand := rows[3]
		_, active := dialog.activeQuery()
		var title string
		switch hits := len(hitLines); {
		case !active:
			title = " Archived transcript "
		case hits == 0:
			title = " Transcript · no hits "
		default:
			title = fmt.Sprintf(" Transcript · hit %d/%d ", min(dialog.previewHit, hits-1)+1, hits)
		}
		previewBlock := chat.Panel(false).WithTitle(chat.RawLine(title))
		body := previewBlock.Inner(previewBand)
		frame.RenderWidget(previewBlock, previewBand)
		if len(hitLines) > 0 && previewBand.Dx() >= 30 {
			for _, button := range []struct {
				id        ResumeFocus
				label     string
				fromRight int
				enabled   bool
			}{
				{ResumeFocusPreviousHit, "↑", 8, dialog.previewHit > 0},
				{ResumeFocusNextHit, "↓", 4, dialog.previewHit+1 < len(hitLines)},
			} {
				buttonArea := uv.Rect(previewBand.Max.X-button.fromRight, previewBand.Min.Y, 3, 1)
				form.Register(button.id, chat.ControlKindButton{}, buttonArea, button.enabled)
				var style chat.Style
				switch {
				case !button.enabled:
					style = chat.Muted()
				case form.IsFocused(button.id) || form.IsArmed(button.id):
					style = chat.FocusControl()
				default:
					style = chat.Actionable()
				}
				frame.RenderWidget(
					chat.NewParagraph(chat.RawText(fmt.Sprintf("[%s]", button.label))).WithStyle(style),
					buttonArea,
				)
			}
		}
		// Wrapped here rather than by the paragraph, so the scroll offset,
		// the clamp and the scrollbar all count the same rows.
		wrapped := wrapPreviewLines(previewLines, body.Dx())
		viewport := body.Dy()
		length := len(wrapped)
		offset := min(dialog.previewScroll, max(length-viewport, 0))
		surfaces.Push(chat.SurfaceFrameScrollable(
			chat.SurfaceIdResumePreview{},
			body,
			offset,
			max(length, viewport),
		))
		frame.RenderWidget(
			chat.NewParagraph(chat.TextFrom(wrapped...)).WithScroll(min(offset, math.MaxUint16), 0),
			body,
		)
		renderSessionScrollbar(frame, previewBand, length, offset, max(viewport, 1))
		dialog.previewScrollbar.SetGeometry(sessionScrollbarGeometry(
			previewBand,
			length,
			offset,
			max(viewport, 1),
		))
	} else {
		dialog.previewScrollbar.Clear()
	}
	var selected *ResumeRow
	if index, ok := selectedIndex(dialog, len(listRows)); ok {
		selected = &listRows[index]
	}
	var footer []chat.Line
	if selected != nil {
		text := selected.details
		if id, ok := selected.sessionID(); ok {
			if _, dismissible := dashboard.notificationDismissalTarget(id); dismissible {
				text = fmt.Sprintf("d: dismiss notification · %s", selected.details)
			}
		}
		footer = append(footer, chat.StyledLine(
			truncateToCells(text, footerBand.Dx(), TruncateSummary),
			chat.Style{}.Foreground(chat.ThemePalette().Muted),
		))
	}
	if errors := dialog.errors(); dialog.tab == ResumeTabImport && len(errors) > 0 {
		footer = append(footer, chat.StyledLine(
			truncateToCells(fmt.Sprintf("Scan failed for %s", errors[0]), footerBand.Dx(), TruncateSummary),
			chat.Style{}.Foreground(chat.ThemePalette().Warning),
		))
	}
	buttons := []chat.ButtonSpec[ResumeFocus]{{ID: ResumeFocusCancel, Label: "Cancel", Enabled: true}}
	var unavailableImport *string
	if selected != nil && selected.key.Kind == ResumeRowKeyKindNative {
		unavailableImport = selected.unavailableReason
	}
	if unavailableImport != nil {
		buttons = append(buttons, chat.ButtonSpec[ResumeFocus]{ID: ResumeFocusCopyID, Label: "Copy session ID", Enabled: true})
	}
	importLabel := "Import"
	if unavailableImport != nil {
		if *unavailableImport == "missing Git repo" {
			importLabel = "Cannot import: missing Git repo"
		} else {
			importLabel = "Cannot import"
		}
	}
	if dialog.tab == ResumeTabHel {
		buttons = append(buttons, chat.ButtonSpec[ResumeFocus]{
			ID:      ResumeFocusDestroy,
			Label:   "Destroy",
			Enabled: dialog.canDestroy(listRows),
		})
	}
	var openLabel string
	switch dialog.tab {
	case ResumeTabLive:
		openLabel = "Open"
	case ResumeTabHel:
		openLabel = "Resume"
	case ResumeTabImport:
		openLabel = importLabel
	default:
		openLabel = "Restore"
	}
	buttons = append(buttons, chat.ButtonSpec[ResumeFocus]{
		ID:      ResumeFocusOpen,
		Label:   openLabel,
		Enabled: dialog.canOpen(listRows),
	})
	// Keep the unavailable explanation visible when the action row would
	// overflow.
	splitActions := unavailableImport != nil && footerBand.Dx() < 68
	actionHeight := 1
	if splitActions {
		actionHeight = 2
	}
	footer = chat.WrapLines(footer, footerBand.Dx())
	noteHeight := min(len(footer), max(footerBand.Dy()-(actionHeight+1), 0))
	noteArea := uv.Rect(
		footerBand.Min.X,
		max(footerBand.Max.Y-(actionHeight+1+noteHeight), 0),
		footerBand.Dx(),
		noteHeight,
	)
	frame.RenderWidget(
		chat.NewParagraph(chat.TextFrom(footer...)).WithAlignment(chat.AlignmentCenter),
		noteArea,
	)
	buttonHeight := 0
	if footerBand.Dy() > 0 {
		buttonHeight = 1
	}
	buttonArea := uv.Rect(
		footerBand.Min.X,
		max(footerBand.Max.Y-actionHeight, 0),
		footerBand.Dx(),
		buttonHeight,
	)
	if splitActions {
		form.RenderActions(frame, buttonArea, buttons[:2])
		explanationArea := uv.Rect(
			buttonArea.Min.X,
			buttonArea.Min.Y+1,
			buttonArea.Dx(),
			buttonArea.Dy(),
		)
		form.RenderActions(frame, explanationArea, buttons[2:])
	} else {
		form.RenderActions(frame, buttonArea, buttons)
	}
	form.EndFrame(ResumeFocusSessions)
}

// resumeTabLabels is the tab labels, carrying the scan progress on Import
// and, while a query is active, how many rows each tab matched.
func resumeTabLabels(dashboard *DashboardState, dialog *ResumeDialog) []string {
	searching := !dialog.search.IsEmpty()
	scanned, total := dialog.scanProgress()
	labels := make([]string, 0, resumeTabCount)
	for _, tab := range []struct {
		tab  ResumeTab
		name string
	}{
		{ResumeTabLive, "Live"},
		{ResumeTabHel, "Mjolnir"},
		{ResumeTabImport, "Import"},
		{ResumeTabArchive, "Archived"},
	} {
		label := " " + tab.name
		// History counts belong to the limited index page. Live also has
		// immediate metadata matches and a separate conversation answer.
		if searching && tab.tab != ResumeTabLive {
			label += fmt.Sprintf(" · %d", dashboard.resumeHitCount(tab.tab))
		}
		if tab.tab == ResumeTabImport && dialog.isScanning() {
			label += fmt.Sprintf(" · scanning %d/%d", scanned, total)
		}
		label += " "
		labels = append(labels, label)
	}
	return labels
}

// resumeListTitle is the list panel's title: what the tab lists, how the
// current search is going, and, on Import, whether the native scan is still
// running.
func resumeListTitle(dashboard *DashboardState, dialog *ResumeDialog, rows int, width int) chat.Line {
	spinner := dashboard.config.Get().Spinner
	elapsed := uint64(time.Since(dialog.openedAt).Milliseconds())
	spans := []chat.Span{chat.RawSpan(" ")}
	_, wikiRefreshing := dialog.wikiSearch.(WikiSearchStateRefreshing)
	_, liveRefreshing := dialog.liveSearch.(LiveSearchStateRefreshing)
	_, wikiFailed := dialog.wikiSearch.(WikiSearchStateFailed)
	_, liveFailed := dialog.liveSearch.(LiveSearchStateFailed)
	_, syncLabelled := dialog.sync.Label()
	refreshing := (wikiRefreshing || liveRefreshing) && !wikiFailed && !liveFailed && !syncLabelled
	historyTab := dialog.tab == ResumeTabHel || dialog.tab == ResumeTabImport
	var historyNotice []chat.Span
	if historyTab {
		switch history := dialog.history.(type) {
		case HistoryLoadLoading:
			historyNotice = []chat.Span{
				chat.CompactSpan(spinner, elapsed),
				chat.RawSpan(" Loading suspended sessions…"),
			}
		case HistoryLoadFailed:
			historyNotice = []chat.Span{chat.RawSpan(fmt.Sprintf("Could not list suspended sessions: %s", history.Value))}
		}
	}
	switch {
	case historyNotice != nil:
		spans = append(spans, historyNotice...)
	case refreshing:
		// The final read's status is appended below, just like sync
		// progress.
	case dialog.search.IsEmpty():
		// A state filter replaces "every workspace": the list is still every
		// workspace's, and what it is narrowed to is the news.
		switch dialog.tab {
		case ResumeTabLive:
			if dialog.liveState != nil {
				spans = append(spans, chat.RawSpan(fmt.Sprintf("Live sessions · %s · newest first", dialog.liveState.label())))
			} else {
				spans = append(spans, chat.RawSpan("Live sessions · every workspace · newest first"))
			}
		case ResumeTabHel:
			spans = append(spans, chat.RawSpan("Mjolnir sessions · newest first"))
		case ResumeTabImport:
			spans = append(spans, chat.RawSpan("Importable sessions · newest first"))
		case ResumeTabArchive:
			spans = append(spans, chat.RawSpan("Archived sessions · newest first"))
		}
	case dialog.tab == ResumeTabLive:
		spans = append(spans, chat.RawSpan(matchCount(rows)))
		switch live := dialog.liveSearch.(type) {
		case LiveSearchStatePending, LiveSearchStateRefreshing:
			spans = append(spans, chat.CompactSpan(spinner, elapsed), chat.RawSpan(" Searching conversations…"))
		case LiveSearchStateFailed:
			spans = append(spans, chat.RawSpan(fmt.Sprintf(" · Conversation search failed: %s", live.Value)))
		}
	default:
		switch wiki := dialog.wikiSearch.(type) {
		case WikiSearchStatePending, WikiSearchStateRefreshing:
			spans = append(spans, chat.CompactSpan(spinner, elapsed), chat.RawSpan(" Searching…"))
		case WikiSearchStateFailed:
			spans = append(spans, chat.RawSpan(fmt.Sprintf("Search failed: %s", wiki.Value)))
		case WikiSearchStateAnswered:
			spans = append(spans, chat.RawSpan(matchCount(rows)))
		}
	}
	indexMismatch := false
	switch wiki := dialog.wikiSearch.(type) {
	case WikiSearchStateFailed:
		if dialog.search.IsEmpty() {
			spans = append(spans, chat.RawSpan(fmt.Sprintf(" · Search failed: %s", wiki.Value)))
		}
	case WikiSearchStateAnswered:
		if wiki.Value.State == client.WikiIndexStateVersionMismatch {
			indexMismatch = true
			spans = append(spans, chat.RawSpan(" · index from a newer SessionWiki"))
		}
	}
	var progress chat.Line
	haveProgress := true
	if refreshing {
		progress = chat.LineFrom(chat.RawSpan("Searching…"))
	} else {
		progress, haveProgress = dialog.sync.Line(spinner)
	}
	if haveProgress {
		if chat.LineFrom(spans...).Width()+progress.Width()+4 > max(width-2, 0) {
			prefix := " "
			_, historyFailed := dialog.history.(HistoryLoadFailed)
			_, historyLoading := dialog.history.(HistoryLoadLoading)
			if historyTab && historyFailed {
				prefix += "History failed · Sync: "
			} else if historyTab && historyLoading {
				prefix += "Loading history · "
			}
			if indexMismatch {
				prefix += "Index mismatch · "
			}
			room := max(width-(chat.RawSpan(prefix).Width()+3), 0)
			var fitted chat.Line
			fits := true
			if refreshing {
				fitted = progress
			} else {
				fitted, fits = dialog.sync.LineFitting(spinner, room)
			}
			if fits {
				fitted.Spans = append(append([]chat.Span{chat.RawSpan(prefix)}, fitted.Spans...), chat.RawSpan(" "))
				return fitted
			}
		}
		if len(spans) > 1 {
			spans = append(spans, chat.RawSpan(" · "))
		}
		spans = append(spans, progress.Spans...)
	}
	if dialog.tab == ResumeTabImport && dialog.isScanning() {
		scanned, total := dialog.scanProgress()
		spans = append(spans,
			chat.RawSpan(" · "),
			chat.CompactSpan(spinner, elapsed),
			chat.RawSpan(fmt.Sprintf(" scanning %d/%d", scanned, total)),
		)
	}
	spans = append(spans, chat.RawSpan(" "))
	return chat.LineFrom(spans...)
}

// matchCount is how many rows the query matched, counted in the reader's own
// grammar.
func matchCount(rows int) string {
	return counted(rows, "match", "matches")
}

// emptySearchMessage is what an empty list says while a query is running:
// where the query's hits are, when they are on another tab. The dialog never
// switches tabs by itself, so the message has to say where to look.
func emptySearchMessage(dashboard *DashboardState, dialog *ResumeDialog) string {
	var elsewhere []string
	for _, tab := range []struct {
		tab  ResumeTab
		name string
	}{
		{ResumeTabHel, "Mjolnir"},
		{ResumeTabImport, "Import"},
		{ResumeTabArchive, "Archived"},
	} {
		if hits := dashboard.resumeHitCount(tab.tab); tab.tab != dialog.tab && hits > 0 {
			elsewhere = append(elsewhere, fmt.Sprintf("%d on %s", hits, tab.name))
		}
	}
	if len(elsewhere) == 0 {
		if dialog.tab == ResumeTabLive {
			return "No matching live sessions"
		}
		return "No matching sessions"
	}
	return fmt.Sprintf("No matches here · %s", strings.Join(elsewhere, ", "))
}

// hitRoleLabel is the label and colour a matching message carries, in the
// same two colours the conversation view gives those roles. The roles are
// the briefing's own: anything else, such as the one-line message an error
// is shown as, carries no label. Tool messages never reach here, because a
// run of them is collapsed into a single line before any text is laid out.
func hitRoleLabel(role string) (string, chat.Color, bool) {
	switch role {
	case "user":
		return "User: ", chat.ThemePalette().Accent, true
	case "assistant":
		return "Assistant: ", chat.ThemePalette().Secondary, true
	}
	return "", 0, false
}

func omittedMarker(messages int) string {
	return fmt.Sprintf("*[… %s omitted …]*", counted(messages, "message", "messages"))
}

// hitBlockLines appends one matching message as lines of spans, with the
// matched ranges styled and the logical line each match starts on collected
// for hit navigation.
//
// block.Hits are byte ranges into the whole message, so a match that
// straddles a newline is split across the lines it covers and only its first
// line counts as the hit's position.
func hitBlockLines(block *client.WikiHitBlock, lines *[]chat.Line, hitLines *[]int) {
	hitStyle := chat.Style{}.Foreground(chat.ThemePalette().Accent).Bold()
	var prefix *chat.Span
	if label, color, ok := hitRoleLabel(block.Role); ok {
		span := chat.StyledSpan(label, chat.Style{}.Foreground(color))
		prefix = &span
	}
	start := 0
	for _, text := range strings.Split(block.Text, "\n") {
		end := start + len(text)
		var spans []chat.Span
		if prefix != nil {
			spans = append(spans, *prefix)
			prefix = nil
		}
		cursor := start
		for _, hit := range block.Hits {
			hitStart, hitEnd := hit[0], hit[1]
			from := max(hitStart, cursor)
			to := min(hitEnd, end)
			if from >= to {
				continue
			}
			if from > cursor {
				spans = append(spans, chat.RawSpan(block.Text[cursor:from]))
			}
			if hitStart >= start {
				*hitLines = append(*hitLines, len(*lines))
			}
			spans = append(spans, chat.StyledSpan(block.Text[from:to], hitStyle))
			cursor = to
		}
		if cursor < end {
			spans = append(spans, chat.RawSpan(block.Text[cursor:end]))
		}
		*lines = append(*lines, chat.LineFrom(spans...))
		// Past the newline that ended this line.
		start = end + 1
	}
	if block.Truncated {
		*lines = append(*lines, chat.StyledLine(
			"[… truncated …]",
			chat.Style{}.Foreground(chat.ThemePalette().Muted),
		))
	}
}

// toolRole is the briefing's name for a tool message.
const toolRole = "tool"

// toolRunLabel is what a run of consecutive tool messages is shown as. The
// preview is there to remind the reader what the session was about, and
// tool output is the part they did not write and would scroll past.
const toolRunLabel = "[tool calls]"

// hitTranscriptLines is the matching passages as logical lines of spans,
// with the logical line each hit starts on.
func hitTranscriptLines(transcript *client.WikiHitTranscript) ([]chat.Line, []int) {
	muted := chat.Style{}.Foreground(chat.ThemePalette().Muted)
	if len(transcript.Blocks) == 0 {
		return []chat.Line{chat.StyledLine("No matching passages", muted)}, nil
	}
	var lines []chat.Line
	var hitLines []int
	collapsingTools := false
	for index := range transcript.Blocks {
		block := &transcript.Blocks[index]
		isTool := block.Role == toolRole
		// A second tool message in the same run adds nothing to the one line
		// already standing in for the run. A run does end at a group
		// boundary, which the omitted marker announces.
		if isTool && collapsingTools && block.OmittedBefore == 0 {
			continue
		}
		if len(lines) > 0 {
			lines = append(lines, chat.RawLine(""))
		}
		if block.OmittedBefore > 0 {
			lines = append(lines, chat.StyledLine(omittedMarker(block.OmittedBefore), muted))
		}
		if isTool {
			lines = append(lines, chat.StyledLine(toolRunLabel, muted))
		} else {
			hitBlockLines(block, &lines, &hitLines)
		}
		collapsingTools = isTool
	}
	if transcript.OmittedAfter > 0 {
		lines = append(lines, chat.StyledLine(omittedMarker(transcript.OmittedAfter), muted))
	}
	return lines, hitLines
}

// wrapPreviewLines wraps preview lines to width cells, keeping each span's
// style.
//
// The pane wraps its own text so the wrapped row count is exact: the scroll
// offset, the clamp and the scrollbar all count the rows the renderer draws.
// Continuation rows are not indented, so a wrapped passage stays flush with
// the pane's left edge.
func wrapPreviewLines(lines []chat.Line, width int) []chat.Line {
	return chat.WrapLines(lines, min(max(width, 1), math.MaxUint16))
}

func resumeHeaderLine(layout *RowLayout, tab ResumeTab) chat.Line {
	style := chat.Style{}.Foreground(chat.ThemePalette().Muted).Bold()
	originLabel := "TARGET"
	if tab == ResumeTabLive {
		originLabel = "WORKSPACE"
	}
	var spans []chat.Span
	// A zero-width profile column means the tab has none; drop its cell and
	// separator together so no stray gap opens at the left of every row.
	if layout.profile > 0 {
		spans = append(spans, chat.StyledSpan(paddedCell("PROFILE", layout.profile), style), chat.RawSpan("  "))
	}
	spans = append(spans,
		chat.StyledSpan(paddedCell(originLabel, layout.origin), style),
		chat.RawSpan("  "),
		chat.StyledSpan(paddedCell("LAST ACTIVE", layout.activity), style),
		chat.RawSpan("  "),
		chat.StyledSpan(truncateToCells("SESSION", layout.title, TruncateSummary), style),
	)
	return chat.LineFrom(spans...)
}

func paddedCell(text string, width int) string {
	// Go's %-*s pads by code points, as Rust's `{:<width$}` does.
	return fmt.Sprintf("%-*s", width, truncateToCells(text, width, TruncateSummary))
}

func resumeRowLine(row *ResumeRow, layout *RowLayout, now time.Time) chat.Line {
	palette := chat.ThemePalette()
	titleStyle := chat.Style{}.Foreground(palette.Warning)
	if row.status.isRecoverable() {
		titleStyle = chat.Style{}.Bold()
	}
	var origin chat.Span
	if warning, ok := row.status.warning(); ok {
		origin = chat.StyledSpan(
			fmt.Sprintf("%-*s", layout.origin, warning),
			chat.Style{}.Foreground(palette.Warning),
		)
	} else {
		origin = chat.StyledSpan(
			fmt.Sprintf("%-*s", layout.origin, truncateToCells(row.origin, layout.origin, TruncateSummary)),
			chat.Style{}.Foreground(palette.Accent),
		)
	}
	marks := ""
	if row.unavailableReason != nil {
		marks += "  [unavailable]"
	}
	if operation := row.moveRecovery; operation != nil {
		if operation.QueueAdmissionStarted && !operation.QueueAdmissionFinished {
			marks += "  [move queue needs retry]"
		} else {
			marks += "  [move needs recovery]"
		}
	}
	var spans []chat.Span
	// Matches the zero-width rule in resumeHeaderLine, so header and rows
	// agree on the same layout.
	if layout.profile > 0 {
		spans = append(spans,
			chat.StyledSpan(paddedCell(row.profileID, layout.profile), chat.Style{}.Foreground(palette.Secondary)),
			chat.RawSpan("  "),
		)
	}
	spans = append(spans,
		origin,
		chat.RawSpan("  "),
		chat.StyledSpan(
			paddedCell(formatLastActive(now, row.lastActivityMs), layout.activity),
			chat.Style{}.Foreground(palette.Muted),
		),
		chat.RawSpan("  "),
	)
	marker := ""
	if row.publication != nil {
		switch *row.publication {
		case core.PublicationStateUnpublished:
			marker = "↑ "
		case core.PublicationStateUnknown:
			marker = "? "
		}
	}
	if marker != "" {
		spans = append(spans, chat.StyledSpan(marker, chat.Style{}.Foreground(palette.Warning)))
	}
	spans = append(spans,
		// The marks are ASCII. The title gives up their cells so they always
		// show in full.
		chat.StyledSpan(
			truncateToCells(
				row.title,
				max(layout.title-(len(marks)+utf8.RuneCountInString(marker)), 0),
				TruncateSummary,
			),
			titleStyle,
		),
		chat.StyledSpan(marks, chat.Style{}.Foreground(palette.Muted)),
	)
	return chat.LineFrom(spans...)
}

// ResumeProfilePlaceholders is placeholder entries so every configured
// profile shows before its scan reports anything.
func ResumeProfilePlaceholders(profiles iter.Seq2[string, core.HarnessKind]) []ImportProfileOption {
	var placeholders []ImportProfileOption
	for profileID, harnessKind := range profiles {
		placeholders = append(placeholders, ImportProfileOption{
			ProfileID:   profileID,
			HarnessKind: harnessKind,
		})
	}
	return placeholders
}

// maxChronoTimestampMs is the last instant chrono's
// `DateTime::from_timestamp_millis` accepts (+262142-12-31T23:59:59.999Z).
// A later activity time is unknown, as it is in Rust.
var maxChronoTimestampMs = time.Date(262142, time.December, 31, 23, 59, 59, 999_000_000, time.UTC).UnixMilli()

// resumeKeyModifiers is a key press's modifiers as crossterm reports them:
// Caps Lock, Num Lock and Scroll Lock are key state there, not modifiers.
func resumeKeyModifiers(modifiers tea.KeyMod) tea.KeyMod {
	return modifiers &^ (tea.ModCapsLock | tea.ModNumLock | tea.ModScrollLock)
}

// sortResumeRowsNewestFirst orders rows newest first, with the key breaking
// ties so the order holds still between rebuilds.
func sortResumeRowsNewestFirst(rows []ResumeRow) {
	slices.SortStableFunc(rows, func(left, right ResumeRow) int {
		if order := cmp.Compare(right.lastActivityMs, left.lastActivityMs); order != 0 {
			return order
		}
		return left.key.compare(right.key)
	})
}

// resumeWikiRankKey is a row's place in the index's answer, with unranked
// rows last (Rust's `wiki_rank.unwrap_or(usize::MAX)`).
func resumeWikiRankKey(row *ResumeRow) int {
	if row.wikiRank == nil {
		return math.MaxInt
	}
	return *row.wikiRank
}

// resumeReviewViews iterates a session's review views for
// client.AnyWorking.
func resumeReviewViews(views []client.RuntimeReviewView) iter.Seq[*client.RuntimeReviewView] {
	return func(yield func(*client.RuntimeReviewView) bool) {
		for index := range views {
			if !yield(&views[index]) {
				return
			}
		}
	}
}

// resumeHitCount is how many rows the current query matched on tab (Rust's
// `resume_hit_counts[tab.index()]`); zero before the first rebuild.
func (d *DashboardState) resumeHitCount(tab ResumeTab) int {
	if tab.index() < len(d.resumeHitCounts) {
		return d.resumeHitCounts[tab.index()]
	}
	return 0
}

// importProfileOptionEqual is Rust's derived PartialEq for
// ImportProfileOption, where an empty and an absent session list are the
// same Vec.
func importProfileOptionEqual(left, right *ImportProfileOption) bool {
	if left.ProfileID != right.ProfileID || left.HarnessKind != right.HarnessKind {
		return false
	}
	if (left.ScanProgress == nil) != (right.ScanProgress == nil) ||
		(left.ScanProgress != nil && *left.ScanProgress != *right.ScanProgress) {
		return false
	}
	if (left.Error == nil) != (right.Error == nil) || (left.Error != nil && *left.Error != *right.Error) {
		return false
	}
	return slices.EqualFunc(left.Sessions, right.Sessions, func(a, b ImportSessionOption) bool {
		return reflect.DeepEqual(a, b)
	})
}

// wikiRowsEqual is Rust's derived PartialEq over a page of search rows.
func wikiRowsEqual(left, right []client.WikiRow) bool {
	return slices.EqualFunc(left, right, func(a, b client.WikiRow) bool {
		return reflect.DeepEqual(a, b)
	})
}

// resumeStrLines is Rust's `str::lines`: split on `\n`, drop one `\r` before
// each, and no final empty line for a trailing newline.
func resumeStrLines(text string) []string {
	if text == "" {
		return nil
	}
	lines := strings.Split(strings.TrimSuffix(text, "\n"), "\n")
	for index, line := range lines {
		lines[index] = strings.TrimSuffix(line, "\r")
	}
	return lines
}

// resumePathComponents is a path's components as Rust's `Path::components`
// yields them: the root, then the normal parts, with empty and `.` parts
// dropped except a leading `.`.
func resumePathComponents(path string) []string {
	path = filepath.ToSlash(path)
	var parts []string
	if strings.HasPrefix(path, "/") {
		parts = append(parts, "/")
	}
	for index, part := range strings.Split(path, "/") {
		if part == "" || (part == "." && index > 0) {
			continue
		}
		parts = append(parts, part)
	}
	return parts
}

// resumeJoinComponents is Rust's collecting components into a PathBuf.
func resumeJoinComponents(parts []string) string {
	separator := string(filepath.Separator)
	if len(parts) > 0 && parts[0] == "/" {
		return separator + strings.Join(parts[1:], separator)
	}
	return strings.Join(parts, separator)
}

// resumePathStartsWith is Rust's `Path::starts_with`: whether base is a
// component prefix of path.
func resumePathStartsWith(path, base string) bool {
	left, right := resumePathComponents(path), resumePathComponents(base)
	return len(right) <= len(left) && slices.Equal(left[:len(right)], right)
}
