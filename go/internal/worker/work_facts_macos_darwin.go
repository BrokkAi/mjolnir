// Port of mj-worker/src/work_facts/macos.rs: the scan itself, which Rust
// builds on macOS only (#[cfg(target_os = "macos")]; here the _darwin file
// suffix).

package worker

import (
	"math"
	"os"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// readTable is every process the scan may consider, from two ps listings,
// and the reader for the survivors.
func readTable(sessionID string) (ProcessTable, error) {
	wanted := "MJ_SESSION_ID=" + sessionID
	environment, err := ps([]string{"-axwwE", "-o", "pid=,command="})
	if err != nil {
		return ProcessTable{}, err
	}
	environmentLines := map[uint32]string{}
	for _, line := range rustLines(environment) {
		fields := strings.Fields(line)
		if len(fields) == 0 {
			continue
		}
		pid, err := strconv.ParseUint(fields[0], 10, 32)
		if err != nil {
			continue
		}
		environmentLines[uint32(pid)] = line
	}
	me := uint32(os.Geteuid())
	// etime is measured when this ps runs; the harness's age is taken at the
	// same instant.
	sampledMs := core.EpochMillis()
	listed, err := ps(listing)
	if err != nil {
		return ProcessTable{}, err
	}
	var rows []ProcRow
	commands := map[uint32]string{}
	for _, line := range rustLines(listed) {
		fields := strings.Fields(line)
		if len(fields) < 5 {
			continue
		}
		pid, pidErr := strconv.ParseUint(fields[0], 10, 32)
		ppid, ppidErr := strconv.ParseUint(fields[1], 10, 32)
		uid, uidErr := strconv.ParseUint(fields[2], 10, 32)
		elapsed, elapsedOK := parseElapsedSecs(fields[3])
		if pidErr != nil || ppidErr != nil || uidErr != nil || !elapsedOK {
			continue
		}
		command := strings.Join(fields[5:], " ")
		if uint32(ppid) == uint32(os.Getpid()) && command == ownPsCommand(listing) {
			continue
		}
		var environmentLine *string
		if line, ok := environmentLines[uint32(pid)]; ok {
			environmentLine = &line
		}
		marker := classifyMarker(environmentLine, command, uint32(uid) == me, wanted)
		commands[uint32(pid)] = command
		// Rust's saturating_mul.
		ageMs, ok := macosCheckedMul(elapsed, 1000)
		if !ok {
			ageMs = math.MaxInt64
			if elapsed < 0 {
				ageMs = math.MinInt64
			}
		}
		rows = append(rows, ProcRow{
			Pid:    uint32(pid),
			Ppid:   uint32(ppid),
			AgeMs:  ageMs,
			Zombie: strings.HasPrefix(fields[4], "Z"),
			Marker: marker,
		})
	}
	reader := newProcessReader(func(row *ProcRow) Described {
		command := commands[row.Pid]
		var exe *string
		if program, _, _ := strings.Cut(command, " "); strings.HasPrefix(program, "/") {
			exe = &program
		}
		identity, err := core.ProcessBirthIdentity(row.Pid)
		if err != nil {
			identity = ""
		}
		return Described{Command: command, Exe: exe, Identity: identity}
	})
	return ProcessTable{
		Rows:      rows,
		Reader:    reader,
		SampledMs: sampledMs,
		// etime is whole seconds.
		ResolutionMs: 1000,
	}, nil
}
