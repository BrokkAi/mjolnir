// Port of mj-worker/src/work_facts/macos.rs.
//
// ps reader for the process scan. macOS has no /proc; `ps -E` appends a
// process's environment to its command line, which is how the session
// marker is found.
//
// Rust compiles the parsing here everywhere so it is tested on every
// platform, and runs it on macOS only; so does Go. readTable, which runs ps
// for a scan, is in work_facts_macos_darwin.go.

package worker

import (
	"fmt"
	"math"
	"os"
	"os/exec"
	"slices"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// parseElapsedSecs reads `[[dd-]hh:]mm:ss` as `ps -o etime` prints it;
// false when it is malformed or overflows.
func parseElapsedSecs(text string) (int64, bool) {
	days, clock := int64(0), text
	if before, after, ok := strings.Cut(text, "-"); ok {
		parsed, err := strconv.ParseInt(before, 10, 64)
		if err != nil {
			return 0, false
		}
		days, clock = parsed, after
	}
	seconds := int64(0)
	for _, part := range strings.Split(clock, ":") {
		value, err := strconv.ParseInt(part, 10, 64)
		if err != nil {
			return 0, false
		}
		var ok bool
		if seconds, ok = macosCheckedMul(seconds, 60); !ok {
			return 0, false
		}
		if seconds, ok = macosCheckedAdd(seconds, value); !ok {
			return 0, false
		}
	}
	total, ok := macosCheckedMul(days, 86_400)
	if !ok {
		return 0, false
	}
	return macosCheckedAdd(total, seconds)
}

// macosCheckedMul is Rust's i64::checked_mul.
func macosCheckedMul(a, b int64) (int64, bool) {
	if a == 0 || b == 0 {
		return 0, true
	}
	product := a * b
	if product/b != a || (a == -1 && b == math.MinInt64) || (b == -1 && a == math.MinInt64) {
		return 0, false
	}
	return product, true
}

// macosCheckedAdd is Rust's i64::checked_add.
func macosCheckedAdd(a, b int64) (int64, bool) {
	sum := a + b
	if (b > 0 && sum < a) || (b < 0 && sum > a) {
		return 0, false
	}
	return sum, true
}

// ownPsCommand is the command line ps shows for itself when run with args.
// The scan's own ps is a child of the worker that started after the harness
// was configured, so it would be reported as the agent's work without this.
func ownPsCommand(args []string) string {
	return "ps " + strings.Join(args, " ")
}

func ps(args []string) (string, error) {
	command := exec.Command("ps", args...)
	command.Env = append(os.Environ(), "LC_ALL=C")
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		return "", fmt.Errorf("run ps: %w", err)
	}
	if !output.Status.Success() {
		return "", fmt.Errorf("ps failed with %s", workFactsExitStatus(output.Status))
	}
	return fromUTF8Lossy(output.Stdout), nil
}

// listing is the ps arguments of the scan's process listing.
var listing = []string{"-axww", "-o", "pid=,ppid=,uid=,etime=,stat=,command="}

// classifyMarker is whether a process's environment carries the session
// marker, from the line `ps -E` printed for it (nil when it printed none).
// `ps -E` appends a process's environment to its command; for a process of
// the same user whose environment it cannot show it prints the command
// alone, which is the case reported as unreadable rather than as an absent
// marker (the Linux reader does the same for a refused read). A process of
// another user is not this session's and stays absent.
func classifyMarker(environmentLine *string, command string, sameUser bool, wanted string) WorkFactsMarker {
	if environmentLine == nil {
		return WorkFactsMarkerAbsent
	}
	// The line is `pid command... ENV=value ...`.
	words := strings.Fields(*environmentLine)
	if len(words) > 0 {
		words = words[1:]
	}
	switch {
	case slices.Contains(words, wanted):
		return WorkFactsMarkerPresent
	case sameUser && strings.Join(words, " ") == command:
		return WorkFactsMarkerUnreadable
	}
	return WorkFactsMarkerAbsent
}
