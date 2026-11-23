// Port of mj-github-watch/src/service.rs's tests and test-only items.

package githubwatch

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// alwaysEnabledPollControl is Rust's test-only PollControl::always_enabled.
func alwaysEnabledPollControl() pollControl {
	return newPollControl(func() bool { return true })
}

// newGithubAPI is Rust's test-only GithubApi::new.
func newGithubAPI(base string, token *string) (*githubAPI, error) {
	return newGithubAPIWithControl(base, token, alwaysEnabledPollControl())
}

// sessionDigest is Rust's test-only session_digest.
func sessionDigest(sessionID string) (core.SessionContext, error) {
	return spawnBlocking("load session turns for GitHub classification", func() (core.SessionContext, error) {
		materialized, err := store.LoadMaterializedRecentTurns(sessionID, core.SessionTurns)
		if err != nil || materialized == nil {
			return core.SessionContext{}, err
		}
		return transcript.SessionDigest(materialized), nil
	})
}
