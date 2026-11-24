// Port of mj-worker-staging/src/worker_binary/binary_select.rs.

package workerstaging

import (
	"errors"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// DisplayPath: Linux appends " (deleted)" to `/proc/<pid>/exe` for a
// removed image. That marker belongs in a message but never in a decision,
// which isFile makes.
func DisplayPath(path string) string {
	return strings.TrimSuffix(path, " (deleted)")
}

// TemplateArchitecture is the architecture a configured template names
// outright, if it names one. A container `platform` such as `linux/arm64`
// decides what the target runs whatever the controller's own machine is,
// and it is the only architecture a configured target template can state:
// the configured AwsEc2 variant names a launch template, whose instance
// type is only discoverable through the AWS API.
func TemplateArchitecture(template core.TargetTemplate) (string, bool) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_select.rs template_architecture")
}

// PreflightArchitectures is the architectures a resume must be able to
// serve, knowing only the configured template. Provisioning learns the real
// answer by running `uname -m` on the live target; a resume has no target
// yet, so this uses what is knowable without one: an architecture the
// template names, else the controller's own architecture for a target that
// runs on this machine, else either Linux architecture for a remote target.
func PreflightArchitectures(template core.TargetTemplate) []string {
	panic("unported: mj-worker-staging/src/worker_binary/binary_select.rs preflight_architectures")
}

// ReleaseImageSuppliesWorker is whether a container on this template gets
// its worker from the image: it names this release's own default image,
// which the release workflow published with this build's worker.
// Installation still asks the image's worker for its build before linking
// it (see WorkerInstallForTarget).
func ReleaseImageSuppliesWorker(template core.TargetTemplate) bool {
	if !core.ReleaseImageCarriesWorker() {
		return false
	}
	container := template.Container()
	return container != nil && container.Image == core.ReleaseContainerImage
}

// PreflightWorkerBinary resolves the worker before expensive provisioning
// or transcript compaction. Existing SSH bare hosts are probed; disposable
// targets use their template.
//
// A resume compacts a cross-harness transcript before it provisions
// anything, which costs minutes and paid model requests. Resolving the
// worker binary is performed before target creation, so a resume that could
// never install a worker must fail before spending any of that. Remote
// sources are downloaded and verified here too, before provisioning can
// create a container.
func PreflightWorkerBinary(template core.TargetTemplate, executor core.CommandExecutor) error {
	if ReleaseImageSuppliesWorker(template) {
		return nil
	}
	return PreflightHostWorkerBinary(template, executor)
}

// PreflightHostWorkerBinary is PreflightWorkerBinary without trusting a
// release's image: a worker of this build must be available from the host.
func PreflightHostWorkerBinary(template core.TargetTemplate, executor core.CommandExecutor) error {
	panic("unported: mj-worker-staging/src/worker_binary/binary_select.rs preflight_host_worker_binary")
}

// WorkerSourceProblem is why no worker binary could serve a session on this
// target; false when one can (or when that cannot be told without a host
// that does not answer).
//
// This is the resolution PreflightWorkerBinary performs, without the
// download or the build check, so the daemon can refuse a launch up front
// instead of admitting it and failing the session seconds later. An
// existing SSH host is asked its platform; a host that does not answer, or
// answers with a platform Mjolnir does not support, is not a worker-source
// problem and is left to the launch to report.
func WorkerSourceProblem(template core.TargetTemplate, executor core.CommandExecutor) (string, bool) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_select.rs worker_source_problem")
}

// SSHWorkerPrerequisite is the worker an existing SSH host needs, with the
// target triple it is named by (Rust: `(String,
// Result<WorkerBinaryAvailability>)`).
type SSHWorkerPrerequisite struct {
	Triple       string
	Availability WorkerBinaryAvailability
	Err          error
}

// SSHWorkerBinaryPrerequisite is the worker an existing SSH host needs,
// read from the host's own platform. Nil for a template that is not a bare
// SSH host and for a host that does not say what it runs; the SSH
// reachability check reports the latter.
func SSHWorkerBinaryPrerequisite(template core.TargetTemplate, executor core.CommandExecutor) *SSHWorkerPrerequisite {
	panic("unported: mj-worker-staging/src/worker_binary/binary_select.rs ssh_worker_binary_prerequisite")
}

func WorkerBinaryFor(locator *core.TargetsTargetLocator, executor core.CommandExecutor) (string, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_select.rs worker_binary_for")
}

func materializeWorkerSource(source WorkerBinaryAvailability) (string, error) {
	var path string
	switch source := source.(type) {
	case WorkerBinaryAvailabilityLocal:
		path = source.Path
	case WorkerBinaryAvailabilityRemote:
		downloaded, err := DownloadWorker(source.URL, source.SHA256, source.Triple)
		if err != nil {
			return "", err
		}
		path = downloaded
	default:
		return "", errors.New("no worker source")
	}
	if err := core.VerifyWorkerBuild(path); err != nil {
		return "", err
	}
	return path, nil
}

func TargetPlatform(locator *core.TargetsTargetLocator, executor core.CommandExecutor) (targets.TargetPlatform, error) {
	return probePlatform(executor, targets.PlatformProbe(locator))
}

func probePlatform(executor core.CommandExecutor, command core.CommandSpec) (targets.TargetPlatform, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_select.rs probe_platform")
}

func DownloadWorker(url, expectedSHA256, triple string) (string, error) {
	panic("unported: mj-worker-staging/src/worker_binary/binary_select.rs download_worker")
}

func ValidateWorkerSHA256(expectedSHA256 string) error {
	valid := len(expectedSHA256) == 64
	for index := 0; valid && index < len(expectedSHA256); index++ {
		b := expectedSHA256[index]
		valid = (b >= '0' && b <= '9') || (b >= 'a' && b <= 'f') || (b >= 'A' && b <= 'F')
	}
	if !valid {
		return errors.New("MJ_WORKER_SHA256 must be a 64-character hexadecimal digest")
	}
	return nil
}
