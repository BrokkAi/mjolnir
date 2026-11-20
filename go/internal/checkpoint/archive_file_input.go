// Port of mj-checkpoint/src/archive/file_input.rs.

package checkpoint

import "io"

// ReadSessionFileInput reads a file sent to a session, bounding input on
// both sides of the transport, including stdin with no length. A file over
// MaxSessionFileBytes is a SessionExportError refusal.
func ReadSessionFileInput(reader io.Reader) ([]byte, error) {
	panic("unported: mj-checkpoint/src/archive/file_input.rs read_session_file_input")
}

// WriteSessionFile publishes a complete file in an idle session's workspace.
// The caller owns the worker barrier through completion, so queued harness
// work cannot race it. A refusal is a SessionExportError refusal; any other
// failure is a SessionExportError failure.
func WriteSessionFile(root, relative string, bytes []byte, overwrite bool) error {
	panic("unported: mj-checkpoint/src/archive/file_input.rs write_session_file")
}
