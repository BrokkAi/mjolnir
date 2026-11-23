// Port of mj-tui/src/setup/tests/review.rs.

package tui

import "testing"

func TestOffReviewWithNoAvailableModelCanSaveSetup(t *testing.T) {
	dashboard := dashboardWithSession(stoppedSession())
	dashboard.BeginSetup()
	setupChoose(t, dashboard, "review")
	setup, ok := dashboard.mode.(ModeSetup)
	if !ok {
		t.Fatal("setup")
	}
	if setup.reviewEditor == nil {
		t.Fatal("Code Review opens")
	}
	if len(setup.reviewEditor.review.Reviewers) != 0 {
		t.Fatalf("reviewers = %v", setup.reviewEditor.review.Reviewers)
	}
	if !setup.reviewEditor.canSave() {
		t.Fatal("an off review can save")
	}
	if _, ok := dashboard.HandleKey(setupCtrl('s')).(DashboardActionSaveSetup); !ok {
		t.Fatal("Save saves Settings")
	}
}
