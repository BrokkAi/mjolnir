// Port of mj-core/src/targets/storage.rs.
//
// Recognising a full disk on a target, and measuring a target's free space.
//
// Mjolnir writes to targets through `scp`, `ssh`, container engines and its
// own worker. Each says "the disk is full" in its own words, so the one
// classifier lives here and every executor reports through it.
//
// Free space is kept per filesystem: a host can have a full `~/Projects`
// beside a healthy `/`, and a write is judged by the filesystem it lands on.
// Paths are target text (POSIX on every target Mjolnir writes to), so they
// are interpreted here, at that boundary, and nowhere else.

package core

import (
	"encoding/json"
	"slices"
	"strings"
	"sync"
)

// ReportsNoSpace reports whether text says that a write failed for lack of
// space.
//
// Covers `No space left on device` (scp, ssh, coreutils, podman, docker and
// Rust's `os error 28`) and `Disk quota exceeded` (EDQUOT): a quota stops
// writes the same way a full filesystem does.
func ReportsNoSpace(text string) bool {
	text = strings.ToLower(text)
	return strings.Contains(text, "no space left on device") ||
		strings.Contains(text, "enospc") ||
		strings.Contains(text, "os error 28)") ||
		strings.Contains(text, "disk quota exceeded") ||
		strings.Contains(text, "edquot")
}

// NoSpacePath is the target path a no-space failure names, when it names
// one: scp's `write remote "PATH"`, coreutils' `cp: error writing 'PATH'`, or
// a `PATH: No space left on device` prefix.
func NoSpacePath(text string) (string, bool) {
	panic("unported: mj-core/src/targets/storage.rs no_space_path")
}

// StorageHostOfDestination is the host a target command ran on, as the
// storage owner names it: the SSH destination without its user, or
// LocalStorageHost. An empty destination means none.
func StorageHostOfDestination(destination *string) string {
	if destination != nil {
		return SSHHostName(*destination)
	}
	return LocalStorageHost
}

// SSHHostName is the host part of an OpenSSH destination such as `build@host`.
func SSHHostName(destination string) string {
	if i := strings.LastIndexByte(destination, '@'); i >= 0 {
		return destination[i+1:]
	}
	return destination
}

// LocalStorageHost is the name the storage owner gives the machine the daemon
// runs on.
const LocalStorageHost = "local"

// RemoteWorkersDirectory is where worker roots live under an SSH or EC2
// user's home.
const RemoteWorkersDirectory = ".local/share/hel/workers"

// RemoteProfilesDirectory is where staged profile homes live under an SSH or
// EC2 user's home.
const RemoteProfilesDirectory = ".local/share/hel/profiles"

// RemoteCacheDirectory holds upload staging and binary caches under an SSH
// user's home.
const RemoteCacheDirectory = ".cache/mjolnir"

// DefaultBuildCacheDirectory is mbx's default cache under a user's home.
const DefaultBuildCacheDirectory = ".cache/mbx"

// TemporaryDirectory holds temporary files every target writes.
const TemporaryDirectory = "/tmp"

// ContainerStorage names container engines whose storage Mjolnir measures.
type ContainerStorage int

const (
	ContainerStoragePodman ContainerStorage = iota
	ContainerStorageDocker
	// ContainerStorageMicrovm is the `mj-worker microvm` engine's state:
	// image bases, per-VM root disks and volume disks, on the SSH host.
	ContainerStorageMicrovm
)

// Path is where the engine keeps images and container layers, which hold
// every path inside a container: rootless Podman under the user's home,
// Docker under its root directory.
func (s ContainerStorage) Path() string {
	switch s {
	case ContainerStoragePodman:
		return ".local/share/containers"
	case ContainerStorageDocker:
		return "/var/lib/docker"
	default:
		return "/var/lib/mj-microvm"
	}
}

// SessionStoragePaths are the filesystems one session writes to: its worker
// root, workspace (the project directory or managed clone), staged profile
// home and `/tmp`; a container session writes them all inside its engine's
// storage.
type SessionStoragePaths struct {
	Host       string
	WorkerRoot string
	Others     []string
}

// All lists the worker root followed by the other paths.
func (p *SessionStoragePaths) All() []string {
	return append([]string{p.WorkerRoot}, p.Others...)
}

// SessionStoragePathsOf is SessionStoragePaths for a recorded target
// (Rust: session_storage_paths). projectDirectory is the session's checkout
// on a local bare target; empty means none.
func SessionStoragePathsOf(locator TargetLocator, sessionID string, projectDirectory *string) SessionStoragePaths {
	panic("unported: mj-core/src/targets/storage.rs session_storage_paths")
}

