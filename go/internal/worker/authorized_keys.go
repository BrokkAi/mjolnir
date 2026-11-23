// Port of mj-worker/src/authorized_keys.rs.

// Locked `authorized_keys` editor for temporary direct-transfer SSH grants.
//
// Every edit to `~/.ssh/authorized_keys` (install, remove, reconcile) goes
// through this file. It takes an exclusive `flock` on
// `authorized_keys.mj-lock` beside `authorized_keys`, preserves `0700` on the
// parent `.ssh` directory and `0600` on `authorized_keys`, writes to a
// temporary file in the same directory, fsyncs, and atomically renames into
// place.

package worker

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"unicode"
)

// MoveKeyCommentPrefix is the comment prefix identifying a Mjolnir Move
// transfer key.
const MoveKeyCommentPrefix = "mj-move-"

// LockFileName is the lock file name placed beside `authorized_keys`.
const LockFileName = "authorized_keys.mj-lock"

// MoveKeyComment formats the canonical comment for a move operation's
// temporary key.
func MoveKeyComment(operationID string) (string, error) {
	if err := ValidateOperationID(operationID); err != nil {
		return "", err
	}
	return MoveKeyCommentPrefix + operationID, nil
}

// ValidateOperationID validates an operation identifier before using it in a
// key comment.
func ValidateOperationID(operationID string) error {
	if operationID == "" {
		return errors.New("operation_id must not be empty")
	}
	for i := 0; i < len(operationID); i++ {
		b := operationID[i]
		if !isASCIIAlphanumericByte(b) && b != '-' && b != '_' {
			return errors.New("operation_id must contain only ASCII alphanumeric characters, '-' or '_'")
		}
	}
	return nil
}

func isASCIIAlphanumericByte(b byte) bool {
	return (b >= 'a' && b <= 'z') || (b >= 'A' && b <= 'Z') || (b >= '0' && b <= '9')
}

// validateOptionValue validates an SSH `authorized_keys` option value
// (`from` or `command`).
//
// It rejects control characters (including CR/LF/NUL) as well as `"` and
// `\` so the value cannot break out of double-quoted option syntax.
func validateOptionValue(label, value string) error {
	if value == "" {
		return fmt.Errorf("%s must not be empty", label)
	}
	if strings.ContainsFunc(value, func(c rune) bool {
		return unicode.IsControl(c) || c == '"' || c == '\\'
	}) {
		return fmt.Errorf("%s contains forbidden control or quoting characters", label)
	}
	return nil
}

// validateEd25519Pubkey validates an ed25519 public key line before
// embedding it in `authorized_keys`.
func validateEd25519Pubkey(publicKey string) (string, error) {
	trimmed := strings.TrimSpace(publicKey)
	if trimmed == "" {
		return "", errors.New("public_key must not be empty")
	}
	if strings.ContainsFunc(trimmed, unicode.IsControl) {
		return "", errors.New("public_key contains control characters")
	}
	parts := strings.Fields(trimmed)
	if len(parts) < 1 {
		return "", errors.New("public_key missing key type")
	}
	keyType := parts[0]
	if len(parts) < 2 {
		return "", errors.New("public_key missing key data")
	}
	keyData := parts[1]
	if keyType != "ssh-ed25519" {
		return "", fmt.Errorf("expected ssh-ed25519 public key, got %s", keyType)
	}
	for i := 0; i < len(keyData); i++ {
		b := keyData[i]
		if !isASCIIAlphanumericByte(b) && b != '+' && b != '/' && b != '=' {
			return "", errors.New("public_key contains invalid base64 characters")
		}
	}
	return keyType + " " + keyData, nil
}

// FormatRestrictedLine formats a single restricted `authorized_keys` line
// for a Move transfer.
func FormatRestrictedLine(sourceAddress, restrictedWorkerCommand, ed25519Pubkey, operationID string) (string, error) {
	if err := validateOptionValue("source_address", sourceAddress); err != nil {
		return "", err
	}
	if err := validateOptionValue("restricted_worker_command", restrictedWorkerCommand); err != nil {
		return "", err
	}
	pubkey, err := validateEd25519Pubkey(ed25519Pubkey)
	if err != nil {
		return "", err
	}
	comment, err := MoveKeyComment(operationID)
	if err != nil {
		return "", err
	}
	return fmt.Sprintf("restrict,from=\"%s\",command=\"%s\" %s %s", sourceAddress, restrictedWorkerCommand, pubkey, comment), nil
}

