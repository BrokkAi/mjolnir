// Port of brokk-sessionwiki 0.34.28 src/util.rs.

package brokksessionwiki

import (
	"fmt"
	"path/filepath"

	"golang.org/x/text/unicode/norm"
)

// ShortID is a stable short id from a path string: the first 12 hex digits
// of its FNV-1a hash. FNV-1a is written out because the hash must not change
// between releases.
func ShortID(s string) string {
	hash := uint64(0xcbf29ce484222325)
	for index := 0; index < len(s); index++ {
		hash ^= uint64(s[index])
		hash *= 0x100000001b3
	}
	return fmt.Sprintf("%016x", hash)[:12]
}

// NFC is s in Unicode Normalization Form C.
func NFC(s string) string { return norm.NFC.String(s) }

// CanonicalizeAsRecorded is std::fs::canonicalize, spelled as tools record a
// folder. On Windows Rust's canonicalize answers `\\?\C:\...`, a spelling no
// tool records; Go's symlink resolution never adds that prefix.
func CanonicalizeAsRecorded(path string) (string, error) {
	absolute, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	return filepath.EvalSymlinks(absolute)
}
