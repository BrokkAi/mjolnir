// Port of mj-worker/src/work_facts/linux.rs.
//
// /proc reader for the process scan. Built on Linux only (the _linux file
// suffix), as Rust's #[cfg(target_os = "linux")].

package worker

import (
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"math"
	"os"
	"strconv"
	"strings"
	"syscall"
	"unicode/utf8"

	"golang.org/x/sys/unix"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// workFactsStat is Rust's linux::Stat (renamed: locals named stat would
// shadow it, and cpu_usage's ProcStat is procStat): the fields of
// /proc/<pid>/stat that follow the command name.
type workFactsStat struct {
	zombie     bool
	ppid       uint32
	startTicks uint64
}

func parseStat(text string) (workFactsStat, bool) {
	cut := strings.LastIndexByte(text, ')')
	if cut < 0 {
		return workFactsStat{}, false
	}
	fields := strings.Fields(text[cut+1:])
	// State is field 3 of the file and so index 0 here; ppid is field 4 and
	// the start time, in clock ticks since boot, is field 22.
	if len(fields) < 20 {
		return workFactsStat{}, false
	}
	ppid, err := strconv.ParseUint(fields[1], 10, 32)
	if err != nil {
		return workFactsStat{}, false
	}
	start, err := strconv.ParseUint(fields[19], 10, 64)
	if err != nil {
		return workFactsStat{}, false
	}
	return workFactsStat{zombie: strings.HasPrefix(fields[0], "Z"), ppid: uint32(ppid), startTicks: start}, true
}

// workFactsAtClockTick is AT_CLKTCK, the auxiliary vector entry the kernel passes
// with the clock tick frequency.
const workFactsAtClockTick = 17

// ticksPerSecond is sysconf(_SC_CLK_TCK). glibc answers it from the
// auxiliary vector's AT_CLKTCK, and with 100 when the kernel passed none;
// Go has no sysconf, so this reads the same entry.
func ticksPerSecond() (uint64, error) {
	ticks := int64(100)
	if vector, err := unix.Auxv(); err == nil {
		for _, entry := range vector {
			if entry[0] == workFactsAtClockTick && entry[1] != 0 {
				ticks = int64(entry[1])
			}
		}
	}
	if ticks <= 0 {
		return 0, errors.New("could not read the clock tick frequency")
	}
	return uint64(ticks), nil
}

// uptimeMs is uptime in milliseconds. It is monotonic, unlike the boot time
// in /proc/stat, which moves with wall-clock corrections.
func uptimeMs() (int64, error) {
	text, err := os.ReadFile("/proc/uptime")
	if err != nil {
		return 0, fmt.Errorf("read /proc/uptime: %w", err)
	}
	fields := strings.Fields(string(text))
	if len(fields) == 0 {
		return 0, errors.New("empty /proc/uptime")
	}
	seconds, err := strconv.ParseFloat(fields[0], 64)
	if err != nil {
		return 0, fmt.Errorf("parse /proc/uptime: %w", err)
	}
	return int64(seconds * 1000.0), nil
}

// sessionMarker is whether pid's environment carries wanted. A refused read
// is not the same as an absent marker: for a process of this user it means
// the marker cannot be checked, which the scan reports instead of treating
// the process as foreign.
func sessionMarker(pid uint32, wanted []byte) WorkFactsMarker {
	environ, err := os.ReadFile(fmt.Sprintf("/proc/%d/environ", pid))
	switch {
	case err == nil:
		for _, entry := range bytes.Split(environ, []byte{0}) {
			if bytes.Equal(entry, wanted) {
				return WorkFactsMarkerPresent
			}
		}
		return WorkFactsMarkerAbsent
	case errors.Is(err, fs.ErrPermission):
		info, err := os.Stat(fmt.Sprintf("/proc/%d", pid))
		if err == nil {
			if stat, ok := info.Sys().(*syscall.Stat_t); ok && stat.Uid == uint32(os.Geteuid()) {
				return WorkFactsMarkerUnreadable
			}
		}
		return WorkFactsMarkerAbsent
	}
	// The process exited, or has no environment to read.
	return WorkFactsMarkerAbsent
}

// readTable is every process the scan may consider, and the reader for the
// survivors.
func readTable(sessionID string) (ProcessTable, error) {
	hz, err := ticksPerSecond()
	if err != nil {
		return ProcessTable{}, err
	}
	// One instant for every age: the process ages are measured against this
	// uptime, and the harness's age against the wall clock read beside it.
	uptime, err := uptimeMs()
	if err != nil {
		return ProcessTable{}, err
	}
	sampledMs := core.EpochMillis()
	bootIDText, err := os.ReadFile("/proc/sys/kernel/random/boot_id")
	if err != nil {
		return ProcessTable{}, fmt.Errorf("read the boot identity: %w", err)
	}
	bootID := strings.TrimSpace(string(bootIDText))
	wanted := []byte("MJ_SESSION_ID=" + sessionID)
	listing, err := os.Open("/proc")
	if err != nil {
		return ProcessTable{}, fmt.Errorf("list processes: %w", err)
	}
	// An entry that cannot be read is skipped, as one that is not a process.
	names, _ := listing.Readdirnames(-1)
	_ = listing.Close()
	var rows []ProcRow
	for _, name := range names {
		pid, err := strconv.ParseUint(name, 10, 32)
		if err != nil {
			continue
		}
		// A process may exit between the listing and the read.
		text, err := workFactsReadText(fmt.Sprintf("/proc/%d/stat", pid))
		if err != nil {
			continue
		}
		stat, ok := parseStat(text)
		if !ok {
			continue
		}
		startMs := int64(0)
		if ticks := workFactsSaturatingMul(stat.startTicks, 1000) / hz; ticks <= math.MaxInt64 {
			startMs = int64(ticks)
		}
		rows = append(rows, ProcRow{
			Pid:    uint32(pid),
			Ppid:   stat.ppid,
			AgeMs:  max(workFactsSaturatingSub(uptime, startMs), 0),
			Zombie: stat.zombie,
			Marker: sessionMarker(uint32(pid), wanted),
		})
	}
	// The start tick is read again for survivors only, so the identity
	// matches the one PROCESS_BIRTH_SCRIPT prints for the same process.
	reader := newProcessReader(func(row *ProcRow) Described { return describe(row.Pid, bootID) })
	perTick := int64(100)
	if hz <= math.MaxInt64 {
		perTick = int64(hz)
	}
	return ProcessTable{
		Rows:      rows,
		Reader:    reader,
		SampledMs: sampledMs,
		// A start is a count of clock ticks, and /proc/uptime has its own
		// hundredths of a second.
		ResolutionMs: max(1000/perTick, 10),
	}, nil
}

func workFactsSaturatingMul(a, b uint64) uint64 {
	if a != 0 && b > math.MaxUint64/a {
		return math.MaxUint64
	}
	return a * b
}

func describe(pid uint32, bootID string) Described {
	command := ""
	if arguments, err := os.ReadFile(fmt.Sprintf("/proc/%d/cmdline", pid)); err == nil {
		var parts []string
		for _, argument := range bytes.Split(arguments, []byte{0}) {
			parts = append(parts, captureLossy(argument))
		}
		command = strings.Join(parts, " ")
	}
	if strings.TrimSpace(command) == "" {
		// A process without arguments, such as a kernel thread, has only
		// its name.
		command = ""
		if text, err := workFactsReadText(fmt.Sprintf("/proc/%d/stat", pid)); err == nil {
			if _, after, ok := strings.Cut(text, "("); ok {
				if close := strings.LastIndexByte(after, ')'); close >= 0 {
					command = "[" + after[:close] + "]"
				}
			}
		}
	}
	var exe *string
	if target, err := os.Readlink(fmt.Sprintf("/proc/%d/exe", pid)); err == nil {
		// A replaced executable reads back with this suffix.
		target = strings.TrimSuffix(target, " (deleted)")
		exe = &target
	}
	identity := ""
	if text, err := workFactsReadText(fmt.Sprintf("/proc/%d/stat", pid)); err == nil {
		if stat, ok := parseStat(text); ok {
			identity = fmt.Sprintf("%s:%d", bootID, stat.startTicks)
		}
	}
	return Described{Command: command, Exe: exe, Identity: identity}
}

// workFactsReadText is Rust's fs::read_to_string: the file's text, failing when
// it is not UTF-8.
func workFactsReadText(path string) (string, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return "", err
	}
	if !utf8.Valid(data) {
		return "", errors.New("stream did not contain valid UTF-8")
	}
	return string(data), nil
}
