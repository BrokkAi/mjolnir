// Port of mj-worker-staging/src/worker_binary/image_worker.rs.

// The worker an agent-dev image carries, and the choice between it and a
// host binary.

package workerstaging

import (
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ImageWorker is where an agent-dev image keeps the worker built from its
// own commit.
const ImageWorker = "/opt/mjolnir/mj-worker"

// imageWorkerBuildScript prints the build of the image's worker, or
// `absent` when the image has none. A worker that is present but cannot run
// fails the command, so an unusable worker is reported rather than mistaken
// for a missing one.
const imageWorkerBuildScript = "if [ -e /opt/mjolnir/mj-worker ]; then exec /opt/mjolnir/mj-worker worker build-id; fi; echo absent"

// WorkerInstall is how a session's target gets the worker this controller
// needs: WorkerInstallImage or WorkerInstallHost. Either way the worker
// carries core.WorkerBuildID. Rust enum; the variants are comparable with
// ==.
type WorkerInstall interface {
	isWorkerInstall()
	// HostBinary is the host binary this install copies, if it copies one.
	HostBinary() (string, bool)
}

// WorkerInstallImage: the container's image carries this controller's
// worker build. The session's `hel` links to it and nothing is copied.
type WorkerInstallImage struct{}

// WorkerInstallHost: a host binary carrying this controller's build is
// copied in.
type WorkerInstallHost struct {
	Path string
}

func (WorkerInstallImage) isWorkerInstall() {}
func (WorkerInstallHost) isWorkerInstall()  {}

func (WorkerInstallImage) HostBinary() (string, bool) { return "", false }
func (w WorkerInstallHost) HostBinary() (string, bool) {
	return w.Path, true
}

// WorkerInstallForTarget is Rust's `WorkerInstall::for_target`: choose the
// worker for a target.
//
// A release runs its container image's worker when that is this build. Any
// other build prefers the worker it was built with: its identity covers
// committed inputs only, so an image worker reporting the same build can
// still lack a developer's uncommitted changes. It runs the image's worker
// only when it has none of its own. A custom or older image gets the host
// binary, which must carry this build too.
func WorkerInstallForTarget(locator *core.TargetsTargetLocator, executor core.CommandExecutor) (WorkerInstall, error) {
	panic("unported: mj-worker-staging/src/worker_binary/image_worker.rs WorkerInstall::for_target")
}

// ImageWorkerBuild is the worker build a container's image carries at
// ImageWorker; false when the image has no worker there.
func ImageWorkerBuild(locator *core.TargetsTargetLocator, executor core.CommandExecutor) (string, bool, error) {
	panic("unported: mj-worker-staging/src/worker_binary/image_worker.rs image_worker_build")
}

func parseImageWorkerBuild(stdout []byte) (string, bool, error) {
	panic("unported: mj-worker-staging/src/worker_binary/image_worker.rs parse_image_worker_build")
}

// WorkerBuildLabel is the label `containers/Containerfile.agent-dev` gives
// the worker it bakes in. The image build fails unless the worker reports
// exactly this build.
const WorkerBuildLabel = "org.brokk.mjolnir.worker-build"

// ImageWorkerLabel is the worker build an image's `image inspect` JSON is
// labelled with, from Docker's or Podman's shape; false when it has none or
// cannot be read. Sessions decide by asking the worker itself; this is for
// planning before a container exists.
func ImageWorkerLabel(inspection []byte) (string, bool) {
	panic("unported: mj-worker-staging/src/worker_binary/image_worker.rs image_worker_label")
}
