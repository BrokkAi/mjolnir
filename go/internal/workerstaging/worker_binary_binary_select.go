// Port of mj-worker-staging/src/worker_binary/binary_select.rs.

package workerstaging

import (
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"runtime"
	"strings"
	"time"

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
	container := template.Container()
	if container == nil || container.Platform == nil {
		return "", false
	}
	// Platform strings appear as "linux/arm64", "arm64", or "linux/arm64/v8".
	for _, part := range strings.Split(*container.Platform, "/") {
		if arch, err := targets.NormalizeArchitecture(strings.TrimSpace(part)); err == nil {
			return arch, true
		}
	}
	return "", false
}

// PreflightArchitectures is the architectures a resume must be able to
// serve, knowing only the configured template. Provisioning learns the real
// answer by running `uname -m` on the live target; a resume has no target
// yet, so this uses what is knowable without one: an architecture the
// template names, else the controller's own architecture for a target that
// runs on this machine, else either Linux architecture for a remote target.
func PreflightArchitectures(template core.TargetTemplate) []string {
	if arch, ok := TemplateArchitecture(template); ok {
		return []string{arch}
	}
	switch template.(type) {
	case core.TargetTemplateLocalBare, core.TargetTemplateLocalPodman, core.TargetTemplateLocalDocker,
		core.TargetTemplateAppleContainer:
		return []string{rustArch()}
	case core.TargetTemplateSshAppleContainer:
		// Apple container runs only native Apple-silicon images, so a remote
		// one is arm64 by definition.
		return []string{"aarch64"}
	default:
		return []string{"x86_64", "aarch64"}
	}
}

// workerRequirementForTemplate is the worker a template's sessions need
// when no host answers for it: the native one on this host, otherwise the
// portable Linux one.
func workerRequirementForTemplate(template core.TargetTemplate) WorkerBinaryRequirement {
	if _, ok := template.(core.TargetTemplateLocalBare); ok {
		return WorkerBinaryRequirementLocalHost
	}
	return WorkerBinaryRequirementPortableLinux
}

// sshPlatformProbe asks an existing SSH host what it runs.
func sshPlatformProbe(executor core.CommandExecutor, template core.TargetTemplateSshBare) (targets.TargetPlatform, error) {
	ssh := core.SshTargetFromSshConnection(&template.SSH)
	command := core.SSHCommand(core.SshTrafficClassControl, &ssh, []string{"uname", "-sm"}).
		WithPurpose("detect target platform")
	return probePlatform(executor, command)
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
	if sshBare, ok := template.(core.TargetTemplateSshBare); ok {
		platform, err := sshPlatformProbe(executor, sshBare)
		if err != nil {
			return err
		}
		availability, err := WorkerBinaryForArch(platform.Architecture, WorkerBinaryRequirementForOs(platform.Os))
		if err != nil {
			return err
		}
		_, err = materializeWorkerSource(availability)
		return err
	}
	// Existing SSH bare hosts were resolved above. Containers and the
	// disposable EC2 backend require portable Linux workers.
	requirement := workerRequirementForTemplate(template)
	var failure error
	for _, arch := range PreflightArchitectures(template) {
		availability, err := WorkerBinaryForArch(arch, requirement)
		if err == nil {
			_, err = materializeWorkerSource(availability)
		}
		if err == nil {
			return nil
		}
		failure = err
	}
	if failure != nil {
		// The message is the one provisioning would have printed later, so
		// the user reads the same fix, sooner.
		return fmt.Errorf("preflight the worker binary before provisioning: %w", failure)
	}
	return nil
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
	if ReleaseImageSuppliesWorker(template) {
		return "", false
	}
	if sshBare, ok := template.(core.TargetTemplateSshBare); ok {
		platform, err := sshPlatformProbe(executor, sshBare)
		if err != nil {
			return "", false
		}
		if _, err := WorkerBinaryForArch(platform.Architecture, WorkerBinaryRequirementForOs(platform.Os)); err != nil {
			return err.Error(), true
		}
		return "", false
	}
	requirement := workerRequirementForTemplate(template)
	failure, failed := "", false
	for _, arch := range PreflightArchitectures(template) {
		if _, err := WorkerBinaryForArch(arch, requirement); err != nil {
			failure, failed = err.Error(), true
			continue
		}
		return "", false
	}
	return failure, failed
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
	sshBare, ok := template.(core.TargetTemplateSshBare)
	if !ok {
		return nil
	}
	platform, err := sshPlatformProbe(executor, sshBare)
	if err != nil {
		return nil
	}
	requirement := WorkerBinaryRequirementForOs(platform.Os)
	availability, err := WorkerBinaryForArch(platform.Architecture, requirement)
	return &SSHWorkerPrerequisite{Triple: requirement.Triple(platform.Architecture), Availability: availability, Err: err}
}