// LocalHomePath is a home-relative path made absolute against this machine's
// home.
func LocalHomePath(relative string) string {
	panic("unported: mj-core/src/targets/storage.rs local_home_path")
}

// NormalizeTargetPath is a target path as the board compares it: absolute
// against home, without a trailing slash. `~/x` and `x` are home-relative, as
// for ssh and scp.
func NormalizeTargetPath(path string, home *string) string {
	panic("unported: mj-core/src/targets/storage.rs normalize_target_path")
}

// pathWithin reports whether path is base or lies under it, by whole
// components.
func pathWithin(path, base string) bool {
	if path == base || base == "/" {
		return true
	}
	rest, ok := strings.CutPrefix(path, base)
	return ok && strings.HasPrefix(rest, "/")
}

// NoSpaceObserver is told that a write failed on host because its disk is
// full.
type NoSpaceObserver func(host, detail string)

var (
	noSpaceObserverOnce sync.Once
	noSpaceObserver     NoSpaceObserver
)

// SetNoSpaceObserver installs the process's owner of target storage health.
// Only the daemon installs one; other processes classify nothing.
func SetNoSpaceObserver(observer NoSpaceObserver) {
	noSpaceObserverOnce.Do(func() { noSpaceObserver = observer })
}

// ReportIfNoSpace reports detail to the installed owner when it says the
// disk on host is full. Returns whether it did.
func ReportIfNoSpace(host, detail string) bool {
	if !ReportsNoSpace(detail) {
		return false
	}
	if observer := noSpaceObserver; observer != nil {
		observer(host, strings.TrimSpace(detail))
	}
	return true
}

// observeCommandOutput is called by every process executor once a command
// has finished.
func observeCommandOutput(command *CommandSpec, output *CommandOutput) {
	panic("unported: mj-core/src/targets/storage.rs observe_command_output")
}

// FilesystemSpace is one filesystem Mjolnir writes to on a target, as
// `df -Pk` reports it.
type FilesystemSpace struct {
	// Mount is where the filesystem is mounted on the target.
	Mount string `json:"mount"`
	// AvailableBytes is what an unprivileged writer can still use (`f_bavail`).
	AvailableBytes uint64 `json:"available_bytes"`
	TotalBytes     uint64 `json:"total_bytes"`
	// ReservedBytes the filesystem keeps back for root (ext4's reserve):
	// free, but not to Mjolnir's non-root writers.
	ReservedBytes uint64 `json:"reserved_bytes"`
	// Paths are the measured paths that live on this filesystem, normalized.
	// A write belongs to the filesystem of the longest of these that
	// contains it.
	Paths []string `json:"paths"`
}

// MarshalJSON writes Paths as [] when empty, as serde does.
func (s FilesystemSpace) MarshalJSON() ([]byte, error) {
	type plain FilesystemSpace
	if s.Paths == nil {
		s.Paths = []string{}
	}
	return json.Marshal(plain(s))
}

// HostStorageSample is what one probe found on one host: every filesystem
// Mjolnir writes to there.
type HostStorageSample struct {
	Host string `json:"host"`
	// Home is the probing user's home, against which relative paths are read.
	Home        *string           `json:"home"`
	Filesystems []FilesystemSpace `json:"filesystems"`
}

// MarshalJSON writes Filesystems as [] when empty, as serde does.
func (s HostStorageSample) MarshalJSON() ([]byte, error) {
	type plain HostStorageSample
	if s.Filesystems == nil {
		s.Filesystems = []FilesystemSpace{}
	}
	return json.Marshal(plain(s))
}

// WriteReserveBytes is what a write must leave free for the sessions already
// running on the filesystem: their journals, logs and harness writes are what
// fail first on a full disk. A flat amount rather than a percentage, because
// 2% of a 4 TB disk would refuse writes with 80 GB free.
const WriteReserveBytes uint64 = 1 << 30

// LowSpaceBytes is the level below which a filesystem is shown as low on
// space; nothing is refused.
const LowSpaceBytes uint64 = 5 << 30

// StorageCondition is the storage owner's verdict on one filesystem. The
// order is Ok < Low < Full.
type StorageCondition string

