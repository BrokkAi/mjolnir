// Port of mj-tui/src/wiki_sync.rs.
//
// Sync episodes outlive individual debounced queries.

package tui

import (
	"math"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const syncFailedPrefix = "Index sync failed: "

// filledBlocks is how many of width progress blocks done of total fills;
// all of them when total is zero.
func filledBlocks(done int, total int, width int) int {
	if total == 0 {
		return width
	}
	return min(done*width/total, width)
}

// SyncEpisode is one session-index sync the dashboard is waiting on.
type SyncEpisode struct {
	ID uint64
	// Status is nil until the daemon first answers.
	Status *client.DaemonResult[client.WikiSyncStatus]
}

func (e *SyncEpisode) waiting() bool {
	if e.Status == nil || e.Status.Err != nil {
		return false
	}
	status := e.Status.Ok
	return status.Target != nil && status.Started < *status.Target
}

// Label is the episode's progress sentence; ok is false once the sync
// finished successfully.
func (e *SyncEpisode) Label() (string, bool) {
	panic("unported: mj-tui/src/wiki_sync.rs SyncEpisode::label")
}

func (e *SyncEpisode) Scanning() bool {
	panic("unported: mj-tui/src/wiki_sync.rs SyncEpisode::scanning")
}

func (e *SyncEpisode) LineFitting(spinner core.SpinnerStyle, width int) (chat.Line, bool) {
	panic("unported: mj-tui/src/wiki_sync.rs SyncEpisode::line_fitting")
}

func (e *SyncEpisode) Line(spinner core.SpinnerStyle) (chat.Line, bool) {
	label, ok := e.Label()
	if !ok {
		return chat.Line{}, false
	}
	var spans []chat.Span
	if e.Scanning() {
		spans = append(spans, chat.CompactSpan(spinner, chat.ElapsedMs()), chat.RawSpan(" "))
	}
	spans = append(spans, chat.RawSpan(label))
	return chat.LineFrom(spans...), true
}

func (d *DashboardState) WikiSyncEpisode(dialog bool) (uint64, bool) {
	panic("unported: mj-tui/src/wiki_sync.rs DashboardState::wiki_sync_episode")
}

// ApplyWikiSync records a sync status answer (err is Rust's Err(String)) and
// returns true exactly when the caller should rerun the current query.
func (d *DashboardState) ApplyWikiSync(dialog bool, episode uint64, status client.WikiSyncStatus, err error) bool {
	panic("unported: mj-tui/src/wiki_sync.rs DashboardState::apply_wiki_sync")
}

// RefreshSessionsTextSearch reruns the Sessions filter's conversation search
// after a sync; ok is false when no text was asked.
func (d *DashboardState) RefreshSessionsTextSearch() (requestID uint64, query string, ok bool) {
	panic("unported: mj-tui/src/wiki_sync.rs DashboardState::refresh_sessions_text_search")
}

func (d *DashboardState) sessionsSyncLine(width int) (chat.Line, bool) {
	panic("unported: mj-tui/src/wiki_sync.rs DashboardState::sessions_sync_line")
}

func (d *DashboardState) sessionsSyncLabel() (string, bool) {
	line, ok := d.sessionsSyncLine(math.MaxUint16)
	if !ok {
		return "", false
	}
	return line.String(), true
}
