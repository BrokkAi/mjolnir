// Port of mj-worker-launch/src/local_profile_homes.rs: sysinfo's process
// command lines on Linux, read from /proc.

package workerlaunch

import (
	"bytes"
	"os"
	"path/filepath"
	"strconv"
	"strings"
)

// processCommandLines is the command line of every process on this machine,
// each one's arguments joined with spaces, as sysinfo reads them with
// ProcessRefreshKind::new().with_cmd(UpdateKind::Always). A process whose
// command line cannot be read lists as "".
//
// sysinfo also lists each thread of a process, with the process's own
// command line; the listing is only searched for arguments, so the threads
// would add nothing and are not read.
func processCommandLines() []string {
	entries, err := os.ReadDir("/proc")
	if err != nil {
		return nil
	}
	var lines []string
	for _, entry := range entries {
		if !entry.IsDir() {
			continue
		}
		if _, err := strconv.ParseUint(entry.Name(), 10, 64); err != nil {
			continue
		}
		lines = append(lines, linuxCommandLine(filepath.Join("/proc", entry.Name(), "cmdline")))
	}
	return lines
}

// linuxCommandLine reads one `cmdline` file as sysinfo's copy_from_file
// does: each NUL-terminated argument with ASCII whitespace trimmed from both
// ends, empty arguments and an unterminated tail dropped, then joined with
// spaces.
func linuxCommandLine(path string) string {
	data, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	var arguments []string
	for {
		end := bytes.IndexByte(data, 0)
		if end < 0 {
			break
		}
		if argument := bytes.TrimFunc(data[:end], isASCIIWhitespace); len(argument) != 0 {
			arguments = append(arguments, lossyText(argument))
		}
		data = data[end+1:]
	}
	return strings.Join(arguments, " ")
}

// isASCIIWhitespace is Rust's u8::is_ascii_whitespace: space, tab, line
// feed, form feed and carriage return (not vertical tab).
func isASCIIWhitespace(r rune) bool {
	return r == ' ' || r == '\t' || r == '\n' || r == '\f' || r == '\r'
}
