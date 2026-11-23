// Port of mj-worker/src/worker_stop/suspension/darwin_environment.rs:
// contains_marker, compiled for tests everywhere so its parsing is tested
// here; it runs on macOS only.

//go:build linux || darwin

package worker

import (
	"bytes"
	"encoding/binary"
)

// containsMarker reports whether the KERN_PROCARGS2 buffer arguments holds
// marker as one exact environment record. Darwin returns argc, exec_path
// with NUL padding, argv, then envp. See Apple's adv_cmds/ps/print.c,
// getproclline.
func containsMarker(arguments []byte, marker []byte) bool {
	if len(arguments) < 4 {
		return false
	}
	argc := int32(binary.NativeEndian.Uint32(arguments[:4]))
	if argc < 0 {
		return false
	}
	strings := arguments[4:]
	endOfPath := bytes.IndexByte(strings, 0)
	if endOfPath < 0 {
		return false
	}
	paddedArgs := strings[endOfPath:]
	start := bytes.IndexFunc(paddedArgs, func(r rune) bool { return r != 0 })
	if start < 0 {
		return false
	}
	if argc == 0 {
		return false
	}
	entries := bytes.Split(paddedArgs[start:], []byte{0})
	if int(argc) >= len(entries) {
		return false
	}
	for _, entry := range entries[argc:] {
		if len(entry) == 0 {
			return false
		}
		if bytes.Equal(entry, marker) {
			return true
		}
	}
	return false
}
