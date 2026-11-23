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
	"slices"

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
	panic("unported: mj-tui/src/render_changes.rs DashboardState::current_clock_signature")
}

func (d *DashboardState) currentAnimationSignature() RenderAnimationSignature {
	panic("unported: mj-tui/src/render_changes.rs DashboardState::current_animation_signature")
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
	panic("unported: mj-tui/src/render_changes.rs DashboardState::support_projection_visible")
}

func (d *DashboardState) sessionCPUSignature() []DisplayedClock {
	panic("unported: mj-tui/src/render_changes.rs DashboardState::session_cpu_signature")
}

func (d *DashboardState) sessionClockSignature(now uint64) []DisplayedClock {
	panic("unported: mj-tui/src/render_changes.rs DashboardState::session_clock_signature")
}

func (d *DashboardState) capacityClockSignature(now uint64) []DisplayedClock {
	panic("unported: mj-tui/src/render_changes.rs DashboardState::capacity_clock_signature")
}

func (d *DashboardState) quotaClockSignature(now uint64) []DisplayedClock {
	panic("unported: mj-tui/src/render_changes.rs DashboardState::quota_clock_signature")
}

func (d *DashboardState) resumeClockSignature() []DisplayedClock {
	panic("unported: mj-tui/src/render_changes.rs DashboardState::resume_clock_signature")
}

func (d *DashboardState) visibleAnimationActive() bool {
	panic("unported: mj-tui/src/render_changes.rs DashboardState::visible_animation_active")
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
