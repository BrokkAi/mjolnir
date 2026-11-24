// Port of mj-worker-staging/src/worker_binary/test_fixtures.rs.

// Stand-ins the staging tests, and the tests of the packages above that
// stage a worker, build on. Rust keeps this module public (doc-hidden) so
// the crates above can use it; Go test files cannot be imported by other
// packages, so it is an ordinary file here too. Failures panic, as Rust's
// unwrap does.

package workerstaging

import (
	"os"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func StampedWorker(body []byte) []byte {
	bytes := append([]byte{}, body...)
	return append(bytes, core.WorkerBuildStamp()...)
}

// PodmanInstallFixture is a stamped worker, launch config, ownership record
// and profile stage in a temporary directory, for an SSH Podman target.
// Close removes the directory (Rust: dropping the TempDir).
type PodmanInstallFixture struct {
	root         string
	WorkerBinary string
	LaunchConfig string
	Ownership    string
	ProfileStage string
	Locator      core.TargetsTargetLocator
}

// Close removes the fixture's directory.
func (f *PodmanInstallFixture) Close() {
	_ = os.RemoveAll(f.root)
}

// NewPodmanInstallFixture is Rust's `podman_install_fixture` (renamed: Go
// cannot have a function and a type both named PodmanInstallFixture).
func NewPodmanInstallFixture() *PodmanInstallFixture {
	root, err := os.MkdirTemp("", ".tmp")
	fixtureMust(err)
	workerBinary := filepath.Join(root, "hel")
	fixtureMust(os.WriteFile(workerBinary, StampedWorker([]byte("worker-binary-bytes")), 0o666))
	launchConfig := filepath.Join(root, "launch.json")
	fixtureMust(os.WriteFile(launchConfig, []byte("{}"), 0o666))
	ownership := filepath.Join(root, "ownership.json")
	fixtureMust(os.WriteFile(ownership, []byte("{}"), 0o666))
	profileStage := filepath.Join(root, "profile")
	fixtureMust(os.MkdirAll(profileStage, 0o777))
	return &PodmanInstallFixture{
		root:         root,
		WorkerBinary: workerBinary,
		LaunchConfig: launchConfig,
		Ownership:    ownership,
		ProfileStage: profileStage,
		Locator: core.TargetsTargetLocator{
			Kind:        core.TargetsTargetLocatorSshPodman,
			SSH:         &core.SshTarget{Destination: "user@example.test"},
			ContainerID: strings.Repeat("a", 64),
		},
	}
}

func fixtureMust(err error) {
	if err != nil {
		panic(err)
	}
}

// WriteOpencodeDatabase writes an OpenCode 2 database holding one session
// and one API-key login with key, in the `credential` table layout OpenCode
// 2.1 creates, replacing any file at path.
func WriteOpencodeDatabase(path, key string) {
	panic("unported: mj-worker-staging/src/worker_binary/test_fixtures.rs write_opencode_database")
}

// LoginBytes is a login as each harness writes it. A higher generation is a
// fresher copy of the same grant, which is what the credential sync orders
// copies by.
func LoginBytes(kind core.HarnessKind, generation int64) []byte {
	panic("unported: mj-worker-staging/src/worker_binary/test_fixtures.rs login_bytes")
}
