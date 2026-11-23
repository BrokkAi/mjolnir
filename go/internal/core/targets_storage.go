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
	"cmp"
	"encoding/json"
	"fmt"
	"log/slog"
	"math"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
)

// ReportsNoSpace reports whether text says that a write failed for lack of
// space.
//
// Covers `No space left on device` (scp, ssh, coreutils, podman, docker and
// Rust's `os error 28`) and `Disk quota exceeded` (EDQUOT): a quota stops
// writes the same way a full filesystem does.
//
// The errno names count only as words of their own: `/work/enospc-repro` in
// an unrelated error is a path, not a full disk.
func ReportsNoSpace(text string) bool {
	lower := asciiLowercase(text)
	if strings.Contains(lower, "no space left on device") ||
		strings.Contains(lower, "os error 28)") ||
		strings.Contains(lower, "disk quota exceeded") {
		return true
	}
	words := strings.FieldsFunc(lower, func(character rune) bool {
		return !(character < 0x80 && (character >= 'a' && character <= 'z' || character >= 'A' && character <= 'Z' ||
			character >= '0' && character <= '9' || strings.ContainsRune("_-./", character)))
	})
	for _, word := range words {
		if word == "enospc" || word == "edquot" {
			return true
		}
	}
	return false
}

// NoSpacePath is the target path a no-space failure names, when it names
// one: scp's `write remote "PATH"`, coreutils' `cp: error writing 'PATH'`, or
// a `PATH: No space left on device` prefix.
func NoSpacePath(text string) (string, bool) {
	var line string
	found := false
	for _, candidate := range targetsLines(text) {
		if ReportsNoSpace(candidate) {
			line, found = candidate, true
			break
		}
	}
	if !found {
		return "", false
	}
	quoted := func(open string, close byte) (string, bool) {
		start := strings.Index(line, open)
		if start < 0 {
			return "", false
		}
		start += len(open)
		end := strings.IndexByte(line[start:], close)
		if end < 0 {
			return "", false
		}
		return line[start : start+end], true
	}
	if path, ok := quoted("write remote \"", '"'); ok {
		return path, true
	}
	if path, ok := quoted("error writing '", '\''); ok {
		return path, true
	}
	if path, ok := quoted("error writing \"", '"'); ok {
		return path, true
	}
	// `cat: /path: No space left on device`, `write /path: no space left`.
	for _, word := range strings.FieldsFunc(line, func(r rune) bool { return r == ' ' || r == ':' }) {
		if strings.HasPrefix(word, "/") && len(word) > 1 {
			return word, true
		}
	}
	return "", false
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
	remoteProfile := RemoteProfilesDirectory + "/" + sessionID
	paths := SessionStoragePaths{Host: LocalStorageHost, Others: []string{}}
	switch locator := locator.(type) {
	case TargetLocatorLocalBare:
		paths.WorkerRoot = locator.WorkerRoot
		if projectDirectory != nil {
			paths.Others = append(paths.Others, *projectDirectory)
		}
		paths.Others = append(paths.Others, TemporaryDirectory)
	case TargetLocatorLocalPodman:
		paths.WorkerRoot = LocalHomePath(ContainerStoragePodman.Path())
	case TargetLocatorLocalDocker:
		paths.WorkerRoot = ContainerStorageDocker.Path()
	case TargetLocatorAppleContainer:
		// Apple's container runtime keeps its storage in a VM disk image.
	case TargetLocatorAwsEc2:
		paths.Host = locator.InstanceID
		if locator.Address != nil {
			paths.Host = *locator.Address
		}
		paths.WorkerRoot = RemoteWorkersDirectory + "/" + sessionID
		paths.Others = []string{".local/share/hel/workspaces/" + sessionID, remoteProfile, TemporaryDirectory}
	case TargetLocatorSshBare:
		paths.Host = SSHHostName(locator.Host)
		worker := sessionID
		if locator.WorkerID != nil {
			worker = *locator.WorkerID
		}
		paths.WorkerRoot = RemoteWorkersDirectory + "/" + worker
		paths.Others = []string{locator.Workspace, remoteProfile, TemporaryDirectory}
	case TargetLocatorSshPodman:
		paths.Host = SSHHostName(locator.Host)
		paths.WorkerRoot = ContainerStoragePodman.Path()
	case TargetLocatorSshDocker:
		paths.Host = SSHHostName(locator.Host)
		paths.WorkerRoot = ContainerStorageDocker.Path()
	case TargetLocatorSshAppleContainer:
		// Apple's container runtime keeps its storage in a VM disk image on
		// the remote host, so there is no directory to board (as locally).
		paths.Host = SSHHostName(locator.Host)
	case TargetLocatorSshNitro:
		// A Nitro sandbox's disks are sparse files the engine keeps on the
		// host.
		paths.Host = SSHHostName(locator.Host)
		paths.WorkerRoot = ContainerStorageMicrovm.Path()
	}
	return paths
}

