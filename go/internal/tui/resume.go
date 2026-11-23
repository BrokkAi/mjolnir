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
	"fmt"
	"iter"
	"math"
	"path/filepath"
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
	panic("unported: mj-tui/src/resume.rs resume_dialog")
}

// resumeDialogMut is resumeDialog for a caller that changes the dialog in
// place.
func resumeDialogMut(mode *Mode) *ResumeDialog {
	panic("unported: mj-tui/src/resume.rs resume_dialog_mut")
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
	panic("unported: mj-tui/src/resume.rs ResumeDialog::prepare")
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
	panic("unported: mj-tui/src/resume.rs ResumeDialog::is_scanning")
}

// scanProgress is the scanned and total counts summed across every profile
// still loading.
func (d *ResumeDialog) scanProgress() (scanned int, total int) {
	panic("unported: mj-tui/src/resume.rs ResumeDialog::scan_progress")
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
	panic("unported: mj-tui/src/resume.rs ResumeDialog::preview_body")
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
	panic("unported: mj-tui/src/resume.rs hel_row")
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
	panic("unported: mj-tui/src/resume.rs format_last_active")
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
	panic("unported: mj-tui/src/resume.rs merged_resume_rows")
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
	panic("unported: mj-tui/src/resume.rs source_project_path")
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
	panic("unported: mj-tui/src/resume.rs build_resume_rows")
}

// liveResumeRows is every session listed in a workspace, newest activity
// first, narrowed by the search box's text when there is any.
//
// Built here rather than in buildResumeRows because what the Sessions pane
// lists, and what each workspace is called, are this type's own knowledge.
func (d *DashboardState) liveResumeRows(dialog *ResumeDialog) []ResumeRow {
	panic("unported: mj-tui/src/resume.rs DashboardState::live_resume_rows")
}

// RebuildResumeRows rebuilds the open dialog's rows from what they are
// derived from: the Hel records, the scanned native sessions, the checkpoint
// sizes, and the newest search answer. Every mutation of those inputs calls
// this. Moving the selection only reads the rows.
func (d *DashboardState) RebuildResumeRows() {
	panic("unported: mj-tui/src/resume.rs DashboardState::rebuild_resume_rows")
}

// Rust's DashboardState::resume_rows() getter (the rows the open dialog
// shows; empty when no dialog is open) is not declared: in Go it would
// collide with the resumeRows field, which callers read directly.

// NeedsFastTick reports whether anything on screen animates on its own and
// so needs a redraw faster than the one-second clock: loading dialogs,
// session activity, or an in-flight lifecycle transition.
func (d *DashboardState) NeedsFastTick() bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::needs_fast_tick")
}

func (d *DashboardState) resumeCommandSessionID() (string, bool) {
	panic("unported: mj-tui/src/resume.rs DashboardState::resume_command_session_id")
}

func (d *DashboardState) ShowResumeDialog(discoveryID uint64, profiles []ImportProfileOption) {
	panic("unported: mj-tui/src/resume.rs DashboardState::show_resume_dialog")
}

// SetResumeFilter seeds Open with the workspace filter before its first
// background search.
func (d *DashboardState) SetResumeFilter(query string, state *SessionStateFilter) {
	panic("unported: mj-tui/src/resume.rs DashboardState::set_resume_filter")
}

// SessionRecord is one session's record: the live one from the runtime feed,
// or else a stopped one loaded on demand for the resume dialog, `mj go`, or
// an import. The feed carries only live sessions.
func (d *DashboardState) SessionRecord(sessionID string) *core.SessionRecord {
	panic("unported: mj-tui/src/resume.rs DashboardState::session_record")
}

// SetFailedLaunchRetries replaces what new-session defaults are chosen from.
func (d *DashboardState) SetFailedLaunchRetries(sessions map[string]struct{}) {
	panic("unported: mj-tui/src/resume.rs DashboardState::set_failed_launch_retries")
}

func (d *DashboardState) SetContainerUpgradeSessions(sessions map[string]struct{}) {
	panic("unported: mj-tui/src/resume.rs DashboardState::set_container_upgrade_sessions")
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
	panic("unported: mj-tui/src/resume.rs DashboardState::resume_checkpoint_archives")
}

// RememberStoppedRecord keeps one stopped record that something loaded on
// demand: the session `mj go` starts in, or a session just imported.
func (d *DashboardState) RememberStoppedRecord(session client.SessionRecordWithVerdict) {
	panic("unported: mj-tui/src/resume.rs DashboardState::remember_stopped_record")
}

// ApplyResumeRecord opens the resume wizard on a record fetched for the row
// picked in the open dialog. An answer for a dialog that has since closed,
// or for a row no longer being opened, is dropped. record is nil when the
// session no longer exists; err is set when it could not be loaded.
func (d *DashboardState) ApplyResumeRecord(sessionID string, record *client.SessionRecordWithVerdict, err error) DashboardAction {
	panic("unported: mj-tui/src/resume.rs DashboardState::apply_resume_record")
}

