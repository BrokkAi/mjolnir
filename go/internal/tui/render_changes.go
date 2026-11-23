// Port of mj-tui/src/render_changes.rs.
//
// Cheap, display-scoped invalidation for dashboard timers and animation.
//
// The dashboard is fed by several independent background projections. Their
// state remains current even while another workspace or a modal is visible,
// but only the values that can change the next frame are included here. The
// snapshot is refreshed after a frame is drawn; checking a clock or animation
// never mutates the dashboard or requests a redraw by itself.

package tui

import (
	"fmt"
	"maps"
	"math"
	"slices"
	"strconv"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// epochSeconds is Rust's re-export of mj_core::clock::epoch_seconds.
func epochSeconds() uint64 {
	return core.EpochSeconds()
}

// RenderChangeSnapshot is the values that were visible when the last frame
// was drawn.
type RenderChangeSnapshot struct {
	clock     RenderClockSignature
	animation RenderAnimationSignature
}

type RenderClockSignature struct {
	sessions []DisplayedClock
	cpu      []DisplayedClock
	capacity []DisplayedClock
	quota    []DisplayedClock
	resume   []DisplayedClock
	// importStatus is nil when no import is in progress.
	importStatus *string
}

// equal is Rust's derived PartialEq.
func (s RenderClockSignature) equal(other RenderClockSignature) bool {
	return slices.Equal(s.sessions, other.sessions) &&
		slices.Equal(s.cpu, other.cpu) &&
		slices.Equal(s.capacity, other.capacity) &&
		slices.Equal(s.quota, other.quota) &&
		slices.Equal(s.resume, other.resume) &&
		renderChangesOptionalEqual(s.importStatus, other.importStatus)
}

type DisplayedClock struct {
	key   string
	value string
}

type RenderAnimationSignature struct {
	active     bool
	frame      *string
	modalFrame *string
}

// equal is Rust's derived PartialEq.
func (s RenderAnimationSignature) equal(other RenderAnimationSignature) bool {
	return s.active == other.active &&
		renderChangesOptionalEqual(s.frame, other.frame) &&
		renderChangesOptionalEqual(s.modalFrame, other.modalFrame)
}

// renderChangesOptionalEqual compares two Option<String> values.
func renderChangesOptionalEqual(a, b *string) bool {
	if a == nil || b == nil {
		return a == b
	}
	return *a == *b
}

// ClockChanged reports whether a dashboard clock has changed since the last
// drawn frame.
//
// Only text that actually contains a moving value is sampled. Idle sessions,
// unrefreshed panes, and static reset strings therefore do not turn the
// once-per-second wakeup into a redraw.
func (d *DashboardState) ClockChanged() bool {
	return d.quotaProviders.ready() || !d.renderChangeSnapshot.clock.equal(d.currentClockSignature())
}

// AnimationChanged reports whether a visible dashboard spinner has advanced
// to another frame.
//
// The active flag is part of the signature so starting or settling the last
// spinner is noticed even when its previous frame happens to match the new
// idle signature.
func (d *DashboardState) AnimationChanged() bool {
	return !d.renderChangeSnapshot.animation.equal(d.currentAnimationSignature())
}

// AcknowledgeRender seeds the time signatures with the values represented by
// the frame just drawn. The event loop calls this after rendering, including
// frames requested by input or background content, so the next timer checks
// the displayed baseline rather than the previous timer check.
func (d *DashboardState) AcknowledgeRender() {
	d.renderChangeSnapshot.clock = d.currentClockSignature()
	d.renderChangeSnapshot.animation = d.currentAnimationSignature()
}

func (d *DashboardState) currentClockSignature() RenderClockSignature {
	now := epochSeconds()
	signature := RenderClockSignature{
		sessions: d.sessionClockSignature(now),
		cpu:      d.sessionCPUSignature(),
		capacity: d.capacityClockSignature(now),
		quota:    d.quotaClockSignature(now),
		resume:   d.resumeClockSignature(),
	}
	if importing, ok := d.mode.(ModeImporting); ok {
		status := importProgressStatus(importing.ImportProgress).String()
		signature.importStatus = &status
	}
	return signature
}

func (d *DashboardState) currentAnimationSignature() RenderAnimationSignature {
	active := d.visibleAnimationActive()
	signature := RenderAnimationSignature{active: active}
	if active {
		frame := chat.CompactFrame(d.config.Get().Spinner, chat.ElapsedMs())
		signature.frame = &frame
	}
	if dialog, ok := d.mode.(ModeResumeDialog); ok && dialog.isScanning() {
		frame := chat.CompactFrame(d.config.Get().Spinner, uint64(time.Since(dialog.openedAt).Milliseconds()))
		signature.modalFrame = &frame
	}
	return signature
}

// sessionIsVisible reports whether a session row belongs to the current tab
// while the record is still active, or while an explicit operation keeps its
// row on screen.
func (d *DashboardState) sessionIsVisible(sessionID string) bool {
	index, ok := d.orderedSessionPosition(sessionID)
	return ok && d.sessionRowIsVisibleAt(index)
}

// sessionRowIsVisibleAt reads sessionRowAreas, which the last rendered
// Sessions table populates. Before the first frame there is no viewport to
// consult, so an active workspace row is treated as visible and the first
// projection can still request that frame. Once geometry exists, only rows
// in that viewport participate in feed invalidation and timer signatures.
func (d *DashboardState) sessionRowIsVisibleAt(index int) bool {
	if len(d.sessionRowAreas) == 0 {
		return true
	}
	for _, area := range d.sessionRowAreas {
		if area.First == index {
			return true
		}
	}
	return false
}

// supportProjectionVisible reports whether a change to a support projection
// can be seen on the dashboard. Minimized panes still show their one-line
// summary, so all three panes count as visible whenever the dashboard has a
// workspace selected. A target-capacity sample is also used by the New and
// Resume wizards.
func (d *DashboardState) supportProjectionVisible(pane SupportPane) bool {
	if _, ok := d.ActiveWorkspaceID(); !ok {
		return false
	}
	switch d.mode.(type) {
	case ModeNew, ModeResume:
		return pane == SupportPaneTargets
	}
	if d.ModalOpen() {
		return false
	}
	if d.paneAreas == nil {
		return true
	}
	var index int
	switch pane {
	case SupportPaneSessions:
		index = 0
	case SupportPaneTargets:
		index = 1
	case SupportPaneQuota:
		index = 2
	default:
		return d.notificationsArea != nil && d.notificationsArea.Dx() > 0 && d.notificationsArea.Dy() > 0
	}
	area := d.paneAreas[index]
	return area.Dx() > 0 && area.Dy() > 0
}

func (d *DashboardState) sessionCPUSignature() []DisplayedClock {
	if report, ok := d.mode.(ModeSessionCpuReport); ok {
		lines := reportLines(d, report.metric)
		clocks := make([]DisplayedClock, 0, len(lines))
		for index, line := range lines {
			clocks = append(clocks, DisplayedClock{key: strconv.Itoa(index), value: line.String()})
		}
		return clocks
	}
	if !d.supportProjectionVisible(SupportPaneSessions) {
		return nil
	}
	var clocks []DisplayedClock
	for index, session := range d.orderedSessions() {
		if !d.sessionRowIsVisibleAt(index) {
			continue
		}
		share, ok := d.cpuShare(session.ID)
		if !ok || share.permille < 10 {
			continue
		}
		clocks = append(clocks, DisplayedClock{key: session.ID, value: share.label()})
	}
	return clocks
}

func (d *DashboardState) sessionClockSignature(now uint64) []DisplayedClock {
	var clocks []DisplayedClock
	for index, session := range d.orderedSessions() {
		if !d.sessionRowIsVisibleAt(index) {
			continue
		}
		detail := (*d.sessionDetails.Get())[session.ID]
		operation := (*d.sessionOperations.Get())[session.ID]
		_, unreachable := (*d.unreachableSessions.Get())[session.ID]
		if value, ok := sessionDisplayClock(
			d,
			session,
			detail,
			d.sessionReviews(session.ID),
			unreachable,
			operation,
			now,
		); ok {
			clocks = append(clocks, DisplayedClock{key: fmt.Sprintf("session:%s:elapsed", session.ID), value: value})
		}
		if value, ok := recoveryAgeClock(session, d, now); ok {
			clocks = append(clocks, DisplayedClock{key: fmt.Sprintf("session:%s:recovery", session.ID), value: value})
		}
	}
	return clocks
}

func (d *DashboardState) capacityClockSignature(now uint64) []DisplayedClock {
	if !d.supportProjectionVisible(SupportPaneTargets) {
		return nil
	}
	var clocks []DisplayedClock
	for _, id := range slices.Sorted(maps.Keys(d.capacityDetails)) {
		reading := d.capacityDetails[id].Reading
		if reading.SampledAtEpochSeconds == nil {
			continue
		}
		sampledAt := *reading.SampledAtEpochSeconds
		if reading.ProbeError != nil || !core.CapacitySampleIsStale(sampledAt, now) {
			continue
		}
		clocks = append(clocks, DisplayedClock{key: "capacity:" + id, value: refreshAge(now, sampledAt)})
	}
	return clocks
}

func (d *DashboardState) quotaClockSignature(now uint64) []DisplayedClock {
	var clocks []DisplayedClock
	if !d.supportProjectionVisible(SupportPaneQuota) {
		return clocks
	}
	if len(d.quotaRefreshing) == 0 && len(d.quotas) > 0 {
		refreshed := uint64(math.MaxUint64)
		for _, quota := range d.quotas {
			refreshed = min(refreshed, quota.RefreshedAtEpochSeconds)
		}
		clocks = append(clocks, DisplayedClock{key: "quota:refreshed", value: refreshAge(now, refreshed)})
	}

	providers := providerPools(d.config.Get(), d.quotas, &d.quotaProviders, now)
	// A reading crossing its reset changes pool totals and lanes while the
	// countdowns stay at `0m`, so count the windows awaiting a reading (the
	// minimized line shows the totals too). Long and five-hour windows are
	// counted separately: either crossing its reset changes what is drawn.
	awaiting, fiveAwaiting := 0, 0
	for _, provider := range providers {
		for _, pool := range provider.Pools {
			for _, member := range pool.Members {
				if member.AwaitingReading {
					awaiting++
				}
				if member.FiveHourAwaitingReading {
					fiveAwaiting++
				}
			}
		}
	}
	clocks = append(clocks, DisplayedClock{
		key:   "quota:awaiting-reading",
		value: fmt.Sprintf("%d/%d", awaiting, fiveAwaiting),
	})
	if d.PaneSize(SupportPaneQuota) != core.PaneSizeMinimized {
		for _, provider := range providers {
			for _, pool := range provider.Pools {
				expanded := slices.ContainsFunc(d.quotaExpanded, func(key PoolKey) bool {
					return key.compare(pool.Key) == 0
				})
				// The key only has to tell pools apart; Rust writes the
				// pool key's Debug form.
				poolKey := fmt.Sprintf("%#v", pool.Key)
				for _, member := range pool.Members {
					clocks = append(clocks, DisplayedClock{
						key:   fmt.Sprintf("quota:%s:%s:weekly", poolKey, member.ID),
						value: resetDuration(member.Reset, now),
					})
					if expanded && member.FiveHour != nil {
						clocks = append(clocks, DisplayedClock{
							key:   fmt.Sprintf("quota:%s:%s:five-hour", poolKey, member.ID),
							value: resetDuration(member.FiveHour.ResetsAtEpochSeconds, now),
						})
					}
					if member.Note != nil {
						clocks = append(clocks, DisplayedClock{
							key:   fmt.Sprintf("quota:%s:hold", member.ID),
							value: *member.Note,
						})
					}
				}
			}
			for _, text := range provider.Text {
				clocks = append(clocks, DisplayedClock{key: fmt.Sprintf("quota:%s:status", text.ID), value: text.Text})
			}
		}
		if len(d.quotaExpanded) > 0 {
			clocks = append(clocks, DisplayedClock{key: "quota:axis", value: strconv.FormatUint(now/60, 10)})
		}
	}
	return clocks
}

func (d *DashboardState) resumeClockSignature() []DisplayedClock {
	dialog, ok := d.mode.(ModeResumeDialog)
	if !ok {
		return nil
	}
	now := displayNow()
	offset := dialog.form.ListOffset(ResumeFocusSessions)
	height := math.MaxInt
	if surface := d.frameSurfaces.Surface(chat.SurfaceIdResumeList{}); surface != nil {
		height = max(surface.Rect.Dy()-1, 0)
	}
	var clocks []DisplayedClock
	for index, row := range d.resumeRowsField {
		if index < offset {
			continue
		}
		if index-offset >= height {
			break
		}
		clocks = append(clocks, DisplayedClock{key: strconv.Itoa(index), value: formatLastActive(now, row.lastActivityMs)})
	}
	return clocks
}

func (d *DashboardState) visibleAnimationActive() bool {
	sessions := false
	for index, session := range d.orderedSessions() {
		if !d.sessionRowIsVisibleAt(index) {
			continue
		}
		reviews := d.sessionReviews(session.ID)
		primary := d.sessionIsWorking(session.ID)
		transition := d.TransitionKind(session.ID) != nil && session.LastError == nil
		reviewing := client.AnyWorking(func(yield func(*client.RuntimeReviewView) bool) {
			for index := range reviews {
				if !yield(&reviews[index]) {
					return
				}
			}
		})
		if primary || transition || reviewing {
			sessions = true
			break
		}
	}
	opening := false
	if sessionID, ok := d.OpeningSession(); ok {
		opening = d.sessionIsVisible(sessionID)
	}
	modal := false
	if dialog, ok := d.mode.(ModeTargetActions); ok {
		modal = dialog.testing != nil
	}
	return sessions || opening || modal
}

// recoveryAgeClock is the age of the checkpoint a session can be recovered
// from, while its last checkpoint failed and no transition owns it.
func recoveryAgeClock(session *core.SessionRecord, dashboard *DashboardState, now uint64) (string, bool) {
	if session.LastCheckpointError == nil ||
		dashboard.TransitionKind(session.ID) != nil ||
		dashboard.TransitionFailureKind(session.ID) != nil {
		return "", false
	}
	if session.Checkpoint == nil {
		return "", false
	}
	return checkpointAge(now, session.Checkpoint.CreatedAt), true
}
