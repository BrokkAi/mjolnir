// Port of mj-cli/src/api_client/fetch.rs.
//
// Disk-backed downloads. Retrying a GET after daemon replacement is safe;
// only a complete download can be published or written to stdout.

package cli

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
)

// fetch copies a path from the session's target, following a daemon
// replacement: after a handoff or a broken transport to a daemon that was
// replaced, it reconnects and downloads again.
func (c *apiClient) fetch(ctx context.Context, session, path string, list bool) (*checkpoint.FetchedPath, error) {
	panic("unported: mj-cli/src/api_client/fetch.rs ApiClient::fetch")
}

// fetchOnce downloads path into a spool file and checks it arrived whole.
func (c *apiClient) fetchOnce(ctx context.Context, session, path string, list bool) (*checkpoint.FetchedPath, error) {
	panic("unported: mj-cli/src/api_client/fetch.rs ApiClient::fetch_once")
}
