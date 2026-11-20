// Port of the Linux `imp` of mj-worker/src/worker_stop/process_table.rs.

package worker

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"strconv"
	"strings"
	"syscall"
)

// processTableLookup is pid's entry; false when no such process exists. A
// process can vanish between listing and reading it.
func processTableLookup(pid uint32) (processEntry, bool, error) {
	text, err := os.ReadFile(fmt.Sprintf("/proc/%d/stat", pid))
	if err != nil {
		if errors.Is(err, fs.ErrNotExist) || errors.Is(err, syscall.ESRCH) {
			return processEntry{}, false, nil
		}
		return processEntry{}, false, err
	}
	entry, err := processTableParse(pid, string(text))
	if err != nil {
		return processEntry{}, false, err
	}
	return entry, true, nil
}

// processTableList is every process the kernel shows in /proc.
func processTableList() ([]processEntry, error) {
	directory, err := os.Open("/proc")
	if err != nil {
		return nil, err
	}
	defer directory.Close()
	names, err := directory.Readdirnames(-1)
	if err != nil {
		return nil, err
	}
	var entries []processEntry
	for _, name := range names {
		pid, err := strconv.ParseUint(name, 10, 32)
		if err != nil {
			continue
		}
		entry, found, err := processTableLookup(uint32(pid))
		if err != nil {
			return nil, err
		}
		if found {
			entries = append(entries, entry)
		}
	}
	return entries, nil
}

// processTableParse reads one /proc/<pid>/stat text. The command name may
// hold spaces and parentheses, so fields are counted from the last `)`. The
// tail starts at field 3 (state): ppid is field 4, pgrp 5 and starttime 22.
func processTableParse(pid uint32, text string) (processEntry, error) {
	invalid := fmt.Errorf("bad /proc/%d/stat", pid)
	end := strings.LastIndexByte(text, ')')
	if end < 0 {
		return processEntry{}, invalid
	}
	fields := strings.Fields(text[end+1:])
	field := func(index int) (string, bool) {
		if index >= len(fields) {
			return "", false
		}
		return fields[index], true
	}
	number := func(index int, bits int) (uint64, error) {
		text, ok := field(index)
		if !ok {
			return 0, invalid
		}
		value, err := strconv.ParseUint(text, 10, bits)
		if err != nil {
			return 0, invalid
		}
		return value, nil
	}
	ppid, err := number(1, 32)
	if err != nil {
		return processEntry{}, err
	}
	pgid, err := number(2, 32)
	if err != nil {
		return processEntry{}, err
	}
	start, err := number(19, 64)
	if err != nil {
		return processEntry{}, err
	}
	state, ok := field(0)
	if !ok {
		return processEntry{}, invalid
	}
	return processEntry{
		pid:    pid,
		ppid:   uint32(ppid),
		pgid:   uint32(pgid),
		start:  start,
		zombie: state == "Z" || state == "X",
	}, nil
}
