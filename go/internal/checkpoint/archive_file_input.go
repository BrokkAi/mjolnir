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

// WriteSessionFile publishes a complete file in an idle session's workspace.
// The caller owns the worker barrier through completion, so queued harness
// work cannot race it. A refusal is a SessionExportError refusal; any other
// failure is a SessionExportError failure.
func WriteSessionFile(root, relative string, data []byte, overwrite bool) error {
	length := uint64(len(data))
	_, err := WriteSessionFileFromReader(root, relative, bytes.NewReader(data), &length, overwrite)
	return err
}

// WriteSessionFileFromReader streams a file from reader and atomically
// publishes it in an idle session's workspace. When expectedLen is set, it
// rejects short reads and trailing extra bytes without publishing.
func WriteSessionFileFromReader(root, relative string, reader io.Reader, expectedLen *uint64, overwrite bool) (uint64, error) {
	refuse := func(err error) error { return SessionExportErrorRefused(err.Error()) }
	if err := core.ValidateRelativeDestination(relative); err != nil {
		return 0, refuse(err)
	}
	resolvedRoot, err := archiveCanonicalize(root)
	if err != nil {
		return 0, SessionExportErrorFailed{fmt.Errorf("resolve session workspace: %w", err)}
	}
	// The workspace's own processes can rename and relink its directories
	// while this runs, so every step resolves from handles opened without
	// following a link rather than from path text (#1419).
	confined, err := core.ConfinedDirOpenRoot(resolvedRoot)
	if err != nil {
		return 0, SessionExportErrorFailed{err}
	}
	if confined == nil {
		return 0, SessionExportErrorFailed{errors.New("session workspace disappeared")}
	}
	defer confined.Close()
	// The destination is validated: every component is a plain name.
	var names []string
	for _, component := range archivePathComponents(relative) {
		names = append(names, component.text)
	}
	if len(names) == 0 {
		return 0, SessionExportErrorFailed{errors.New("file destination has no name")}
	}
	parent, name := names[:len(names)-1], names[len(names)-1]
	directory, err := confined.CreateDirAll(filepath.Join(parent...))
	if err != nil {
		return 0, refuse(err)
	}
	defer directory.Close()
	publish := core.PublishCreateNew
	if overwrite {
		publish = core.PublishReplace
	}
	written, failure := directory.WriteAtomicallyFromReader(name, reader, expectedLen, publish)
	switch failure := failure.(type) {
	case nil:
		return written, nil
	case core.PublishErrorNotARegularFile:
		return 0, SessionExportErrorRefused("destination is not a regular file")
	case core.PublishErrorAlreadyExists:
		return 0, SessionExportErrorRefused("file already exists; request overwrite explicitly")
	case core.PublishErrorLengthMismatch:
		return 0, SessionExportErrorRefused(fmt.Sprintf(
			"incomplete file upload: expected %d bytes, received %d", failure.Expected, failure.Received))
	case core.PublishErrorFailed:
		return 0, SessionExportErrorFailed{fmt.Errorf("publish session file: %w", failure.Err)}
	default:
		return 0, SessionExportErrorFailed{fmt.Errorf("publish session file: %w", failure)}
	}
}
