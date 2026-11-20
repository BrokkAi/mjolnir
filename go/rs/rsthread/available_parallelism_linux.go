// Port of std 1.99.1 src/sys/thread/unix.rs (available_parallelism and the
// cgroups module) for Linux

package rsthread

import (
	"bufio"
	"io"
	"math"
	"os"
	"runtime"
	"strings"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

func availableParallelism() (int, error) {
	quota := max(cgroupsQuota(), 1)
	var set unix.CPUSet
	if unix.SchedGetaffinity(0, &set) == nil {
		count := min(set.Count(), quota)
		// According to sched_getaffinity's API it should always be non-zero, but
		// some old MIPS kernels were buggy and zero-initialized the mask if
		// none was explicitly set.
		// In that case we use the sysconf fallback.
		if count != 0 {
			return count, nil
		}
	}
	// sysconf(_SC_NPROCESSORS_ONLN). Go reads the same count the C library
	// does at start-up; this is reached only when sched_getaffinity fails.
	cpus := runtime.NumCPU()
	// Cover the unusual situation where we were able to get the quota but not the affinity mask
	return min(cpus, quota), nil
}

// The cgroups module. Currently not covered:
//   - cgroup v2 in non-standard mountpoints
//   - paths containing control characters or spaces, since those would be
//     escaped in procfs output and we don't unescape

type cgroupVersion uint8

const (
	cgroupV1 cgroupVersion = iota
	cgroupV2
)

// cgroupsQuota is cgroups::quota: the cgroup CPU quota in core-equivalents,
// rounded down, or the largest int if the quota cannot be determined or is
// not set.
func cgroupsQuota() int {
	quota := math.MaxInt

	// find our place in the cgroup hierarchy
	buf, err := os.ReadFile("/proc/self/cgroup")
	if err != nil {
		return quota
	}
	type found struct {
		path    string
		version cgroupVersion
	}
	var current *found
	for _, line := range strings.Split(string(buf), "\n") {
		fields := strings.SplitN(line, ":", 3)
		// 2nd field is a list of controllers for v1 or empty for v2
		if len(fields) < 2 {
			continue
		}
		var version cgroupVersion
		switch controllers := fields[1]; {
		case controllers == "":
			version = cgroupV2
		case utf8.ValidString(controllers) && hasField(controllers, ",", "cpu"):
			version = cgroupV1
		default:
			continue
		}

		// already-found v1 trumps v2 since it explicitly specifies its controllers
		if current != nil && version == cgroupV2 {
			continue
		}

		if len(fields) < 3 {
			current = nil
			continue
		}
		path := fields[2]
		if len(path) == 0 {
			// Rust slices `path[1..]` here, which panics on an empty path.
			panic("range start index 1 out of range for slice of length 0")
		}
		// skip leading slash
		current = &found{path: path[1:], version: version}
	}
	if current == nil {
		return quota
	}

	switch current.version {
	case cgroupV1:
		return quotaV1(current.path)
	default:
		return quotaV2(current.path)
	}
}

// hasField reports whether one of the parts of text split at sep is field.
func hasField(text, sep, field string) bool {
	for _, part := range strings.Split(text, sep) {
		if part == field {
			return true
		}
	}
	return false
}

func quotaV2(groupPath string) int {
	quota := math.MaxInt

	// standard mount location defined in file-hierarchy(7) manpage
	const cgroupMount = "/sys/fs/cgroup"

	path := pathPush(cgroupMount, groupPath)
	path = pathPush(path, "cgroup.controllers")

	// skip if we're not looking at cgroup2
	if !pathExists(path) {
		return math.MaxInt
	}

	path = pathPop(path)

	for pathStartsWith(path, cgroupMount) {
		path = pathPush(path, "cpu.max")
		if text, ok := readToString(path); ok {
			rawQuota, ok := firstLine(text)
			if !ok {
				return quota
			}
			parts := strings.Split(rawQuota, " ")
			if len(parts) < 2 {
				return quota
			}
			limit, limitOk := parseUsize(parts[0])
			period, periodOk := parseUsize(parts[1])
			if limitOk && periodOk && period > 0 {
				quota = min(quota, limit/period)
			}
		}
		path = pathPop(path) // pop filename
		path = pathPop(path) // pop dir
	}
	return quota
}

func quotaV1(groupPath string) int {
	quota := math.MaxInt

	// Hardcode commonly used locations mentioned in the cgroups(7) manpage
	// if that doesn't work scan mountinfo and adjust `group_path` for bind-mounts
	mounts := []func(string) (string, string, bool){
		func(p string) (string, string, bool) { return "/sys/fs/cgroup/cpu", p, true },
		func(p string) (string, string, bool) { return "/sys/fs/cgroup/cpu,cpuacct", p, true },
		// this can be expensive on systems with tons of mountpoints
		// but we only get to this point when /proc/self/cgroups explicitly indicated
		// this process belongs to a cpu-controller cgroup v1 and the defaults didn't work
		findMountpoint,
	}

	for _, mount := range mounts {
		mountPoint, group, ok := mount(groupPath)
		if !ok {
			continue
		}
		path := pathPush(mountPoint, group)

		// skip if we guessed the mount incorrectly
		if !pathExists(path) {
			continue
		}

		for pathStartsWith(path, mountPoint) {
			parseFile := func(name string) (int, bool) {
				text, ok := readToString(pathPush(path, name))
				if !ok {
					return 0, false
				}
				return parseUsize(strings.TrimSpace(text))
			}
			limit, limitOk := parseFile("cpu.cfs_quota_us")
			period, periodOk := parseFile("cpu.cfs_period_us")
			if limitOk && periodOk && period > 0 {
				quota = min(quota, limit/period)
			}
			path = pathPop(path)
		}

		// we passed the try_exists above so we should have traversed the correct hierarchy
		// when reaching this line
		break
	}
	return quota
}

// findMountpoint scans mountinfo for cgroup v1 mountpoint with a cpu
// controller.
//
// If the cgroupfs is a bind mount then `group_path` is adjusted to skip over
// the already-included prefix
func findMountpoint(groupPath string) (string, string, bool) {
	file, err := os.Open("/proc/self/mountinfo")
	if err != nil {
		return "", "", false
	}
	defer file.Close()
	reader := bufio.NewReader(file)
	for {
		// read_line: an error, or text that is not UTF-8, ends the scan, as
		// does the end of the file.
		line, err := reader.ReadString('\n')
		if (err != nil && err != io.EOF) || !utf8.ValidString(line) || line == "" {
			return "", "", false
		}
		items := strings.Split(strings.TrimSpace(line), " ")
		// sub_path is the 4th item and mount_point the 5th; mount_opts is
		// the last and filesystem_type the one two before it. Rust takes
		// them from both ends of one iterator, which needs eight items; with
		// fewer the scan ends.
		if len(items) < 8 {
			return "", "", false
		}
		subPath := items[3]
		mountPoint := items[4]
		mountOpts := items[len(items)-1]
		filesystemType := items[len(items)-3]

		if filesystemType != "cgroup" || !hasField(mountOpts, ",", "cpu") {
			// not a cgroup / not a cpu-controller
			continue
		}

		if !strings.HasPrefix(subPath, "/") {
			return "", "", false
		}
		subPath = strings.TrimLeft(subPath, "/")

		if !pathStartsWith(groupPath, subPath) {
			// this is a bind-mount and the bound subdirectory
			// does not contain the cgroup this process belongs to
			continue
		}

		return mountPoint, pathStripPrefix(groupPath, subPath), true
	}
}

// readToString is fs::read_to_string: the file's text, which must be UTF-8.
func readToString(path string) (string, bool) {
	data, err := os.ReadFile(path)
	if err != nil || !utf8.Valid(data) {
		return "", false
	}
	return string(data), true
}

// firstLine is `text.lines().next()`.
func firstLine(text string) (string, bool) {
	if text == "" {
		return "", false
	}
	line, _, _ := strings.Cut(text, "\n")
	return strings.TrimSuffix(line, "\r"), true
}

// parseUsize is `text.parse::<usize>()`: decimal digits after an optional
// '+', fitting the port's usize.
func parseUsize(text string) (int, bool) {
	text = strings.TrimPrefix(text, "+")
	if text == "" {
		return 0, false
	}
	value := 0
	for i := 0; i < len(text); i++ {
		c := text[i]
		if c < '0' || c > '9' {
			return 0, false
		}
		digit := int(c - '0')
		if value > (math.MaxInt-digit)/10 {
			return 0, false
		}
		value = value*10 + digit
	}
	return value, true
}

// pathExists is fs::exists returning Ok(true).
func pathExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// pathComponents splits a Unix path into its normal components, as
// Path::components does: separators collapse and "." is skipped.
func pathComponents(path string) []string {
	var components []string
	for _, part := range strings.Split(path, "/") {
		if part != "" && part != "." {
			components = append(components, part)
		}
	}
	return components
}

// pathPush is PathBuf::push for Unix paths.
func pathPush(base, path string) string {
	if strings.HasPrefix(path, "/") {
		return path
	}
	if base != "" && !strings.HasSuffix(base, "/") {
		base += "/"
	}
	return base + path
}

// pathPop is PathBuf::pop for the absolute paths built here: the path
// without its last component, or the path itself when it has none.
func pathPop(path string) string {
	components := pathComponents(path)
	if len(components) == 0 {
		return path
	}
	return "/" + strings.Join(components[:len(components)-1], "/")
}

// pathStartsWith is Path::starts_with: whether base's components begin
// path's.
func pathStartsWith(path, base string) bool {
	if strings.HasPrefix(base, "/") != strings.HasPrefix(path, "/") {
		return false
	}
	pathParts, baseParts := pathComponents(path), pathComponents(base)
	if len(baseParts) > len(pathParts) {
		return false
	}
	for i, part := range baseParts {
		if pathParts[i] != part {
			return false
		}
	}
	return true
}

// pathStripPrefix is Path::strip_prefix for a prefix pathStartsWith
// accepted.
func pathStripPrefix(path, base string) string {
	return strings.Join(pathComponents(path)[len(pathComponents(base)):], "/")
}