// managedMoveKeyComment extracts the `mj-move-<id>` comment token ONLY if
// line matches the full managed key shape emitted by FormatRestrictedLine:
// `restrict,from="...",command="..." ssh-ed25519 <base64> mj-move-<id>`.
//
// Unrelated user keys (even those whose comment happens to end with
// `mj-move-<id>`) return false and are never modified.
func managedMoveKeyComment(line string) (string, bool) {
	trimmed := strings.TrimSpace(line)
	afterFromPrefix, ok := strings.CutPrefix(trimmed, "restrict,from=\"")
	if !ok {
		return "", false
	}
	_, afterFrom, ok := strings.Cut(afterFromPrefix, "\",command=\"")
	if !ok {
		return "", false
	}
	_, rest, ok := strings.Cut(afterFrom, "\" ")
	if !ok {
		return "", false
	}
	tokens := strings.Fields(rest)
	if len(tokens) != 3 {
		return "", false
	}
	keyType, comment := tokens[0], tokens[2]
	if keyType != "ssh-ed25519" || !strings.HasPrefix(comment, MoveKeyCommentPrefix) {
		return "", false
	}
	return comment, true
}

// hasManagedComment reports whether line is a managed Move key carrying
// exactly comment.
func hasManagedComment(line, comment string) bool {
	found, ok := managedMoveKeyComment(line)
	return ok && found == comment
}

// InstallKey installs (or idempotently replaces) a restricted temporary key
// in path.
func InstallKey(path, operationID, sourceAddress, restrictedWorkerCommand, ed25519Pubkey string) error {
	comment, err := MoveKeyComment(operationID)
	if err != nil {
		return err
	}
	formatted, err := FormatRestrictedLine(sourceAddress, restrictedWorkerCommand, ed25519Pubkey, operationID)
	if err != nil {
		return err
	}
	return withLockedAuthorizedKeys(path, true, func(lines *[]string) (bool, error) {
		kept := (*lines)[:0]
		for _, line := range *lines {
			if !hasManagedComment(line, comment) {
				kept = append(kept, line)
			}
		}
		*lines = append(kept, formatted)
		return true, nil
	})
}

// RemoveKey removes only lines carrying the exact comment
// `mj-move-<operation_id>`.
//
// It is idempotent when the file or the key line does not exist, and
// reports whether at least one line was removed.
func RemoveKey(path, operationID string) (bool, error) {
	comment, err := MoveKeyComment(operationID)
	if err != nil {
		return false, err
	}
	removed := false
	err = withLockedAuthorizedKeys(path, false, func(lines *[]string) (bool, error) {
		before := len(*lines)
		kept := (*lines)[:0]
		for _, line := range *lines {
			if !hasManagedComment(line, comment) {
				kept = append(kept, line)
			}
		}
		*lines = kept
		removed = len(kept) != before
		return removed, nil
	})
	if err != nil {
		return false, err
	}
	return removed, nil
}

// ReconcileKeys removes any `mj-move-*` key lines whose operation ID is not
// in activeOperations. Unrelated user keys are never modified.
//
// It returns the number of removed lines.
func ReconcileKeys(path string, activeOperations map[string]struct{}) (int, error) {
	for op := range activeOperations {
		if err := ValidateOperationID(op); err != nil {
			return 0, err
		}
	}
	removed := 0
	err := withLockedAuthorizedKeys(path, false, func(lines *[]string) (bool, error) {
		before := len(*lines)
		kept := (*lines)[:0]
		for _, line := range *lines {
			comment, ok := managedMoveKeyComment(line)
			if !ok {
				kept = append(kept, line)
				continue
			}
			opID, ok := strings.CutPrefix(comment, MoveKeyCommentPrefix)
			if !ok {
				kept = append(kept, line)
				continue
			}
			if _, active := activeOperations[opID]; active {
				kept = append(kept, line)
			}
		}
		*lines = kept
		removed = before - len(kept)
		return removed > 0, nil
	})
	if err != nil {
		return 0, err
	}
	return removed, nil
}

