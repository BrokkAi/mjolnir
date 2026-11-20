// Port of the `cfg(unix)` fixtures of mj-worker/src/test_support.rs.

//go:build unix

package worker

import (
	"os"
	"syscall"
)

// stalledReadFile makes path a FIFO and returns its write end, opened read
// and write: a real file read of path cannot finish until this writer is
// closed. Keeping the writer in the test also releases the read if an
// assertion fails.
func stalledReadFile(path string) *os.File {
	if err := syscall.Mkfifo(path, 0o600); err != nil {
		panic(err)
	}
	file, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		panic(err)
	}
	return file
}
