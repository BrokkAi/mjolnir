// Port of mj-cli/src/dashboard/upgrade.rs.
//
// Private, versioned terminal handoff between installed builds. Keep this
// format backward compatible: the receiving executable is a newer release,
// possibly the Rust mj. The JSON is serde's, field for field.

package cli

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// resumeEnv names the handoff file an upgraded terminal resumes from.
const resumeEnv = "MJ_UPGRADE_RESUME_FILE"

// upgradeResume is what a terminal hands to the upgraded executable that
// replaces it: its workspace, client identity, drafts, forms, transcript
// positions, layouts and read positions.
type upgradeResume struct {
	Version     uint32                             `json:"version"`
	WorkspaceID string                             `json:"workspace_id"`
	ClientID    string                             `json:"client_id"`
	Drafts      composerDraftCache                 `json:"drafts"`
	Questions   map[string][]cachedQuestionDraft   `json:"questions"`
	Positions   map[string]chat.TranscriptPosition `json:"positions"`
	Layouts     map[string]core.ConversationLayout `json:"layouts"`
	// ReadPositions is how far each session has been read.
	ReadPositions map[string]uint64 `json:"read_positions"`
}

// loadUpgradeResume reads the handoff named by resumeEnv; nil when this
// launch is not resuming one. The read runs off the caller's goroutine.
func loadUpgradeResume(ctx context.Context) (*upgradeResume, error) {
	panic("unported: mj-cli/src/dashboard/upgrade.rs UpgradeResume::load")
}

// restart writes the handoff and replaces this process with target's
// executable (exec on Unix; elsewhere it runs it and waits). Called only
// after the old terminal and runtime have been released. It returns only on
// failure, with the handoff preserved.
func (r *upgradeResume) restart(target UpgradeTarget) error {
	panic("unported: mj-cli/src/dashboard/upgrade.rs UpgradeResume::restart")
}

// captureUpgrade collects this terminal's state for the handoff.
func (c *dashboardContext) captureUpgrade() upgradeResume {
	panic("unported: mj-cli/src/dashboard/upgrade.rs DashboardContext::capture_upgrade")
}

// restoreUpgrade adopts a handoff's state, then removes its file. A startup
// error before that leaves it recoverable.
func (c *dashboardContext) restoreUpgrade(ctx context.Context, resume upgradeResume) {
	panic("unported: mj-cli/src/dashboard/upgrade.rs DashboardContext::restore_upgrade")
}
