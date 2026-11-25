// Port of mj-worker-staging/src/worker_binary/image_worker.rs.

// The worker an agent-dev image carries, and the choice between it and a
// host binary.

package workerstaging

import (
	"encoding/json"
	"fmt"
	"log/slog"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
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
	if _, ok := locator.ContainerEngine(); !ok {
		binary, err := WorkerBinaryFor(locator, executor)
		if err != nil {
			return nil, err
		}
		return WorkerInstallHost{Path: binary}, nil
	}
	imageBuild, carried, err := ImageWorkerBuild(locator, executor)
	if err != nil {
		return nil, err
	}
	imageIsCurrent := carried && imageBuild == core.WorkerBuildID()
	if imageIsCurrent && core.ReleaseImageCarriesWorker() {
		return WorkerInstallImage{}, nil
	}
	binary, err := WorkerBinaryFor(locator, executor)
	if err == nil {
		return WorkerInstallHost{Path: binary}, nil
	}
	if imageIsCurrent {
		slog.Info("this build has no host worker; running the container image's worker of the same build",
			"error", err.Error())
		return WorkerInstallImage{}, nil
	}
	described := "no worker"
	if carried {
		described = "worker build " + imageBuild
	}
	return nil, fmt.Errorf("the container's image carries %s, not this Mjolnir's worker build %s, and no host worker can replace it: %w",
		described, core.WorkerBuildID(), err)
}

// ImageWorkerBuild is the worker build a container's image carries at
// ImageWorker; false when the image has no worker there.
func ImageWorkerBuild(locator *core.TargetsTargetLocator, executor core.CommandExecutor) (string, bool, error) {
	command := core.LocatorCommand(core.SshTrafficClassControl, locator, []string{"sh", "-c", imageWorkerBuildScript}).
		WithPurpose("read the container image's worker build")
	output, err := targets.ExecuteChecked(executor, command)
	if err != nil {
		return "", false, err
	}
	return parseImageWorkerBuild(output.Stdout)
}

func parseImageWorkerBuild(stdout []byte) (string, bool, error) {
	text, err := binarySelectFromUTF8(stdout)
	if err != nil {
		return "", false, fmt.Errorf("the image's worker build is not UTF-8: %w", err)
	}
	text = strings.TrimSpace(text)
	if text == "absent" {
		return "", false, nil
	}
	if !core.IsWorkerBuild(text) {
		return "", false, fmt.Errorf("the image's worker reported an unrecognized build %s", core.RustDebugString(text))
	}
	return text, true, nil
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
	var images []json.RawMessage
	if json.Unmarshal(inspection, &images) != nil || len(images) == 0 {
		return "", false
	}
	var image any
	if json.Unmarshal(images[0], &image) != nil {
		return "", false
	}
	for _, pointer := range [][]string{{"Config", "Labels"}, {"Labels"}} {
		labels, ok := jsonPointer(image, pointer)
		if !ok {
			continue
		}
		object, ok := labels.(map[string]any)
		if !ok {
			continue
		}
		label, ok := object[WorkerBuildLabel]
		if !ok {
			continue
		}
		build, ok := label.(string)
		if !ok || build == "" {
			return "", false
		}
		return build, true
	}
	return "", false
}

// jsonPointer is serde_json's `Value::pointer` over object keys.
func jsonPointer(value any, keys []string) (any, bool) {
	for _, key := range keys {
		object, ok := value.(map[string]any)
		if !ok {
			return nil, false
		}
		if value, ok = object[key]; !ok {
			return nil, false
		}
	}
	return value, true
}