// ApplyResumeCandidates installs the daemon's answer about stopped sessions
// in the open dialog. Returns whether it was installed; an answer for a
// dialog that has since closed or reopened is dropped.
func (d *DashboardState) ApplyResumeCandidates(discoveryID uint64, candidates client.ResumeCandidates, err error) bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::apply_resume_candidates")
}

// ApplyResumeProfile folds one profile's scan result into the open dialog,
// keeping the selection on the same row.
func (d *DashboardState) ApplyResumeProfile(discoveryID uint64, profile ImportProfileOption) {
	panic("unported: mj-tui/src/resume.rs DashboardState::apply_resume_profile")
}

// ResumeSearchRequestID: the query remains active while a temporary overlay
// covers its dialog.
func (d *DashboardState) ResumeSearchRequestID() (uint64, bool) {
	panic("unported: mj-tui/src/resume.rs DashboardState::resume_search_request_id")
}

func (d *DashboardState) ApplyWikiSearch(requestID uint64, page client.WikiSearchPage) {
	d.ApplyWikiSearchResult(requestID, page, nil)
}

// ApplyResumeTextSearchResult: both searches belong to the dialog's one
// query generation; a delayed answer can never put an older query's rows or
// previews into the list.
func (d *DashboardState) ApplyResumeTextSearchResult(requestID uint64, matches []client.SessionTextMatch, err error) bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::apply_resume_text_search_result")
}

// ApplyWikiSearchResult: successes and failures share one admission point,
// including refreshes.
func (d *DashboardState) ApplyWikiSearchResult(requestID uint64, page client.WikiSearchPage, err error) bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::apply_wiki_search_result")
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
	panic("unported: mj-tui/src/resume.rs DashboardState::next_wiki_preview")
}

// ApplyWikiBrief folds one fetched briefing into the open dialog's preview
// cache.
func (d *DashboardState) ApplyWikiBrief(wikiID string, markdown string) {
	panic("unported: mj-tui/src/resume.rs DashboardState::apply_wiki_brief")
}

// ApplyWikiHits folds one query's matching passages into the open dialog's
// cache. A nil transcript means the index no longer holds that session,
// which the pane shows the same way as a session with no match.
func (d *DashboardState) ApplyWikiHits(wikiID string, query string, transcript *client.WikiHitTranscript) {
	panic("unported: mj-tui/src/resume.rs DashboardState::apply_wiki_hits")
}

// focusPreviewHit moves the preview pane onto hit index, reporting whether
// there was such a hit. The lines are wrapped here with the width the pane
// was last drawn at, so the stored offset is the row the reader will see.
func (d *DashboardState) focusPreviewHit(index int) bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::focus_preview_hit")
}

// stepPreviewHit moves step hits from the one the pane is on, stopping at
// either end, and reports whether the keystroke belonged to the pane.
func (d *DashboardState) stepPreviewHit(step int) bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::step_preview_hit")
}

// wikiSearchAction asks for a new search after the query changed. The
// debounce and the dropping of stale answers live with the request id, not
// with a timer here.
func (d *DashboardState) wikiSearchAction() DashboardAction {
	panic("unported: mj-tui/src/resume.rs DashboardState::wiki_search_action")
}

// NextWikiSearch claims the next search request id for the open dialog, with
// the query it should run. ok is false when no dialog is open.
func (d *DashboardState) NextWikiSearch() (requestID uint64, query string, ok bool) {
	panic("unported: mj-tui/src/resume.rs DashboardState::next_wiki_search")
}

// RefreshWikiSearch refreshes the same query without removing the results
// under the selection.
func (d *DashboardState) RefreshWikiSearch() (requestID uint64, query string, ok bool) {
	panic("unported: mj-tui/src/resume.rs DashboardState::refresh_wiki_search")
}

// resyncResumeSelection keeps rowIndex pointed at the selected row after the
// list changed.
func (d *DashboardState) resyncResumeSelection() {
	panic("unported: mj-tui/src/resume.rs DashboardState::resync_resume_selection")
}

// syncResumePreviewKey keeps the preview's scroll offset with the excerpt it
// was made for. A different transcript, or the same one under a new query,
// opens at its top and then on its first hit.
func (d *DashboardState) syncResumePreviewKey() {
	panic("unported: mj-tui/src/resume.rs DashboardState::sync_resume_preview_key")
}

// switchResumeTab moves the dialog to one tab, and answers with what the new
// tab needs fetched: the arrow keys and a click on the strip both come
// through here.
//
// The Live tab answers its own list immediately, while its search also
// updates the index results for the other tabs in the background.
func (d *DashboardState) switchResumeTab(tab ResumeTab) DashboardAction {
	panic("unported: mj-tui/src/resume.rs DashboardState::switch_resume_tab")
}