func WorkerBinaryFor(locator *core.TargetsTargetLocator, executor core.CommandExecutor) (string, error) {
	platform, err := TargetPlatform(locator, executor)
	if err != nil {
		return "", err
	}
	requirement := WorkerBinaryRequirementForOs(platform.Os)
	if locator.Kind == core.TargetsTargetLocatorLocalBare {
		requirement = WorkerBinaryRequirementLocalHost
	}
	availability, err := WorkerBinaryForArch(platform.Architecture, requirement)
	if err != nil {
		return "", err
	}
	return materializeWorkerSource(availability)
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
	output, err := targets.ExecuteChecked(executor, command)
	if err != nil {
		return targets.TargetPlatform{}, err
	}
	text, err := binarySelectFromUTF8(output.Stdout)
	if err != nil {
		return targets.TargetPlatform{}, err
	}
	return targets.TargetPlatformParse(text)
}

// binarySelectFromUTF8 is Rust's `std::str::from_utf8`, failing with the
// text of Rust's Utf8Error ("invalid utf-8 sequence of 1 bytes from index
// 3", "incomplete utf-8 byte sequence from index 3").
func binarySelectFromUTF8(text []byte) (string, error) {
	index := 0
	for index < len(text) {
		first := text[index]
		if first < 0x80 {
			index++
			continue
		}
		width := 0
		switch {
		case first >= 0xC2 && first <= 0xDF:
			width = 2
		case first >= 0xE0 && first <= 0xEF:
			width = 3
		case first >= 0xF0 && first <= 0xF4:
			width = 4
		}
		if width == 0 {
			return "", fmt.Errorf("invalid utf-8 sequence of 1 bytes from index %d", index)
		}
		low, high := byte(0x80), byte(0xBF)
		switch first {
		case 0xE0:
			low = 0xA0
		case 0xED:
			high = 0x9F
		case 0xF0:
			low = 0x90
		case 0xF4:
			high = 0x8F
		}
		for offset := 1; offset < width; offset++ {
			if index+offset >= len(text) {
				return "", fmt.Errorf("incomplete utf-8 byte sequence from index %d", index)
			}
			next := text[index+offset]
			if offset == 1 && (next < low || next > high) || offset > 1 && (next < 0x80 || next > 0xBF) {
				return "", fmt.Errorf("invalid utf-8 sequence of %d bytes from index %d", offset, index)
			}
		}
		index += width
	}
	return string(text), nil
}

func DownloadWorker(url, expectedSHA256, triple string) (string, error) {
	if err := ValidateWorkerSHA256(expectedSHA256); err != nil {
		return "", err
	}
	digest := strings.ToLower(expectedSHA256)
	directory := pathJoin(core.DataDir(), "workers", "pinned")
	destination := pathJoin(directory, digest, "hel")
	parent, ok := pathParent(destination)
	if !ok {
		parent = directory
	}
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return "", rustIOError(err)
	}
	if isFile(destination) {
		actual, err := core.WorkerExecutableDigest(destination)
		if err != nil {
			return "", fmt.Errorf("read cached worker for %s from %s: %w", triple, destination, err)
		}
		if actual == digest {
			if err := core.VerifyWorkerBuild(destination); err != nil {
				return "", err
			}
			return destination, nil
		}
		return "", fmt.Errorf("content-addressed worker cache %s does not match %s checksum", destination, expectedSHA256)
	}
	body, err := onDedicatedThread(func() ([]byte, error) {
		client := &http.Client{Timeout: 120 * time.Second}
		response, err := client.Get(url)
		if err != nil {
			return nil, err
		}
		defer response.Body.Close()
		if response.StatusCode >= 400 {
			class := "client error"
			if response.StatusCode >= 500 {
				class = "server error"
			}
			return nil, fmt.Errorf("HTTP status %s (%s) for url (%s)", class, response.Status, url)
		}
		return io.ReadAll(response.Body)
	})
	if err != nil {
		return "", err
	}
	sum := sha256.Sum256(body)
	actual := core.LowerHex(sum[:])
	if !strings.EqualFold(actual, expectedSHA256) {
		return "", fmt.Errorf("downloaded worker checksum mismatch: expected %s, got %s", expectedSHA256, actual)
	}
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return "", rustIOError(err)
	}
	temporary, err := os.CreateTemp(directory, ".tmp")
	if err != nil {
		return "", rustIOError(err)
	}
	prepare := func() error {
		if _, err := temporary.Write(body); err != nil {
			return err
		}
		if err := temporary.Sync(); err != nil {
			return err
		}
		if runtime.GOOS != "windows" {
			return os.Chmod(temporary.Name(), 0o700)
		}
		return nil
	}
	if err := prepare(); err != nil {
		temporary.Close()
		os.Remove(temporary.Name())
		return "", rustIOError(err)
	}
	return PublishCachedWorker(temporary, directory, digest)
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
