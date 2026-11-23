// Port of mj-store/src/database/tool_details.rs.

// Append-only storage for full transcript bodies kept outside SQLite.

package store

import (
	"fmt"
	"os"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// toolDetailsMagic, toolDetailsVersion and toolDetailsHeaderLen describe a
// frame: magic, version, stable ID length (u16), stable ID, CRC32 (u32),
// payload length (u32), zstd payload, all big-endian.
var toolDetailsMagic = [4]byte{'M', 'J', 'T', 'D'}

const toolDetailsVersion byte = 1

const toolDetailsHeaderLen = 15

// dataDirFrom is the data directory holding dbPath: its parent, or "." for a
// bare file name.
func dataDirFrom(dbPath string) string {
	parent := filepath.Dir(dbPath)
	if parent == "" || parent == dbPath {
		return "."
	}
	return parent
}

func detailPath(dataDir, sessionID string) (string, error) {
	if err := core.ValidateID("session", sessionID); err != nil {
		return "", err
	}
	return filepath.Join(dataDir, "tool-details", sessionID+".mjtd"), nil
}

// ToolDetailFrame is where a frame landed: its byte offset and length (Rust's
// (u64, u32)).
type ToolDetailFrame struct {
	Offset uint64
	Len    uint32
}

// AppendToolDetail appends one frame. Callers must serialize appends and
// deletes for a session.
func AppendToolDetail(dataDir, sessionID, stableID, bodyJSON string) (ToolDetailFrame, error) {
	frames, err := AppendToolDetailsBatch(dataDir, sessionID, []ToolDetailBody{{StableID: stableID, BodyJSON: bodyJSON}})
	if err != nil {
		return ToolDetailFrame{}, err
	}
	return frames[0], nil
}

// ToolDetailBody is one body to append (Rust's (&str, &str)).
type ToolDetailBody struct {
	StableID string
	BodyJSON string
}

// AppendToolDetailsBatch persists a batch before publishing any of its
// pointers in SQLite. The database writer serializes changes to each session
// file.
func AppendToolDetailsBatch(dataDir, sessionID string, bodies []ToolDetailBody) ([]ToolDetailFrame, error) {
	panic("unported: mj-store/src/database/tool_details.rs append_tool_details_batch")
}

func ReadToolDetail(dataDir, sessionID, expectedStableID string, offset uint64, length uint32) (core.TranscriptBody, error) {
	panic("unported: mj-store/src/database/tool_details.rs read_tool_detail")
}

// readExactAt fills buf from offset, failing on a short read. Go's ReadAt
// reads fully or reports why on every platform, so this serves Rust's Unix
// read_exact_at and its Windows seek_read loop alike.
func readExactAt(file *os.File, buf []byte, offset uint64) error {
	_, err := file.ReadAt(buf, int64(offset))
	return err
}

func decodeFrame(frame []byte, expectedStableID string) (core.TranscriptBody, error) {
	panic("unported: mj-store/src/database/tool_details.rs decode_frame")
}

func RemoveSessionToolDetails(dataDir, sessionID string) error {
	path, err := detailPath(dataDir, sessionID)
	if err != nil {
		return err
	}
	if err := os.Remove(path); err != nil {
		if os.IsNotExist(err) {
			return nil
		}
		return fmt.Errorf("remove tool details %s: %w", path, err)
	}
	return core.SyncDirectory(filepath.Dir(path))
}
