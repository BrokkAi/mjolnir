// Port of mj-cli/src/dashboard/upgrade.rs.
//
// Private, versioned terminal handoff between installed builds. Keep this
// format backward compatible: the receiving executable is a newer release,
// possibly the Rust mj. The JSON is serde's, field for field.

package cli

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"syscall"

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

// upgradeResumeFields are the fields serde requires: the Rust struct has no
// defaults, so a document missing one does not decode.
var upgradeResumeFields = []string{
	"version", "workspace_id", "client_id", "drafts", "questions", "positions", "layouts", "read_positions",
}

// MarshalJSON writes the handoff as serde_json does, empty maps as {}.
func (r upgradeResume) MarshalJSON() ([]byte, error) {
	type plain upgradeResume
	return core.SerdeJSON(plain(r))
}

// UnmarshalJSON reads the handoff as serde does, refusing a document that
// lacks one of its fields.
func (r *upgradeResume) UnmarshalJSON(data []byte) error {
	var present map[string]json.RawMessage
	if err := json.Unmarshal(data, &present); err != nil {
		return err
	}
	for _, field := range upgradeResumeFields {
		if _, ok := present[field]; !ok {
			return fmt.Errorf("missing field `%s`", field)
		}
	}
	type plain upgradeResume
	return json.Unmarshal(data, (*plain)(r))
}

// loadUpgradeResume reads the handoff named by resumeEnv; nil when this
// launch is not resuming one. The read runs off the caller's goroutine.
func loadUpgradeResume(ctx context.Context) (*upgradeResume, error) {
	path, ok := os.LookupEnv(resumeEnv)
	if !ok {
		return nil, nil
	}
	type loaded struct {
		resume *upgradeResume
		err    error
	}
	result, err := core.SpawnBlocking(func() loaded {
		body, err := os.ReadFile(path)
		if err != nil {
			return loaded{err: err}
		}
		var resume upgradeResume
		if err := json.Unmarshal(body, &resume); err != nil {
			return loaded{err: fmt.Errorf("read terminal upgrade handoff: %w", err)}
		}
		if resume.Version != 1 {
			return loaded{err: fmt.Errorf("unsupported terminal upgrade handoff version %d", resume.Version)}
		}
		return loaded{resume: &resume}
	}).Join(ctx)
	if err != nil {
		return nil, fmt.Errorf("load terminal upgrade handoff task failed: %w", err)
	}
	return result.resume, result.err
}

// restart writes the handoff and replaces this process with target's
// executable (exec on Unix; elsewhere it runs it and waits). Called only
// after the old terminal and runtime have been released. It returns only on
// failure, with the handoff preserved.
func (r *upgradeResume) restart(target UpgradeTarget) error {
	path := filepath.Join(core.DataDir(), "terminal-upgrades", fmt.Sprintf("%d.json", os.Getpid()))
	body, err := json.Marshal(r)
	if err != nil {
		return err
	}
	if err := core.AtomicWrite(path, body); err != nil {
		return fmt.Errorf("preserve terminal state for upgrade: %w", err)
	}
	fmt.Fprintln(os.Stderr, "Mjolnir upgraded; reconnecting this terminal.")
	environment := os.Environ()
	environment = setCommandEnv(environment, resumeEnv, path)
	environment = setCommandEnv(environment, "MJ_UPGRADE_DAEMON", target.Generation)
	environment = setCommandEnv(environment, "MJOLNIR_NO_UPDATE_CHECK", "1")
	if runtime.GOOS != "windows" {
		executable := target.Executable
		// Command::new searches PATH for a bare name; exec does not.
		if filepath.Base(executable) == executable {
			if found, err := exec.LookPath(executable); err == nil {
				executable = found
			}
		}
		err := syscall.Exec(executable, append([]string{target.Executable}, os.Args[1:]...), environment)
		return fmt.Errorf("restart upgraded terminal; state preserved at %s: %w", path, err)
	}
	command := exec.Command(target.Executable, os.Args[1:]...)
	command.Env = environment
	status, err := core.RunInteractive(command)
	if err != nil {
		return fmt.Errorf("restart upgraded terminal; state preserved at %s: %w", path, err)
	}
	if !status.Success() {
		return fmt.Errorf("upgraded terminal exited with %s", status)
	}
	return nil
}

