//go:build linux

// Port of mj-worker/src/microvm/root_check.rs.
//
// Check a flattened image's root filesystem for what a retained-storage
// upgrade needs, without booting it or running anything from it.
//
// The controller's Podman preflight runs `command -v tar`, `command -v flock`,
// `mkdir -p /var/lib/hel && test -w /var/lib/hel` in a throwaway container of
// the new image before it stops anything. A throwaway VM would be charged
// against the memory pool and would need its own cleanup after a crash, so the
// engine answers the same questions from the image's disk: the base `root.ext4`
// is mounted read-only in a private mount namespace (it disappears with the
// process, so no mount or loop device can outlive the check) and the
// questions are answered by walking it with the symlink rules of a chroot.
// Nothing in the image is executed.

package worker

import (
	"fmt"
	"os"
	"strings"
)

// microvmRequiredPrograms are the programs the control-state transfer runs
// in the guest.
var microvmRequiredPrograms = [3]string{"sh", "tar", "flock"}

// microvmTarOptions are the options of `restore_control_state` and
// `capture_control_state`, as the controller's tar probe names them.
var microvmTarOptions = [5]string{
	"one-file-system",
	"unlink-first",
	"strip-components",
	"no-recursion",
	"occurrence",
}

// microvmControlRoot is where the worker keeps the state the transfer
// carries.
const microvmControlRoot = "/var/lib/hel"

// microvmMaxLinks is the kernel's limit on symlinks followed in one lookup.
const microvmMaxLinks = 40

// MicrovmCheckImage refuses the replacement of a VM that runs as user with
// environment path by the image imageID unless the transfer can run in it.
func MicrovmCheckImage(layout MicrovmLayout, imageID string, user string, path string) error {
	panic("unported: mj-worker/src/microvm/root_check.rs check_image")
}

// MicrovmCheckMounted is the inner half of MicrovmCheckImage, run inside
// the mount namespace with the image mounted at root.
func MicrovmCheckMounted(root string, path string) error {
	problems := microvmRootCheckProblems(root, path)
	if len(problems) == 0 {
		return nil
	}
	return fmt.Errorf("the new image cannot carry the worker's state over: %s", strings.Join(problems, "; "))
}

func microvmRootCheckProblems(root string, path string) []string {
	panic("unported: mj-worker/src/microvm/root_check.rs problems")
}

// microvmMissingTarOptions: the restore runs `tar` with these long options,
// and the source-side preflight looks for them in `tar --help`. GNU tar
// carries its option names in its own binary, BusyBox's tar lacks
// `unlink-first` and `occurrence`, so the static counterpart of that check
// is a search of the binary, which nothing executes.
func microvmMissingTarOptions(tar string) []string {
	panic("unported: mj-worker/src/microvm/root_check.rs missing_tar_options")
}

func microvmIsExecutableFile(file string) bool {
	info, err := os.Stat(file)
	return err == nil && info.Mode().IsRegular() && info.Mode().Perm()&0o111 != 0
}

// microvmRootCheckResolve is path as a chroot at root would find it,
// following symlinks inside the image only; ok is false when it does not
// exist. An absolute link restarts at root and `..` never leaves it.
func microvmRootCheckResolve(root string, path string) (resolved string, ok bool, err error) {
	panic("unported: mj-worker/src/microvm/root_check.rs resolve")
}

// microvmRootCheckComponents is Rust's Path::components of path, keeping
// the normal names and `..` only.
func microvmRootCheckComponents(path string) []string {
	var parts []string
	for _, part := range strings.Split(path, "/") {
		if part != "" && part != "." {
			parts = append(parts, part)
		}
	}
	return parts
}

// microvmCreatableDirectory is what `mkdir -p path && test -w path` needs
// of the image for root: every existing component is a directory (through
// links), and the rest can be created.
func microvmCreatableDirectory(root string, path string) error {
	panic("unported: mj-worker/src/microvm/root_check.rs creatable_directory")
}