// LocalHomePath is a home-relative path made absolute against this machine's
// home.
func LocalHomePath(relative string) string {
	home, ok := UserHome()
	if !ok {
		return relative
	}
	if filepath.IsAbs(relative) {
		return relative
	}
	return filepath.Join(home, relative)
}

// NormalizeTargetPath is a target path as the board compares it: absolute
// against home, without a trailing slash. `~/x` and `x` are home-relative, as
// for ssh and scp.
func NormalizeTargetPath(path string, home *string) string {
	path = strings.TrimSpace(path)
	var joined string
	if strings.HasPrefix(path, "/") {
		joined = path
	} else {
		relative := strings.TrimPrefix(path, "~/")
		relative = strings.TrimPrefix(relative, "./")
		switch {
		case home != nil && (relative == "" || relative == "~" || relative == "."):
			joined = *home
		case home != nil:
			joined = strings.TrimRight(*home, "/") + "/" + relative
		default:
			joined = relative
		}
	}
	if len(joined) > 1 {
		return strings.TrimRight(joined, "/")
	}
	return joined
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
	if output.Status == 0 {
		return
	}
	stderr := targetsLossyText(output.Stderr)
	host := StorageHostOfDestination(command.SSHDestination)
	if ReportIfNoSpace(host, stderr) {
		slog.Warn("target command failed because the disk is full: "+strings.TrimSpace(stderr),
			"host", host, "purpose", command.Purpose)
	}
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
	// QuotaLimited: AvailableBytes is the probing user's or group's disk
	// quota, which is smaller than the filesystem's free space.
	QuotaLimited bool `json:"quota_limited,omitempty"`
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

// Compare is the derived Ord::cmp: Ok < Low < Full.
func (c StorageCondition) Compare(other StorageCondition) int {
	return cmp.Compare(c.rank(), other.rank())
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
	views := make([]FilesystemView, 0, len(filesystems))
	for i := range filesystems {
		space := &filesystems[i]
		noSpaceDetail := noSpace(space)
		condition := StorageConditionOk
		switch {
		case noSpaceDetail != nil || unattributedNoSpace != nil || space.AvailableBytes < WriteReserveBytes:
			condition = StorageConditionFull
		case space.AvailableBytes < LowSpaceBytes:
			condition = StorageConditionLow
		}
		copied := *space
		copied.Paths = append([]string(nil), space.Paths...)
		views = append(views, FilesystemView{FilesystemSpace: copied, Condition: condition, NoSpaceDetail: noSpaceDetail})
	}
	return TargetStorageView{
		Host:                  host,
		Home:                  home,
		Filesystems:           views,
		SampledAtEpochSeconds: sampledAtEpochSeconds,
		UnattributedNoSpace:   unattributedNoSpace,
	}
}

// FilesystemFor is the filesystem path lands on: the one holding the longest
// measured path that contains it.
func (v *TargetStorageView) FilesystemFor(path string) *FilesystemView {
	path = NormalizeTargetPath(path, v.Home)
	var best *FilesystemView
	bestLength := -1
	for i := range v.Filesystems {
		filesystem := &v.Filesystems[i]
		for _, base := range filesystem.Paths {
			// Rust's max_by_key keeps the last of equal maxima.
			if pathWithin(path, base) && len(base) >= bestLength {
				best, bestLength = filesystem, len(base)
			}
		}
	}
	return best
}

// WorstFor is the worst filesystem among those paths land on.
func (v *TargetStorageView) WorstFor(paths []string) *FilesystemView {
	var worst *FilesystemView
	for _, path := range paths {
		if path == "" {
			continue
		}
		filesystem := v.FilesystemFor(path)
		if filesystem == nil {
			continue
		}
		// Rust's max_by_key keeps the last of equal maxima.
		if worst == nil || filesystem.Condition.rank() >= worst.Condition.rank() {
			worst = filesystem
		}
	}
	return worst
}

// Worst is the worst filesystem on the host, for a one-line summary.
func (v *TargetStorageView) Worst() *FilesystemView {
	var worst *FilesystemView
	for i := range v.Filesystems {
		filesystem := &v.Filesystems[i]
		if worst == nil {
			worst = filesystem
			continue
		}
		order := cmp.Compare(filesystem.Condition.rank(), worst.Condition.rank())
		if order == 0 {
			order = cmp.Compare(worst.AvailableBytes, filesystem.AvailableBytes)
		}
		// Rust's max_by keeps the last of equal maxima.
		if order >= 0 {
			worst = filesystem
		}
	}
	return worst
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
	text := fmt.Sprintf("%s has %s free on %s", v.Host, FormatBytes(filesystem.AvailableBytes), filesystem.Mount)
	if filesystem.QuotaLimited {
		text += " within its disk quota"
	}
	if filesystem.ReservedBytes >= WriteReserveBytes {
		text += fmt.Sprintf(" (the filesystem reserves %s more for root)", FormatBytes(filesystem.ReservedBytes))
	}
	if filesystem.Condition == StorageConditionFull && filesystem.AvailableBytes >= WriteReserveBytes {
		detail := filesystem.NoSpaceDetail
		if detail == nil {
			detail = v.UnattributedNoSpace
		}
		if detail != nil {
			text += "; a write failed: " + *detail
		}
	}
	return text
}

// FilesystemLines is one line per filesystem: mount, free space, root
// reserve, and a flag on the full or low ones.
func (v *TargetStorageView) FilesystemLines() []string {
	lines := make([]string, 0, len(v.Filesystems))
	for _, filesystem := range v.Filesystems {
		line := fmt.Sprintf("%s: %s free", filesystem.Mount, FormatBytes(filesystem.AvailableBytes))
		if filesystem.ReservedBytes >= WriteReserveBytes {
			line += fmt.Sprintf(", %s reserved for root", FormatBytes(filesystem.ReservedBytes))
		}
		switch filesystem.Condition {
		case StorageConditionFull:
			line += " (full)"
		case StorageConditionLow:
			line += " (low)"
		}
		lines = append(lines, line)
	}
	return lines
}

// ShortStatus is a short summary of every filesystem, such as "/: 41.0 GB ·
// /home: 0 B full".
func (v *TargetStorageView) ShortStatus() string {
	if len(v.Filesystems) == 0 {
		return "free space unknown"
	}
	parts := make([]string, 0, len(v.Filesystems))
	for _, filesystem := range v.Filesystems {
		free := FormatBytes(filesystem.AvailableBytes)
		switch filesystem.Condition {
		case StorageConditionFull:
			parts = append(parts, fmt.Sprintf("%s %s full", filesystem.Mount, free))
		case StorageConditionLow:
			parts = append(parts, fmt.Sprintf("%s %s low", filesystem.Mount, free))
		default:
			parts = append(parts, fmt.Sprintf("%s %s", filesystem.Mount, free))
		}
	}
	return strings.Join(parts, " · ")
}

// ProblemFor is "disk full: precision-3260 has 0 B free on /home …" when one
// of paths lands on a full filesystem.
func (v *TargetStorageView) ProblemFor(paths []string) (string, bool) {
	if filesystem := v.FullFor(paths); filesystem != nil {
		return "disk full: " + v.Explanation(filesystem), true
	}
	// A failure no measurement has placed yet holds every path.
	explanation, ok := v.unattributedExplanation()
	if !ok || !slices.ContainsFunc(paths, func(path string) bool { return path != "" }) {
		return "", false
	}
	return "disk full: " + explanation, true
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
	var explanation string
	if filesystem := v.FilesystemFor(path); filesystem != nil {
		needed := bytes + WriteReserveBytes
		if needed < bytes {
			needed = math.MaxUint64
		}
		if filesystem.Condition != StorageConditionFull && filesystem.AvailableBytes >= needed {
			return "", false
		}
		explanation = v.Explanation(filesystem)
	} else {
		unattributed, ok := v.unattributedExplanation()
		if !ok {
			return "", false
		}
		explanation = unattributed
	}
	size := ""
	if bytes > 0 {
		size = fmt.Sprintf(" (%s)", FormatBytes(bytes))
	}
	return fmt.Sprintf("Cannot %s%s: %s. Mjolnir keeps %s free for running sessions; free space on %s to continue.",
		what, size, explanation, FormatBytes(WriteReserveBytes), v.Host), true
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

// StorageProbeScript is shell that prints the probing user's home and clock,
// then measures each path given as an argument at its nearest existing
// ancestor: `storage=<avail-kib>\t<total-kib>\t<used-kib>\t<mount>\t<path>`.
// `df -Pk` is POSIX, so it reads the same on Linux and macOS; its "Available"
// column is what a non-root user can write. The ancestor walk ends whenever
// a step fails or makes no progress, so a host without `dirname` cannot spin
// it.
//
// `df` does not see a user or group quota (statfs reports XFS and ext4
// project quotas already), so where Linux quota-tools are installed each
// filesystem's quota rows follow as `quota=<mount>\t<the nine columns>`.
// Elsewhere `quota` is absent or rejects these options, and prints nothing.
const StorageProbeScript = `
printf 'home=%s\n' "$HOME"
printf 'now=%s\n' "$(date +%s)"
for hel_path in "$@"; do
    hel_probe=$hel_path
    while [ ! -e "$hel_probe" ]; do
        hel_parent=$(dirname -- "$hel_probe") || break
        [ "$hel_parent" != "$hel_probe" ] || break
        hel_probe=$hel_parent
    done
    hel_row=$(df -Pk -- "$hel_probe" 2>/dev/null | awk -v hel_path="$hel_path" 'NR == 2 { mount = $6; for (i = 7; i <= NF; i++) mount = mount " " $i; printf "storage=%s\t%s\t%s\t%s\t%s\n", $4, $2, $3, mount, hel_path }')
    [ -n "$hel_row" ] || continue
    printf '%s\n' "$hel_row"
    command -v quota >/dev/null 2>&1 || continue
    hel_mount=$(printf '%s\n' "$hel_row" | cut -f4)
    LC_ALL=C quota -ug -w -p -v --show-mntpoint --hide-device -f "$hel_mount" 2>/dev/null | awk -v hel_mount="$hel_mount" '$1 == hel_mount && NF == 9 { printf "quota=%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", $1, $2, $3, $4, $5, $6, $7, $8, $9 }'
done
`

// ParseStorageLines parses StorageProbeScript output into the probing user's
// home and one entry per filesystem, each listing the measured paths on it.
func ParseStorageLines(output []byte) (home *string, filesystems []FilesystemSpace) {
	text := targetsLossyText(output)
	lines := targetsLines(text)
	for _, line := range lines {
		if value, ok := strings.CutPrefix(line, "home="); ok {
			value = strings.TrimSpace(value)
			if strings.HasPrefix(value, "/") {
				home = &value
			}
			break
		}
	}
	saturatingMul := func(value uint64) uint64 {
		if value > math.MaxUint64/1024 {
			return math.MaxUint64
		}
		return value * 1024
	}
	saturatingSub := func(left, right uint64) uint64 {
		if right > left {
			return 0
		}
		return left - right
	}
	for _, line := range lines {
		row, ok := strings.CutPrefix(line, "storage=")
		if !ok {
			continue
		}
		fields := strings.Split(row, "\t")
		if len(fields) != 5 {
			continue
		}
		available, err1 := strconv.ParseUint(fields[0], 10, 64)
		total, err2 := strconv.ParseUint(fields[1], 10, 64)
		used, err3 := strconv.ParseUint(fields[2], 10, 64)
		if err1 != nil || err2 != nil || err3 != nil {
			continue
		}
		placeMeasuredPath(&filesystems, FilesystemSpace{
			Mount:          fields[3],
			AvailableBytes: saturatingMul(available),
			TotalBytes:     saturatingMul(total),
			ReservedBytes:  saturatingMul(saturatingSub(saturatingSub(total, used), available)),
			Paths:          []string{NormalizeTargetPath(fields[4], home)},
		})
	}
	var now *uint64
	for _, line := range lines {
		if value, ok := strings.CutPrefix(line, "now="); ok {
			if parsed, ok := parseRustUnsigned(strings.TrimSpace(value)); ok {
				now = &parsed
			}
			break
		}
	}
	for _, line := range lines {
		row, ok := strings.CutPrefix(line, "quota=")
		if !ok {
			continue
		}
		mount, room, ok := quotaRoomBytes(row, now)
		if !ok {
			continue
		}
		for index := range filesystems {
			if filesystems[index].Mount == mount && room < filesystems[index].AvailableBytes {
				filesystems[index].AvailableBytes = room
				filesystems[index].QuotaLimited = true
			}
		}
	}
	if filesystems == nil {
		filesystems = []FilesystemSpace{}
	}
	return home, filesystems
}

// quotaRoomBytes is what one `quota -w -p -v` row (mount, then blocks,
// quota, limit and grace for space and for files; KiB, grace as epoch
// seconds, 0 when none) still lets its user or group write, or false when it
// sets no limit. A soft limit whose grace has run out stops writes like a
// hard one; without the probe's clock an expired grace cannot be told apart,
// so the hard limit decides.
func quotaRoomBytes(row string, now *uint64) (string, uint64, bool) {
	fields := strings.Split(row, "\t")
	mount, rest := fields[0], fields[1:]
	if len(rest) != 8 {
		return "", 0, false
	}
	var numbers [8]uint64
	for index, field := range rest {
		// `quota` marks a usage over its soft limit with `*`.
		value, ok := parseRustUnsigned(strings.TrimRight(field, "*"))
		if !ok {
			return "", 0, false
		}
		numbers[index] = value
	}
	blocks, soft, hard, grace := numbers[0], numbers[1], numbers[2], numbers[3]
	files, fileSoft, fileHard, fileGrace := numbers[4], numbers[5], numbers[6], numbers[7]
	expired := func(grace uint64) bool { return grace != 0 && now != nil && grace <= *now }
	filesStopped := (fileHard != 0 && files >= fileHard) || (fileSoft != 0 && files > fileSoft && expired(fileGrace))
	blocksStopped := soft != 0 && blocks > soft && expired(grace)
	var roomKiB uint64
	switch {
	case filesStopped || blocksStopped:
		roomKiB = 0
	case hard != 0:
		if hard > blocks {
			roomKiB = hard - blocks
		}
	default:
		return "", 0, false
	}
	if roomKiB > math.MaxUint64/1024 {
		return mount, math.MaxUint64, true
	}
	return mount, roomKiB * 1024, true
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