// captureUpgrade collects this terminal's state for the handoff.
func (c *dashboardContext) captureUpgrade() upgradeResume {
	sessions := make([]string, 0, c.controller.State.Sessions.Len())
	for session := range c.controller.State.Sessions.Keys() {
		sessions = append(sessions, session)
	}
	for _, session := range sessions {
		c.captureComposerDraft(session)
		c.saveQuestionDraft(session)
		if parked := c.dashboard.StandbyQuestionDrafts(session); len(parked) > 0 {
			var ordinal uint64
			if active := c.chats[session]; active != nil {
				ordinal = active.LatestEventOrdinal()
			}
			for _, draft := range parked {
				c.questionDrafts[session] = append(c.questionDrafts[session], cachedQuestionDraft{
					Draft:                draft,
					CapturedEventOrdinal: ordinal,
				})
			}
		}
		if text, ok := c.dashboard.TakeStandbyPromptDraft(session); ok {
			c.composerDrafts.capture(session, text, c.controller.State.Sessions.Index(session).DraftInput)
		}
	}
	readPositions := make(map[string]uint64, c.controller.State.Sessions.Len())
	for id, session := range c.controller.State.Sessions.All() {
		readPositions[id] = session.ViewedThroughEventOrdinal
	}
	layouts := make(map[string]core.ConversationLayout, len(c.workspaceLayouts))
	for id, layout := range c.workspaceLayouts {
		layouts[id] = cloneConversationLayout(layout)
	}
	resume := upgradeResume{
		Version:       1,
		WorkspaceID:   c.workspaceID,
		ClientID:      c.clientID,
		Drafts:        c.composerDrafts,
		Questions:     c.questionDrafts,
		Positions:     c.transcriptPositions,
		Layouts:       layouts,
		ReadPositions: readPositions,
	}
	c.composerDrafts = composerDraftCache{}
	c.questionDrafts = map[string][]cachedQuestionDraft{}
	c.transcriptPositions = map[string]chat.TranscriptPosition{}
	return resume
}

// restoreUpgrade adopts a handoff's state, then removes its file. A startup
// error before that leaves it recoverable.
func (c *dashboardContext) restoreUpgrade(ctx context.Context, resume upgradeResume) {
	c.composerDrafts = resume.Drafts
	c.questionDrafts = resume.Questions
	if c.questionDrafts == nil {
		c.questionDrafts = map[string][]cachedQuestionDraft{}
	}
	c.transcriptPositions = resume.Positions
	if c.transcriptPositions == nil {
		c.transcriptPositions = map[string]chat.TranscriptPosition{}
	}
	for _, id := range sortedKeys(resume.Layouts) {
		layout := resume.Layouts[id]
		if _, known := c.knownWorkspaceLayouts[id]; known {
			c.dashboard.CacheWorkspaceLayout(id, cloneConversationLayout(layout))
			c.setWorkspaceLayout(id, layout)
		}
	}
	for _, id := range sortedKeys(resume.ReadPositions) {
		through := resume.ReadPositions[id]
		if session := c.controller.State.Sessions.GetMut(id); session != nil {
			session.ViewedThroughEventOrdinal = max(session.ViewedThroughEventOrdinal, through)
			if through, ok := c.readReceipts.acknowledge(id, through); ok {
				c.spawnReadReceipt(id, through)
			}
		}
	}
	// Only discard the handoff after its contents have reached the new
	// context. A startup error before this point leaves it recoverable.
	if path, ok := os.LookupEnv(resumeEnv); ok {
		removed, err := core.SpawnBlocking(func() error { return os.Remove(path) }).Join(ctx)
		if err == nil {
			err = removed
		}
		if err != nil {
			slog.Warn("could not remove restored terminal upgrade handoff", "result", err)
		}
	}
}