// settleResumeFocus puts the keyboard where the new tab's keys work: on the
// list, or on the tab strip when the list has no row to hold the focus.
//
// Without this a tab reached by arrow leaves the focus on the strip, where
// Enter is spent moving to the list rather than opening the selected row,
// and an empty list hands the focus to whichever button comes next. A person
// typing in the search box keeps it.
func (d *DashboardState) settleResumeFocus() {
	panic("unported: mj-tui/src/resume.rs DashboardState::settle_resume_focus")
}

// resumeFocusOutsideSearch is where the keyboard belongs once it leaves the
// search box: the list, or the tab strip when the list has no row that could
// take the focus. Asking a disabled list for it would leave the box holding
// the keyboard instead.
func (d *DashboardState) resumeFocusOutsideSearch() ResumeFocus {
	if len(d.resumeRows) == 0 {
		return ResumeFocusTabs
	}
	return ResumeFocusSessions
}

// stepResumeTab moves the dialog step tabs along the strip, wrapping around
// it.
func (d *DashboardState) stepResumeTab(step int) DashboardAction {
	panic("unported: mj-tui/src/resume.rs DashboardState::step_resume_tab")
}

// clearResumeSearch empties the search box and puts the unfiltered list
// back, the same rebuild deleting the last character would do.
func (d *DashboardState) clearResumeSearch() DashboardAction {
	panic("unported: mj-tui/src/resume.rs DashboardState::clear_resume_search")
}

func (d *DashboardState) selectResumeRow(index int) {
	panic("unported: mj-tui/src/resume.rs DashboardState::select_resume_row")
}

// selectedResumeRow is a copy of the row the open dialog points at, or nil.
func (d *DashboardState) selectedResumeRow() *ResumeRow {
	panic("unported: mj-tui/src/resume.rs DashboardState::selected_resume_row")
}

// scrollResumePreview moves the preview pane when a wheel notch lands on it
// or a page key is pressed with the list focused, and reports whether it
// took the event.
//
// The pane registers a scrollable surface every frame, so the last frame's
// registration carries both the hitbox and the wrapped line count the offset
// clamps against.
func (d *DashboardState) scrollResumePreview(event tea.Msg, focused ResumeFocus) bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::scroll_resume_preview")
}

func (d *DashboardState) resumeScrollbarHandlesMouse(mouse tea.MouseMsg) bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::resume_scrollbar_handles_mouse")
}

func (d *DashboardState) handleResumeScrollbarMouse(event tea.Msg) bool {
	panic("unported: mj-tui/src/resume.rs DashboardState::handle_resume_scrollbar_mouse")
}

func (d *DashboardState) handleResumeDialogEvent(event tea.Msg) DashboardAction {
	panic("unported: mj-tui/src/resume.rs DashboardState::handle_resume_dialog_event")
}

// destroySelectedResumeRow asks for confirmation before destroying the
// selected row's session record.
func (d *DashboardState) destroySelectedResumeRow() DashboardAction {
	panic("unported: mj-tui/src/resume.rs DashboardState::destroy_selected_resume_row")
}

func (d *DashboardState) activateSelectedResumeRow(row *ResumeRow) DashboardAction {
	panic("unported: mj-tui/src/resume.rs DashboardState::activate_selected_resume_row")
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
	panic("unported: mj-tui/src/resume.rs row_layout")
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
	panic("unported: mj-tui/src/resume.rs render_resume_dialog")
}

// resumeTabLabels is the tab labels, carrying the scan progress on Import
// and, while a query is active, how many rows each tab matched.
func resumeTabLabels(dashboard *DashboardState, dialog *ResumeDialog) []string {
	panic("unported: mj-tui/src/resume.rs resume_tab_labels")
}

// resumeListTitle is the list panel's title: what the tab lists, how the
// current search is going, and, on Import, whether the native scan is still
// running.
func resumeListTitle(dashboard *DashboardState, dialog *ResumeDialog, rows int, width int) chat.Line {
	panic("unported: mj-tui/src/resume.rs resume_list_title")
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
	panic("unported: mj-tui/src/resume.rs empty_search_message")
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
	panic("unported: mj-tui/src/resume.rs hit_block_lines")
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
	panic("unported: mj-tui/src/resume.rs hit_transcript_lines")
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
	panic("unported: mj-tui/src/resume.rs resume_header_line")
}

func paddedCell(text string, width int) string {
	// Go's %-*s pads by code points, as Rust's `{:<width$}` does.
	return fmt.Sprintf("%-*s", width, truncateToCells(text, width, TruncateSummary))
}

func resumeRowLine(row *ResumeRow, layout *RowLayout, now time.Time) chat.Line {
	panic("unported: mj-tui/src/resume.rs resume_row_line")
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