const (
	StorageConditionOk  StorageCondition = "ok"
	StorageConditionLow StorageCondition = "low"
	// StorageConditionFull is below WriteReserveBytes, or a write to it
	// failed for lack of space since the last measurement. Writes are
	// refused and recovery waits.
	StorageConditionFull StorageCondition = "full"
)

// rank orders conditions as the Rust derive(Ord) does.
func (c StorageCondition) rank() int {
	switch c {
	case StorageConditionOk:
		return 0
	case StorageConditionLow:
		return 1
	default:
		return 2
	}
}

// FilesystemView is one filesystem with the storage owner's verdict.
type FilesystemView struct {
	FilesystemSpace
	Condition StorageCondition `json:"condition"`
	// NoSpaceDetail is the failed write that marked this filesystem full,
	// until a later measurement replaces it.
	NoSpaceDetail *string `json:"no_space_detail,omitempty"`
}

// MarshalJSON flattens the space beside the verdict, as serde's flatten does.
// (Without it the embedded FilesystemSpace.MarshalJSON would be promoted.)
func (v FilesystemView) MarshalJSON() ([]byte, error) {
	paths := v.Paths
	if paths == nil {
		paths = []string{}
	}
	return json.Marshal(struct {
		Mount          string           `json:"mount"`
		AvailableBytes uint64           `json:"available_bytes"`
		TotalBytes     uint64           `json:"total_bytes"`
		ReservedBytes  uint64           `json:"reserved_bytes"`
		Paths          []string         `json:"paths"`
		Condition      StorageCondition `json:"condition"`
		NoSpaceDetail  *string          `json:"no_space_detail,omitempty"`
	}{v.Mount, v.AvailableBytes, v.TotalBytes, v.ReservedBytes, paths, v.Condition, v.NoSpaceDetail})
}

// UnmarshalJSON reads the flattened space and the verdict.
func (v *FilesystemView) UnmarshalJSON(data []byte) error {
	var space FilesystemSpace
	type plainSpace FilesystemSpace
	if err := json.Unmarshal(data, (*plainSpace)(&space)); err != nil {
		return err
	}
	var rest struct {
		Condition     StorageCondition `json:"condition"`
		NoSpaceDetail *string          `json:"no_space_detail"`
	}
	if err := json.Unmarshal(data, &rest); err != nil {
		return err
	}
	*v = FilesystemView{FilesystemSpace: space, Condition: rest.Condition, NoSpaceDetail: rest.NoSpaceDetail}
	return nil
}

// TargetStorageView is the storage owner's published answer for one host.
type TargetStorageView struct {
	Host                  string           `json:"host"`
	Home                  *string          `json:"home"`
	Filesystems           []FilesystemView `json:"filesystems"`
	SampledAtEpochSeconds *uint64          `json:"sampled_at_epoch_seconds"`
	// UnattributedNoSpace is a write that failed for lack of space without
	// naming where. Every filesystem on the host counts as full until the
	// measurement it triggered says which one is.
	UnattributedNoSpace *string `json:"unattributed_no_space,omitempty"`
}

// MarshalJSON writes Filesystems as [] when empty, as serde does.
func (v TargetStorageView) MarshalJSON() ([]byte, error) {
	type plain TargetStorageView
	if v.Filesystems == nil {
		v.Filesystems = []FilesystemView{}
	}
	return json.Marshal(plain(v))
}

// TargetStorageViewEvaluate judges each filesystem of one host from its
// latest measurement and any failure since (Rust: TargetStorageView::evaluate).
func TargetStorageViewEvaluate(
	host string,
	home *string,
	filesystems []FilesystemSpace,
	sampledAtEpochSeconds *uint64,
	noSpace func(space *FilesystemSpace) *string,
	unattributedNoSpace *string,
) TargetStorageView {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::evaluate")
}

// FilesystemFor is the filesystem path lands on: the one holding the longest
// measured path that contains it.
func (v *TargetStorageView) FilesystemFor(path string) *FilesystemView {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::filesystem_for")
}

// WorstFor is the worst filesystem among those paths land on.
func (v *TargetStorageView) WorstFor(paths []string) *FilesystemView {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::worst_for")
}

// Worst is the worst filesystem on the host, for a one-line summary.
func (v *TargetStorageView) Worst() *FilesystemView {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::worst")
}

// FullFor is the full filesystem one of paths lands on, or every
// filesystem's stand-in while a failure is still unattributed.
func (v *TargetStorageView) FullFor(paths []string) *FilesystemView {
	worst := v.WorstFor(paths)
	if worst != nil && worst.Condition == StorageConditionFull {
		return worst
	}
	return nil
}

