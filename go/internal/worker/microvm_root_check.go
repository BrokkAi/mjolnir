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
	"bytes"
	"errors"
	"fmt"
	"os"
	"path/filepath"
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
	switch userPart, _, _ := strings.Cut(user, ":"); userPart {
	case "", "0", "root":
	default:
		return fmt.Errorf("the image check supports sandboxes that run as root, not %s", microvmDebugString(user))
	}
	disk := MicrovmRootImage(layout, imageID)
	if !microvmIsFile(disk) {
		return fmt.Errorf("image %s has no root disk", imageID)
	}
	unshare, err := MicrovmFindTool("unshare")
	if err != nil {
		return err
	}
	mount, err := MicrovmFindTool("mount")
	if err != nil {
		return err
	}
	engine, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate the engine binary: %w", err)
	}
	directory := filepath.Join("/run/mj-microvm-check", MicrovmScratchSuffix())
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return fmt.Errorf("create %s: %w", directory, microvmIOError(err))
	}
	const script = `set -eu; "$1" -o loop,ro,noload,nodev,nosuid,noexec "$2" "$3"; exec "$4" microvm root-check-inner "$3" "$5"`
	output, outputErr := MicrovmRunUnchecked(unshare, []string{
		"--mount",
		"--propagation",
		"private",
		"--",
		"sh",
		"-c",
		script,
		"mj-root-check",
		mount,
		disk,
		directory,
		engine,
		path,
	})
	// The mount went with the namespace; only the empty mount point is left.
	removed := os.Remove(directory)
	if outputErr != nil {
		return outputErr
	}
	if removed != nil {
		return fmt.Errorf("remove %s: %w", directory, microvmIOError(removed))
	}
	if !output.Status.Success() {
		// The inner process reports as an engine failure; the caller adds its own.
		reason := MicrovmTail(microvmLossy(output.Stderr), 20)
		for strings.HasPrefix(reason, "Error: ") {
			reason = strings.TrimPrefix(reason, "Error: ")
		}
		return errors.New(reason)
	}
	return nil
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
	problems := []string{}
	for _, program := range microvmRequiredPrograms {
		found, ok := "", false
		for _, directory := range strings.Split(path, ":") {
			if directory == "" {
				directory = "."
			}
			file, exists, err := microvmRootCheckResolve(root, directory+"/"+program)
			if err == nil && exists && microvmIsExecutableFile(file) {
				found, ok = file, true
				break
			}
		}
		switch {
		case !ok:
			problems = append(problems, fmt.Sprintf("`%s` is not an executable on PATH (%s)", program, path))
		case program == "tar":
			if missing := microvmMissingTarOptions(found); len(missing) > 0 {
				problems = append(problems, fmt.Sprintf("its `tar` lacks the options the restore uses (%s); it needs GNU tar", strings.Join(missing, ", ")))
			}
		}
	}
	if reason := microvmCreatableDirectory(root, microvmControlRoot); reason != nil {
		problems = append(problems, fmt.Sprintf("%s cannot be created: %s", microvmControlRoot, reason.Error()))
	}
	return problems
}

// microvmMissingTarOptions: the restore runs `tar` with these long options,
// and the source-side preflight looks for them in `tar --help`. GNU tar
// carries its option names in its own binary, BusyBox's tar lacks
// `unlink-first` and `occurrence`, so the static counterpart of that check
// is a search of the binary, which nothing executes.
func microvmMissingTarOptions(tar string) []string {
	binary, err := os.ReadFile(tar)
	if err != nil {
		return microvmTarOptions[:]
	}
	var missing []string
	for _, option := range microvmTarOptions {
		if !bytes.Contains(binary, []byte(option)) {
			missing = append(missing, option)
		}
	}
	return missing
}

func microvmIsExecutableFile(file string) bool {
	info, err := os.Stat(file)
	return err == nil && info.Mode().IsRegular() && info.Mode().Perm()&0o111 != 0
}

// microvmRootCheckResolve is path as a chroot at root would find it,
// following symlinks inside the image only; ok is false when it does not
// exist. An absolute link restarts at root and `..` never leaves it.
func microvmRootCheckResolve(root string, path string) (resolved string, ok bool, err error) {
	root = filepath.Clean(root)
	current := root
	pending := microvmRootCheckComponents(path)
	links := 0
	for len(pending) > 0 {
		part := pending[0]
		pending = pending[1:]
		switch part {
		case ".":
		case "..":
			if current != root {
				current = filepath.Dir(current)
			}
		default:
			next := filepath.Join(current, part)
			info, err := os.Lstat(next)
			if err != nil {
				return "", false, nil
			}
			if info.Mode()&os.ModeSymlink != 0 {
				links++
				if links > microvmMaxLinks {
					return "", false, errors.New("too many levels of symbolic links")
				}
				target, err := os.Readlink(next)
				if err != nil {
					return "", false, fmt.Errorf("read link %s: %w", next, microvmIOError(err))
				}
				if strings.HasPrefix(target, "/") {
					current = root
				}
				pending = append(microvmRootCheckComponents(target), pending...)
			} else {
				if len(pending) != 0 && !info.IsDir() {
					return "", false, fmt.Errorf("%s is not a directory", next)
				}
				current = next
			}
		}
	}
	return current, true, nil
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
	walked := ""
	for _, part := range microvmRootCheckComponents(path) {
		walked += "/" + part
		found, ok, err := microvmRootCheckResolve(root, walked)
		if err != nil {
			return err
		}
		if ok {
			if !microvmIsDir(found) {
				return fmt.Errorf("%s is not a directory", walked)
			}
			continue
		}
		// Absent, or a link to nothing: `mkdir -p` creates the first and
		// fails on the second, where the link occupies the name.
		parent := walked[:strings.LastIndex(walked, "/")]
		if parent == "" {
			parent = "/"
		}
		parentPath, ok, err := microvmRootCheckResolve(root, parent)
		if err != nil {
			return err
		}
		if !ok {
			return errors.New("the parent directory disappeared")
		}
		if _, err := os.Lstat(filepath.Join(parentPath, part)); err == nil {
			return fmt.Errorf("%s is a link to nothing", walked)
		}
		return nil
	}
	return nil
}
