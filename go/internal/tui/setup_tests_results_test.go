// Port of mj-tui/src/setup/tests/results.rs.

package tui

import (
	"maps"
	"strings"
	"testing"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestResultsFromAClosedSetupDoNotChangeTheNewDraft(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	old := setupDialog(t, dashboard).generation
	dashboard.CancelModal()
	dashboard.BeginSetup()
	fresh := setupDialog(t, dashboard).generation
	original := setupValueClone(setupDialog(t, dashboard).draft)
	detected := *dashboard.config.Get()
	detected.Targets = maps.Clone(detected.Targets)
	detected.Targets["stale-discovery"] = core.TargetTemplateLocalBare{}
	dashboard.SetupDiscovered(old, setupDetection(DetectScopeRuntimes, detected), nil)
	dashboard.SetupSaved(old, *dashboard.config.Get(), nil)
	dialog := setupDialog(t, dashboard)
	if dialog.generation != fresh {
		t.Fatalf("generation = %d, want %d", dialog.generation, fresh)
	}
	if !setupValueEqual(dialog.draft, original) {
		t.Fatal("a closed dialog's result changed the new draft")
	}
}

func TestDetectionReportKeepsALongFixReachableUntilSettingsCloses(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "targets")
	generation := setupDialog(t, dashboard).generation
	fix := "To enforce them, have systemd delegate the controllers to user sessions: create " +
		"`/etc/systemd/system/user@.service.d/80-delegate.conf` containing `[Service]` and " +
		"`Delegate=pids memory cpu`, run `sudo systemctl daemon-reload`, then log in again. " +
		strings.Repeat("Padding so the report outgrows the band. ", 50) +
		" The tail of the fix is only reachable by scrolling."
	dashboard.SetupDiscovered(generation, SetupDetection{
		ProfileReadiness: map[string]*string{},
		Scope:            DetectScopeRuntimes,
		Config:           core.DefaultConfig(),
		RejectedRuntimes: []RejectedRuntime{{
			Kind:  "podman",
			Label: "Podman on nas",
			Detail: "systemd does not delegate the cpu and memory cgroup controllers to this " +
				"user, so sessions start without them.",
			Remediation: &fix,
		}},
	}, nil)
	// The status line counts and points at the view; it never carries the
	// fix, which a band this size would truncate.
	notice := setupDialog(t, dashboard).notice
	if notice == nil || !strings.Contains(*notice, "1 was left out; Detection results says why.") ||
		strings.Contains(*notice, "delegate") {
		t.Fatalf("notice = %v", notice)
	}

	setupActivate(t, dashboard, SetupControlDetectionResults)
	// The event loop renders between keys; the view's scroll maximum comes
	// from that render.
	if top := strings.Join(drawn(dashboard, 140, 34), "\n"); strings.Contains(top, "only reachable by scrolling") {
		t.Fatalf("the tail must start below the fold:\n%s", top)
	}
	dashboard.HandleKey(key(tea.KeyEnd))
	if shown := strings.Join(drawn(dashboard, 140, 34), "\n"); !strings.Contains(shown, "The tail of the fix is only reachable by scrolling.") {
		t.Fatalf("the end of the report must reach the tail of the fix:\n%s", shown)
	}

	// Esc leaves the report; the page keeps the report until Settings
	// closes.
	dashboard.HandleKey(key(tea.KeyEscape))
	if setupDialog(t, dashboard).report != nil {
		t.Fatal("Esc leaves the report")
	}
	setupActivate(t, dashboard, SetupControlBack)
	setupChoose(t, dashboard, "targets")
	if rejected := setupDialog(t, dashboard).detectionRejected; len(rejected) != 1 {
		t.Fatalf("rejected = %v", rejected)
	}
	setupActivate(t, dashboard, SetupControlDetectionResults)
	if setupDialog(t, dashboard).report == nil {
		t.Fatal("the report opens again")
	}

	// A fresh Settings knows nothing of the previous run's report.
	dashboard.CancelModal()
	dashboard.BeginSetup()
	if rejected := setupDialog(t, dashboard).detectionRejected; len(rejected) != 0 {
		t.Fatalf("rejected = %v", rejected)
	}
}
