// Port of mj-tui/src/wiki_sync.rs.
//
// Sync episodes outlive individual debounced queries.

package tui

import (
	"fmt"
	"math"
	"strings"

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
	if e.Status == nil {
		return "Syncing session index…", true
	}
	if e.Status.Err != nil {
		return syncFailedPrefix + *e.Status.Err, true
	}
	status := &e.Status.Ok
	if status.Target != nil && status.Finished >= *status.Target {
		if status.Successful >= *status.Target {
			return "", false
		}
		if failed, ok := status.Outcome.(client.WikiSyncOutcomeFailed); ok {
			return syncFailedPrefix + failed.Reason, true
		}
		return "", false
	}
	if e.waiting() {
		return "Waiting for session index sync…", true
	}
	progress := status.Running
	if progress == nil || progress.Tool == "" {
		return "Syncing session index…", true
	}
	if progress.Total == nil {
		return "Scanning " + progress.Tool, true
	}
	total := *progress.Total
	blocks := filledBlocks(progress.Done, total, 10)
	return fmt.Sprintf(
		"Syncing %s %d/%d ▕%s%s▏ %d/%d",
		progress.Tool,
		progress.Adapter,
		progress.Adapters,
		strings.Repeat("█", blocks),
		strings.Repeat("░", 10-blocks),
		progress.Done,
		total,
	), true
}

func (e *SyncEpisode) Scanning() bool {
	if e.waiting() {
		return true
	}
	if e.Status == nil {
		return true
	}
	if e.Status.Err != nil {
		return false
	}
	status := &e.Status.Ok
	if status.Target != nil && status.Finished >= *status.Target {
		return false
	}
	return status.Running == nil || status.Running.Total == nil
}

func (e *SyncEpisode) LineFitting(spinner core.SpinnerStyle, width int) (chat.Line, bool) {
	full, ok := e.Line(spinner)
	if !ok {
		return chat.Line{}, false
	}
	if full.Width() <= width {
		return full, true
	}
	label, ok := e.Label()
	if !ok {
		return chat.Line{}, false
	}
	var short string
	if cause, failed := strings.CutPrefix(label, syncFailedPrefix); failed {
		// Keep the actionable cause, rather than spending a narrow row on its prefix.
		short = cause
	} else if !e.waiting() && e.Status != nil && e.Status.Err == nil &&
		e.Status.Ok.Running != nil && e.Status.Ok.Running.Total != nil {
		progress := e.Status.Ok.Running
		total := *progress.Total
		counts := fmt.Sprintf("%d/%d", progress.Done, total)
		blocks := min(max(width-(len(counts)+3), 0), 10)
		if blocks == 0 {
			short = counts
		} else {
			filled := filledBlocks(progress.Done, total, blocks)
			short = fmt.Sprintf(
				"▕%s%s▏ %s",
				strings.Repeat("█", filled),
				strings.Repeat("░", blocks-filled),
				counts,
			)
		}
	} else {
		return chat.TruncateLineToWidth(full, width), true
	}
	return chat.TruncateLineToWidth(chat.RawLine(short), width), true
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
	if dialog {
		if resume := resumeDialog(d.mode); resume != nil {
			return resume.sync.ID, true
		}
		return 0, false
	}
	if sync := d.sessionsText.Get().sync; sync != nil {
		return sync.ID, true
	}
	return 0, false
}

// ApplyWikiSync records a sync status answer (err is Rust's Err(String)) and
// returns true exactly when the caller should rerun the current query.
func (d *DashboardState) ApplyWikiSync(dialog bool, episode uint64, status client.WikiSyncStatus, err error) bool {
	var sync *SyncEpisode
	if dialog {
		if resume := resumeDialogMut(&d.mode); resume != nil {
			sync = &resume.sync
		}
	} else {
		sync = d.sessionsText.Mut().sync
	}
	if sync == nil || sync.ID != episode {
		return false
	}
	var result client.DaemonResult[client.WikiSyncStatus]
	finished := false
	if err != nil {
		result = client.DaemonErr[client.WikiSyncStatus](err.Error())
	} else {
		result = client.DaemonOk(status)
		finished = status.Target != nil && status.Finished >= *status.Target &&
			status.Successful >= *status.Target
	}
	sync.Status = &result
	return finished
}

// RefreshSessionsTextSearch reruns the Sessions filter's conversation search
// after a sync; ok is false when no text was asked.
func (d *DashboardState) RefreshSessionsTextSearch() (requestID uint64, query string, ok bool) {
	if d.sessionsText.Get().asked == "" {
		return 0, "", false
	}
	text := d.sessionsText.Mut()
	text.requestID++
	refresh := SessionsTextQuerySyncRefresh
	text.pending = &refresh
	return text.requestID, text.asked, true
}

func (d *DashboardState) sessionsSyncLine(width int) (chat.Line, bool) {
	text := d.sessionsText.Get()
	if text.sync == nil {
		return chat.Line{}, false
	}
	if line, ok := text.sync.LineFitting(d.config.Get().Spinner, width); ok {
		return line, true
	}
	if text.isPending() {
		return chat.RawLine("Searching…"), true
	}
	return chat.Line{}, false
}

func (d *DashboardState) sessionsSyncLabel() (string, bool) {
	line, ok := d.sessionsSyncLine(math.MaxUint16)
	if !ok {
		return "", false
	}
	return line.String(), true
}
