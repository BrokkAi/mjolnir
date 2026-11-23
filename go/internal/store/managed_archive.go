// Port of mj-store/src/managed_archive.rs.
//
// The file names of the checkpoint archives Mjolnir manages.

package store

import (
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// ManagedCheckpointArchiveName is the session and checkpoint generation a
// managed archive file name names.
//
// Managed checkpoints are named `<session_id>-<frontier>-archive-<32 hex>`,
// so the file name alone says which session a checkpoint belongs to and which
// of that session's checkpoints is the newest.
type ManagedCheckpointArchiveName struct {
	SessionID string
	Frontier  uint64
}

// ParseManagedCheckpointArchiveName is Rust's managed_checkpoint_archive_name
// (renamed because Go cannot give the function and the
// ManagedCheckpointArchiveName type the same name). It parses a managed
// checkpoint archive file name, or returns nil when the name is not one
// Mjolnir wrote. name is the file name (Rust's OsStr); a name that is not
// valid UTF-8 is not one Mjolnir wrote.
func ParseManagedCheckpointArchiveName(name string) *ManagedCheckpointArchiveName {
	if !utf8.ValidString(name) {
		return nil
	}
	stem, ok := strings.CutSuffix(name, ".hel.zip")
	if !ok {
		return nil
	}
	index := strings.LastIndex(stem, "-archive-")
	if index < 0 {
		return nil
	}
	frontierPrefix, nonce := stem[:index], stem[index+len("-archive-"):]
	if len(nonce) != 32 {
		return nil
	}
	for i := 0; i < len(nonce); i++ {
		b := nonce[i]
		if !('0' <= b && b <= '9' || 'a' <= b && b <= 'f') {
			return nil
		}
	}
	index = strings.LastIndexByte(frontierPrefix, '-')
	if index < 0 {
		return nil
	}
	sessionID, frontierText := frontierPrefix[:index], frontierPrefix[index+1:]
	if sessionID == "" || core.ValidateID("session", sessionID) != nil {
		return nil
	}
	frontier, ok := managedArchiveParseU64(frontierText)
	if !ok {
		return nil
	}
	return &ManagedCheckpointArchiveName{SessionID: sessionID, Frontier: frontier}
}

// managedArchiveParseU64 is Rust's str::parse::<u64>, which accepts one
// leading '+' that strconv.ParseUint does not.
func managedArchiveParseU64(text string) (uint64, bool) {
	digits := strings.TrimPrefix(text, "+")
	if digits == "" || digits[0] == '+' || digits[0] == '-' {
		return 0, false
	}
	value, err := strconv.ParseUint(digits, 10, 64)
	return value, err == nil
}
