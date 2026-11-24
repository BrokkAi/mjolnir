//go:build !windows

// Port of mj-worker-staging/src/worker_binary/install.rs: the Unix half of
// the profile archive's spool (tempfile::tempfile).

package workerstaging

import "os"

// profileArchiveSpool creates a file in the temporary directory that has no
// name: it is unlinked as soon as it is open, so nothing is left behind when
// it closes.
func profileArchiveSpool() (*os.File, error) {
	file, err := os.CreateTemp("", ".tmp")
	if err != nil {
		return nil, err
	}
	if err := os.Remove(file.Name()); err != nil {
		file.Close()
		return nil, err
	}
	return file, nil
}