// withLockedAuthorizedKeys runs edit while holding an exclusive `flock` on
// `authorized_keys.mj-lock` beside path. If edit reports a change, it
// atomically rewrites path with mode `0600`.
func withLockedAuthorizedKeys(path string, createParentIfMissing bool, edit func(*[]string) (bool, error)) error {
	// Path::parent: none for a root, empty for a bare file name.
	parent := filepath.Dir(path)
	if parent == filepath.Clean(path) || filepath.Base(path) == path {
		return errors.New("authorized_keys path must have a parent directory")
	}

	if _, err := os.Stat(parent); err != nil {
		if !createParentIfMissing {
			return nil
		}
		if err := createSSHDir(parent); err != nil {
			return err
		}
	}

	lockPath := filepath.Join(parent, LockFileName)
	lock, err := acquireExclusiveFileLock(lockPath)
	if err != nil {
		return err
	}
	defer lock.release()

	_, statErr := os.Stat(path)
	exists := statErr == nil
	if !exists && !createParentIfMissing {
		return nil
	}

	existing := ""
	if exists {
		body, err := os.ReadFile(path)
		if err != nil {
			return fmt.Errorf("read authorized_keys %s: %w", path, err)
		}
		existing = string(body)
	}

	lines := authorizedKeysLines(existing)
	changed, err := edit(&lines)
	if err != nil {
		return err
	}
	if !changed {
		return nil
	}

	temp, err := os.CreateTemp(parent, ".tmp")
	if err != nil {
		return fmt.Errorf("create temp authorized_keys in %s: %w", parent, err)
	}
	persisted := false
	defer func() {
		if !persisted {
			_ = temp.Close()
			_ = os.Remove(temp.Name())
		}
	}()
	if err := setFileMode0600(temp); err != nil {
		return err
	}
	var body strings.Builder
	for _, line := range lines {
		body.WriteString(line)
		body.WriteByte('\n')
	}
	if _, err := temp.WriteString(body.String()); err != nil {
		return fmt.Errorf("write temp authorized_keys in %s: %w", parent, err)
	}
	if err := temp.Sync(); err != nil {
		return fmt.Errorf("fsync temp authorized_keys in %s: %w", parent, err)
	}
	if err := temp.Close(); err != nil {
		return fmt.Errorf("write temp authorized_keys in %s: %w", parent, err)
	}
	if err := os.Rename(temp.Name(), path); err != nil {
		return fmt.Errorf("rename temp authorized_keys into %s: %w", path, err)
	}
	persisted = true
	return nil
}

// authorizedKeysLines is str::lines: lines split on `\n` with one trailing
// `\r` removed, and no final empty line for a trailing newline.
func authorizedKeysLines(text string) []string {
	var lines []string
	for text != "" {
		line, rest, found := strings.Cut(text, "\n")
		lines = append(lines, strings.TrimSuffix(line, "\r"))
		if !found {
			break
		}
		text = rest
	}
	return lines
}

func createSSHDir(dir string) error {
	if err := os.MkdirAll(dir, 0o777); err != nil {
		return fmt.Errorf("create ssh directory %s: %w", dir, err)
	}
	return chmodSSHDir(dir)
}

// exclusiveFileLock is an exclusive `flock`-backed file lock released by
// release.
type exclusiveFileLock struct {
	file *os.File
	path string
}

func acquireExclusiveFileLock(path string) (*exclusiveFileLock, error) {
	file, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE, 0o666)
	if err != nil {
		return nil, fmt.Errorf("open lock file %s: %w", path, err)
	}
	if err := authorizedKeysLockExclusive(file); err != nil {
		_ = file.Close()
		return nil, fmt.Errorf("flock %s failed: %w", path, err)
	}
	return &exclusiveFileLock{file: file, path: path}, nil
}

func (lock *exclusiveFileLock) release() {
	authorizedKeysUnlock(lock.file)
	_ = lock.file.Close()
}
