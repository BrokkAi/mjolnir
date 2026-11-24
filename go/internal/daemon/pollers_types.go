// Port of mj-daemon/src/pollers/types.rs (only WorkerPollUpdate and
// WorkerDiagnosisTracker so far).

package daemon

import "github.com/BrokkAi/mjolnir/go/internal/sessionruntime"

// WorkerPollUpdate is one session's latest view from the session manager.
type WorkerPollUpdate = sessionruntime.SessionManagerUpdate

// workerDiagnosisEpisode is one stretch of a worker failing.
type workerDiagnosisEpisode struct {
	id        uint64
	error     string
	diagnosed bool
}

// WorkerDiagnosisTracker decides when a failing worker is diagnosed: once
// per failure episode. The zero value is Rust's Default.
type WorkerDiagnosisTracker struct {
	nextEpisode uint64
	current     map[string]workerDiagnosisEpisode
	pending     map[string]uint64
}

// WorkerDiagnosisCompletion is what a finished diagnosis asks of its
// caller: the error to show, and an episode to diagnose again; each nil
// when none.
type WorkerDiagnosisCompletion struct {
	DisplayError   *string
	RestartEpisode *uint64
}

// Observe records one poll of a session's worker. It answers the episode
// to diagnose when this poll starts one that is neither diagnosed nor
// already being diagnosed.
func (t *WorkerDiagnosisTracker) Observe(sessionID string, connected bool, err *string) (uint64, bool) {
	if connected || err == nil {
		delete(t.current, sessionID)
	}
	if err == nil {
		return 0, false
	}
	if t.current == nil {
		t.current = map[string]workerDiagnosisEpisode{}
	}
	episode, ok := t.current[sessionID]
	if !ok {
		t.nextEpisode = max(t.nextEpisode+1, 1)
		episode = workerDiagnosisEpisode{id: t.nextEpisode, error: *err}
	}
	episode.error = *err
	t.current[sessionID] = episode
	if episode.diagnosed {
		return 0, false
	}
	if _, pending := t.pending[sessionID]; pending {
		return 0, false
	}
	if t.pending == nil {
		t.pending = map[string]uint64{}
	}
	t.pending[sessionID] = episode.id
	return episode.id, true
}

// Finish records that the diagnosis of episodeID ended.
func (t *WorkerDiagnosisTracker) Finish(sessionID string, episodeID uint64) WorkerDiagnosisCompletion {
	if pending, ok := t.pending[sessionID]; !ok || pending != episodeID {
		return WorkerDiagnosisCompletion{}
	}
	delete(t.pending, sessionID)
	current, ok := t.current[sessionID]
	if !ok {
		return WorkerDiagnosisCompletion{}
	}
	if current.id == episodeID {
		current.diagnosed = true
		t.current[sessionID] = current
		display := current.error
		return WorkerDiagnosisCompletion{DisplayError: &display}
	}
	if !current.diagnosed {
		t.pending[sessionID] = current.id
		restart := current.id
		return WorkerDiagnosisCompletion{RestartEpisode: &restart}
	}
	return WorkerDiagnosisCompletion{}
}
