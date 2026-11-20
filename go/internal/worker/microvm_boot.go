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
	_ "embed"

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

// MicrovmBootPrune removes boot assets built for other kernels or engine
// builds, and scratch from interrupted builds. Running VMs have already
// loaded theirs; a later `start` rebuilds what it needs. The caller holds
// the global lock, which is also what builders hold while they write
// scratch.
func MicrovmBootPrune(layout MicrovmLayout) ([]string, error) {
	panic("unported: mj-worker/src/microvm/boot.rs prune")
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

// MicrovmBootEnsure returns the assets for this kernel and engine build,
// built if absent. Only a statically linked worker can run as the guest
// agent, so any other build is refused.
func MicrovmBootEnsure(layout MicrovmLayout) (*MicrovmBootAssets, error) {
	panic("unported: mj-worker/src/microvm/boot.rs ensure")
}

// microvmMakeReadable: each VM's jail holds hard links to these two files
// and its unprivileged Firecracker reads them through the link, so the
// files must be readable by anyone. They hold nothing private: the host's
// kernel and this engine.
func microvmMakeReadable(assets *MicrovmBootAssets) error {
	panic("unported: mj-worker/src/microvm/boot.rs make_readable")
}

func microvmBootBuild(scratch string, kernelVersion string) error {
	panic("unported: mj-worker/src/microvm/boot.rs build")
}
