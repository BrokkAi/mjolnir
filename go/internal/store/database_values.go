// Port of mj-store/src/database/values.rs.

package store

import (
	"encoding/binary"
	"fmt"
	"runtime"
	"unicode/utf16"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// storedSessionState reads a stored session state. The schema's CHECK
// constraint admits only known session states.
func storedSessionState(value string) core.SessionState {
	state, ok := core.SessionStateFromStored(value)
	if !ok {
		panic("schema CHECK admits only known session states")
	}
	return state
}

// pathToBlob is a path's stored form: its bytes on Unix, its UTF-16LE code
// units on Windows, as Rust stores an OsStr.
func pathToBlob(path string) []byte {
	if runtime.GOOS != "windows" {
		return []byte(path)
	}
	wide := utf16.Encode([]rune(path))
	blob := make([]byte, 0, 2*len(wide))
	for _, unit := range wide {
		blob = binary.LittleEndian.AppendUint16(blob, unit)
	}
	return blob
}

// blobToPath reads a path stored by pathToBlob. On Windows a trailing odd
// byte is ignored, as Rust's as_chunks does.
func blobToPath(bytes []byte) string {
	if runtime.GOOS != "windows" {
		return string(bytes)
	}
	wide := make([]uint16, 0, len(bytes)/2)
	for index := 0; index+1 < len(bytes); index += 2 {
		wide = append(wide, binary.LittleEndian.Uint16(bytes[index:]))
	}
	return string(utf16.Decode(wide))
}

// blobOrNull is Rust's ValueRefExt::blob_or_null on a column value scanned
// into an any: false for SQL NULL, the bytes for a BLOB, and rusqlite's
// invalid-type error for any other storage class.
func blobOrNull(value any) ([]byte, bool, error) {
	switch value := value.(type) {
	case nil:
		return nil, false, nil
	case []byte:
		return value, true, nil
	case int64:
		return nil, false, fmt.Errorf("Invalid column type Integer")
	case float64:
		return nil, false, fmt.Errorf("Invalid column type Real")
	case string:
		return nil, false, fmt.Errorf("Invalid column type Text")
	default:
		return nil, false, fmt.Errorf("Invalid column type %T", value)
	}
}
