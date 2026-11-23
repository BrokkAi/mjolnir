//go:build linux

// Port of mj-worker/src/microvm/boot.rs.
//
// The kernel and initrd every microVM boots.
//
// The kernel is the host's own EFI-stub `vmlinuz`. The initrd is built with
// dracut for each `(kernel version, engine build)` pair: it carries the
// guest `/init` script and a copy of this binary, which `/init` starts as
// the guest agent, so the agent always matches the engine that launched the
// VM.

package worker

import (
	"debug/elf"
	_ "embed"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// microvmInitScript is the guest's `/init` (Rust's include_str! of
// guest-init.sh, copied unchanged to data/microvm/).
//
//go:embed data/microvm/guest-init.sh
var microvmInitScript string

// microvmGuestDrivers are the kernel modules the initrd carries: the boot,
// disk, vsock and network set first, then what nested containers and
// nftables use inside the guest.
const microvmGuestDrivers = "virtio_pci virtio_blk virtio_net ext4 vmw_vsock_virtio_transport nsm overlay fuse tun veth bridge br_netfilter nf_tables nft_ct nft_nat nft_masq nft_chain_nat nft_compat nf_conntrack nf_nat nft_reject_inet nft_reject nf_reject_ipv4 xt_MASQUERADE xt_conntrack xt_addrtype xt_comment xt_mark xt_nat iptable_nat"

// microvmInitPrograms are the programs `/init` runs.
const microvmInitPrograms = "mount umount modprobe ip switch_root mkdir hostname cat cut sleep cp ls seq grep head tee echo ln chmod chown stat rm"

// microvmBootCurrentTag is the assets directory name for this kernel and
// engine build.
func microvmBootCurrentTag() (string, error) {
	release, err := MicrovmKernelRelease()
	if err != nil {
		return "", err
	}
	return release + "-" + core.WorkerBuildID(), nil
}

// microvmReadDirNames is the names in directory in the order the directory
// lists them, as Rust's fs::read_dir yields them.
func microvmReadDirNames(directory string) ([]string, error) {
	file, err := os.Open(directory)
	if err != nil {
		return nil, err
	}
	defer file.Close()
	return file.Readdirnames(-1)
}

// MicrovmBootPrune removes boot assets built for other kernels or engine
// builds, and scratch from interrupted builds. Running VMs have already
// loaded theirs; a later `start` rebuilds what it needs. The caller holds
// the global lock, which is also what builders hold while they write
// scratch.
func MicrovmBootPrune(layout MicrovmLayout) ([]string, error) {
	current, err := microvmBootCurrentTag()
	if err != nil {
		return nil, err
	}
	var removed []string
	names, err := microvmReadDirNames(layout.Boot())
	if os.IsNotExist(err) {
		return removed, nil
	}
	if err != nil {
		return nil, fmt.Errorf("list %s: %w", layout.Boot(), microvmIOError(err))
	}
	for _, name := range names {
		if name == current {
			continue
		}
		path := filepath.Join(layout.Boot(), name)
		if strings.HasPrefix(name, ".tmp-") {
			// Builders write scratch under the global lock, which the caller holds.
			if err := os.RemoveAll(path); err != nil {
				return nil, fmt.Errorf("remove boot scratch %s: %w", name, microvmIOError(err))
			}
			continue
		}
		// Another engine build may be launching from these assets.
		exclusive, err := layout.TryLockBootExclusive(name)
		if err != nil {
			return nil, err
		}
		if exclusive == nil {
			continue
		}
		err = os.RemoveAll(path)
		exclusive.Drop()
		if err != nil {
			return nil, fmt.Errorf("remove boot assets %s: %w", name, microvmIOError(err))
		}
		// The lock file stays: every reservation of this tag must keep
		// synchronizing on the same file.
		removed = append(removed, name)
	}
	return removed, nil
}

// MicrovmBootAssets is the kernel and initrd of one boot-assets directory.
type MicrovmBootAssets struct {
	Kernel string
	Initrd string
	// Tag is the directory name, `<kernel version>-<engine build>`.
	Tag string
	// Reservation keeps `image prune` from other engine builds away until
	// it is released (Rust drops it with the assets).
	Reservation *core.FileLock
}

// microvmStaticallyLinked reports whether the running binary has no ELF
// program interpreter: Rust's `cfg!(target_env = "musl")`, the build that
// can run as the guest agent of any image.
func microvmStaticallyLinked() bool {
	engine, err := os.Executable()
	if err != nil {
		return false
	}
	binary, err := elf.Open(engine)
	if err != nil {
		return false
	}
	defer binary.Close()
	for _, program := range binary.Progs {
		if program.Type == elf.PT_INTERP {
			return false
		}
	}
	return true
}

// MicrovmBootEnsure returns the assets for this kernel and engine build,
// built if absent. Only a statically linked worker can run as the guest
// agent, so any other build is refused.
func MicrovmBootEnsure(layout MicrovmLayout) (*MicrovmBootAssets, error) {
	if !microvmStaticallyLinked() {
		return nil, errors.New("this mj-worker is not a statically linked (musl) build, so it cannot run as the guest agent inside a VM image; use the portable worker")
	}
	kernelVersion, err := MicrovmKernelRelease()
	if err != nil {
		return nil, err
	}
	tag := kernelVersion + "-" + core.WorkerBuildID()
	directory := filepath.Join(layout.Boot(), tag)
	if err := layout.Ensure(); err != nil {
		return nil, err
	}
	reservation, err := layout.LockBootShared(tag)
	if err != nil {
		return nil, err
	}
	assets := &MicrovmBootAssets{
		Kernel:      filepath.Join(directory, "vmlinuz"),
		Initrd:      filepath.Join(directory, "initrd.img"),
		Tag:         tag,
		Reservation: reservation,
	}
	fail := func(err error) (*MicrovmBootAssets, error) {
		reservation.Drop()
		return nil, err
	}
	present := func() bool {
		return microvmIsFile(assets.Kernel) && microvmIsFile(assets.Initrd)
	}
	if present() {
		if err := microvmMakeReadable(assets); err != nil {
			return fail(err)
		}
		return assets, nil
	}
	if err := layout.Ensure(); err != nil {
		return fail(err)
	}
	// One builder at a time; the second finds the first's result.
	global, err := layout.LockGlobal()
	if err != nil {
		return fail(err)
	}
	defer global.Drop()
	if present() {
		if err := microvmMakeReadable(assets); err != nil {
			return fail(err)
		}
		return assets, nil
	}
	scratch := filepath.Join(layout.Boot(), fmt.Sprintf(".tmp-%s-%s", assets.Tag, MicrovmScratchSuffix()))
	if err := os.MkdirAll(scratch, 0o777); err != nil {
		return fail(fmt.Errorf("create %s: %w", scratch, microvmIOError(err)))
	}
	if err := microvmBootBuild(scratch, kernelVersion); err != nil {
		if cleanup := os.RemoveAll(scratch); cleanup != nil {
			slog.Warn("could not remove boot scratch directory", "cleanup", cleanup, "scratch", scratch)
		}
		return fail(fmt.Errorf("build the microvm kernel and initrd: %w", err))
	}
	if microvmExists(directory) {
		if err := os.RemoveAll(directory); err != nil {
			return fail(fmt.Errorf("replace %s: %w", directory, microvmIOError(err)))
		}
	}
	if err := os.Rename(scratch, directory); err != nil {
		return fail(fmt.Errorf("publish %s: %w", directory, microvmIOError(err)))
	}
	if err := microvmMakeReadable(assets); err != nil {
		return fail(err)
	}
	return assets, nil
}

// microvmIsFile is Rust's Path::is_file: the path, followed, is a regular
// file.
func microvmIsFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

// microvmIsDir is Rust's Path::is_dir.
func microvmIsDir(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.IsDir()
}

// microvmExists is Rust's Path::exists: the path, followed, exists.
func microvmExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// microvmMakeReadable: each VM's jail holds hard links to these two files
// and its unprivileged Firecracker reads them through the link, so the
// files must be readable by anyone. They hold nothing private: the host's
// kernel and this engine.
func microvmMakeReadable(assets *MicrovmBootAssets) error {
	for _, file := range []string{assets.Kernel, assets.Initrd} {
		info, err := os.Stat(file)
		if err != nil {
			return fmt.Errorf("stat %s: %w", file, microvmIOError(err))
		}
		if info.Mode().Perm() != 0o644 {
			if err := os.Chmod(file, 0o644); err != nil {
				return fmt.Errorf("make %s readable: %w", file, microvmIOError(err))
			}
		}
	}
	return nil
}

func microvmBootBuild(scratch string, kernelVersion string) error {
	bootKernel := "/boot/vmlinuz-" + kernelVersion
	if err := microvmCopyFile(bootKernel, filepath.Join(scratch, "vmlinuz")); err != nil {
		return fmt.Errorf("copy %s: %w", bootKernel, err)
	}
	init := filepath.Join(scratch, "init")
	if err := os.WriteFile(init, []byte(microvmInitScript), 0o666); err != nil {
		return fmt.Errorf("write the guest init script: %w", microvmIOError(err))
	}
	if err := os.Chmod(init, 0o755); err != nil {
		return microvmIOError(err)
	}
	engine, err := os.Executable()
	if err != nil {
		return fmt.Errorf("locate the engine binary: %w", err)
	}
	initrd := filepath.Join(scratch, "initrd.img")
	dracut, err := MicrovmFindTool("dracut")
	if err != nil {
		return err
	}
	if _, err := MicrovmRunChecked(dracut, []string{
		"--force",
		"--no-hostonly",
		"--reproducible",
		"--no-early-microcode",
		"--modules",
		"bash",
		"--add-drivers",
		microvmGuestDrivers,
		"--install",
		microvmInitPrograms,
		"--include",
		init,
		"/init",
		"--include",
		engine,
		"/mj-worker",
		"--kver",
		kernelVersion,
		initrd,
	}); err != nil {
		return err
	}
	if err := os.Remove(init); err != nil {
		return fmt.Errorf("remove the staged init script: %w", microvmIOError(err))
	}
	return nil
}

// microvmCopyFile is Rust's fs::copy: the contents and the permission bits
// of source into destination.
func microvmCopyFile(source string, destination string) error {
	input, err := os.Open(source)
	if err != nil {
		return microvmIOError(err)
	}
	defer input.Close()
	info, err := input.Stat()
	if err != nil {
		return microvmIOError(err)
	}
	output, err := os.OpenFile(destination, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, info.Mode().Perm())
	if err != nil {
		return microvmIOError(err)
	}
	if _, err := io.Copy(output, input); err != nil {
		_ = output.Close()
		return microvmIOError(err)
	}
	if err := output.Chmod(info.Mode().Perm()); err != nil {
		_ = output.Close()
		return microvmIOError(err)
	}
	return microvmIOError(output.Close())
}