// Explanation reads "precision-3260 has 0 B free on /home (the filesystem
// reserves 24.69 GB more for root)".
func (v *TargetStorageView) Explanation(filesystem *FilesystemView) string {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::explanation")
}

// FilesystemLines is one line per filesystem: mount, free space, root
// reserve, and a flag on the full or low ones.
func (v *TargetStorageView) FilesystemLines() []string {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::filesystem_lines")
}

// ShortStatus is a short summary of every filesystem, such as "/: 41.0 GB ·
// /home: 0 B full".
func (v *TargetStorageView) ShortStatus() string {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::short_status")
}

// ProblemFor is "disk full: precision-3260 has 0 B free on /home …" when one
// of paths lands on a full filesystem.
func (v *TargetStorageView) ProblemFor(paths []string) (string, bool) {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::problem_for")
}

// unattributedExplanation is "host ran out of disk space: …" for a failure no
// measurement has placed on a filesystem yet.
func (v *TargetStorageView) unattributedExplanation() (string, bool) {
	if v.UnattributedNoSpace == nil {
		return "", false
	}
	return v.Host + " ran out of disk space: " + *v.UnattributedNoSpace, true
}

// RefuseWrite is the refusal for a write of bytes (zero when the size is
// unknown) to path, or false when it fits beside WriteReserveBytes or the
// path's filesystem has not been measured.
func (v *TargetStorageView) RefuseWrite(path string, bytes uint64, what string) (string, bool) {
	panic("unported: mj-core/src/targets/storage.rs TargetStorageView::refuse_write")
}

// CapacityStorageHosts are the storage hosts a capacity row stands for: the
// host itself, or each instance a fleet's last reading measured.
func CapacityStorageHosts(target *DeploymentCapacityTarget, usage *DeploymentCapacityUsage) []string {
	if target.Kind == DeploymentCapacityKindHost {
		return []string{target.Host}
	}
	if usage == nil {
		return nil
	}
	hosts := make([]string, 0, len(usage.Storage))
	for _, sample := range usage.Storage {
		hosts = append(hosts, sample.Host)
	}
	return hosts
}

// ViewsFor are the views a row that stands for hosts shows.
func ViewsFor(views []TargetStorageView, hosts []string) []*TargetStorageView {
	var out []*TargetStorageView
	for i := range views {
		for _, host := range hosts {
			if views[i].Host == host {
				out = append(out, &views[i])
				break
			}
		}
	}
	return out
}

// StorageProbeScript is shell that prints the probing user's home, then
// measures each path given as an argument at its nearest existing ancestor:
// `storage=<avail-kib>\t<total-kib>\t<used-kib>\t<mount>\t<path>`. `df -Pk`
// is POSIX, so it reads the same on Linux and macOS; its "Available" column
// is what a non-root user can write. The ancestor walk ends whenever a step
// fails or makes no progress, so a host without `dirname` cannot spin it.
const StorageProbeScript = `
printf 'home=%s\n' "$HOME"
for hel_path in "$@"; do
    hel_probe=$hel_path
    while [ ! -e "$hel_probe" ]; do
        hel_parent=$(dirname -- "$hel_probe") || break
        [ "$hel_parent" != "$hel_probe" ] || break
        hel_probe=$hel_parent
    done
    df -Pk -- "$hel_probe" 2>/dev/null | awk -v hel_path="$hel_path" 'NR == 2 { mount = $6; for (i = 7; i <= NF; i++) mount = mount " " $i; printf "storage=%s\t%s\t%s\t%s\t%s\n", $4, $2, $3, mount, hel_path }'
done
`

// ParseStorageLines parses StorageProbeScript output into the probing user's
// home and one entry per filesystem, each listing the measured paths on it.
func ParseStorageLines(output []byte) (home *string, filesystems []FilesystemSpace) {
	panic("unported: mj-core/src/targets/storage.rs parse_storage_lines")
}

// placeMeasuredPath adds one measurement, a filesystem holding one path,
// keeping one entry per filesystem and the first reading of each.
func placeMeasuredPath(filesystems *[]FilesystemSpace, measured FilesystemSpace) {
	for i := range *filesystems {
		known := &(*filesystems)[i]
		if known.Mount != measured.Mount {
			continue
		}
		for _, path := range measured.Paths {
			if !slices.Contains(known.Paths, path) {
				known.Paths = append(known.Paths, path)
			}
		}
		return
	}
	*filesystems = append(*filesystems, measured)
}
