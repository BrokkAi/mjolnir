// Port of mj-checkpoint/src/archive/file_input.rs.

package checkpoint

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ReadSessionFileInput reads a file sent to a session, bounding input on
// both sides of the transport, including stdin with no length. A file over
// MaxSessionFileBytes is a SessionExportError refusal.
func ReadSessionFileInput(reader io.Reader) ([]byte, error) {
	var buffer bytes.Buffer
	if _, err := buffer.ReadFrom(io.LimitReader(reader, int64(MaxSessionFileBytes)+1)); err != nil {
		return nil, SessionExportErrorFailed{fmt.Errorf("read incoming session file: %w", err)}
	}
	if uint64(buffer.Len()) > MaxSessionFileBytes {
		return nil, SessionExportErrorRefused(fmt.Sprintf("file exceeds %d bytes", MaxSessionFileBytes))
	}
	return buffer.Bytes(), nil
}

// WriteSessionFile publishes a complete file in an idle session's workspace.
// The caller owns the worker barrier through completion, so queued harness
// work cannot race it. A refusal is a SessionExportError refusal; any other
// failure is a SessionExportError failure.
func WriteSessionFile(root, relative string, bytes []byte, overwrite bool) error {
	refuse := func(err error) error { return SessionExportErrorRefused(err.Error()) }
	if err := core.ValidateRelativeDestination(relative); err != nil {
		return refuse(err)
	}
	if uint64(len(bytes)) > MaxSessionFileBytes {
		return SessionExportErrorRefused(fmt.Sprintf("file exceeds %d bytes", MaxSessionFileBytes))
	}
	resolvedRoot, err := archiveCanonicalize(root)
	if err != nil {
		return SessionExportErrorFailed{fmt.Errorf("resolve session workspace: %w", err)}
	}
	// The workspace's own processes can rename and relink its directories
	// while this runs, so every step resolves from handles opened without
	// following a link rather than from path text (#1419).
	confined, err := core.ConfinedDirOpenRoot(resolvedRoot)
	if err != nil {
		return SessionExportErrorFailed{err}
	}
	if confined == nil {
		return SessionExportErrorFailed{errors.New("session workspace disappeared")}
	}
	defer confined.Close()
	// The destination is validated: every component is a plain name.
	var names []string
	for _, component := range archivePathComponents(relative) {
		names = append(names, component.text)
	}
	if len(names) == 0 {
		return SessionExportErrorFailed{errors.New("file destination has no name")}
	}
	parent, name := names[:len(names)-1], names[len(names)-1]
	directory, err := confined.CreateDirAll(filepath.Join(parent...))
	if err != nil {
		return refuse(err)
	}
	defer directory.Close()
	publish := core.PublishCreateNew
	if overwrite {
		publish = core.PublishReplace
	}
	switch failure := directory.WriteAtomically(name, bytes, publish).(type) {
	case nil:
		return nil
	case core.PublishErrorNotARegularFile:
		return SessionExportErrorRefused("destination is not a regular file")
	case core.PublishErrorAlreadyExists:
		return SessionExportErrorRefused("file already exists; request overwrite explicitly")
	case core.PublishErrorFailed:
		return SessionExportErrorFailed{fmt.Errorf("publish session file: %w", failure.Err)}
	default:
		return SessionExportErrorFailed{fmt.Errorf("publish session file: %w", failure)}
	}
}
