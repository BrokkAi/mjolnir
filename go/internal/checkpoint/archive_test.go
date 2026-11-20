// Port of mj-checkpoint/src/archive/tests.rs, with the archive.rs helpers
// that Rust compiles only for tests.
//
// Declarations phase: only the helpers other test files use are declared;
// the tests themselves are ported with the bodies.

package checkpoint

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func prepareArchive(input *ArchiveInput) (core.ArchiveManifest, []pendingPayload, error) {
	return prepareArchiveWithPartSize(input, payloadPartBytes)
}

func prepareArchiveWithPartSize(input *ArchiveInput, partBytes int) (core.ArchiveManifest, []pendingPayload, error) {
	return prepareArchiveViewWithPartSize(input.view(), partBytes)
}

// initializeRepository creates a Git repository at path with branch main
// and a test identity.
func initializeRepository(t testing.TB, path string) {
	t.Helper()
	panic("unported: mj-checkpoint/src/archive/tests.rs initialize_repository")
}

// cloneRepository clones origin as parent/name with a test identity and
// returns the clone's path.
func cloneRepository(t testing.TB, parent, origin, name string) string {
	t.Helper()
	panic("unported: mj-checkpoint/src/archive/tests.rs clone_repository")
}

// input is the archive input the archive tests start from: a Codex session
// with a transcript, a queued prompt, one native artifact and the
// repositories "hel" and "worker".
func input() ArchiveInput {
	panic("unported: mj-checkpoint/src/archive/tests.rs input")
}
