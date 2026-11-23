// Port of the #[cfg(not(unix))] arm of reviewer_environment in
// mj-worker/src/acp/guardian/reviewer.rs. json_gh_rules (which stages an
// OpenCode reviewer's profile) is Unix-only in Rust and in this port.

//go:build !unix

package worker

import "errors"

func opencodeReviewerEnvironment(map[string]string) (map[string]string, error) {
	return nil, errors.New("an OpenCode reviewer needs a Unix worker")
}
