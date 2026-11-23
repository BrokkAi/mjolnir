// Port of mj-store/src/database/tool_details.rs.

// Append-only storage for full transcript bodies kept outside SQLite.

package store

import (
	"bytes"
	"encoding/binary"
	"errors"
	"fmt"
	"hash/crc32"
	"math"
	"os"
	"path/filepath"
	"sync"
	"unicode/utf8"

	"github.com/klauspost/compress/zstd"

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
	path, err := detailPath(dataDir, sessionID)
	if err != nil {
		return nil, err
	}
	if len(bodies) == 0 {
		return []ToolDetailFrame{}, nil
	}
	directory := filepath.Dir(path)
	if err := core.CreateOwnerOnlyDirectory(directory); err != nil {
		return nil, err
	}
	created := false
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_APPEND, 0o600)
	if errors.Is(err, os.ErrNotExist) {
		file, err = os.OpenFile(path, os.O_WRONLY|os.O_APPEND|os.O_CREATE|os.O_EXCL, 0o600)
		if err != nil {
			return nil, fmt.Errorf("create tool details %s: %w", path, err)
		}
		created = true
	} else if err != nil {
		return nil, fmt.Errorf("open tool details %s: %w", path, err)
	}
	defer file.Close()
	if created {
		if err := core.SyncDirectory(directory); err != nil {
			return nil, err
		}
		if err := core.SyncDirectory(dataDir); err != nil {
			return nil, err
		}
	}
	info, err := file.Stat()
	if err != nil {
		return nil, err
	}
	offset := uint64(info.Size())
	frames := make([]ToolDetailFrame, 0, len(bodies))
	for _, body := range bodies {
		if len(body.StableID) > math.MaxUint16 {
			return nil, errors.New("tool detail stable ID is too long: out of range integral type conversion attempted")
		}
		payload := toolDetailsEncoder().EncodeAll([]byte(body.BodyJSON), nil)
		if uint64(len(payload)) > math.MaxUint32 {
			return nil, errors.New("tool detail payload is too long: out of range integral type conversion attempted")
		}
		length := uint64(toolDetailsHeaderLen + len(body.StableID) + len(payload))
		if length > math.MaxUint32 {
			return nil, errors.New("tool detail frame is too long: out of range integral type conversion attempted")
		}
		frame := make([]byte, 0, length)
		frame = append(frame, toolDetailsMagic[:]...)
		frame = append(frame, toolDetailsVersion)
		frame = binary.BigEndian.AppendUint16(frame, uint16(len(body.StableID)))
		frame = append(frame, body.StableID...)
		frame = binary.BigEndian.AppendUint32(frame, crc32.ChecksumIEEE(payload))
		frame = binary.BigEndian.AppendUint32(frame, uint32(len(payload)))
		frame = append(frame, payload...)
		if _, err := file.Write(frame); err != nil {
			return nil, fmt.Errorf("append tool detail %s to %s: %w", core.RustDebugString(body.StableID), path, err)
		}
		frames = append(frames, ToolDetailFrame{Offset: offset, Len: uint32(length)})
		offset += length
	}
	if err := file.Sync(); err != nil {
		return nil, fmt.Errorf("sync tool details %s: %w", path, err)
	}
	return frames, nil
}

// The shared zstd codec: level 0 asks zstd for its default level, 3.
var (
	toolDetailsEncoder = sync.OnceValue(func() *zstd.Encoder {
		encoder, err := zstd.NewWriter(nil, zstd.WithEncoderLevel(zstd.EncoderLevelFromZstd(3)), zstd.WithEncoderConcurrency(1))
		if err != nil {
			panic(fmt.Sprintf("zstd compressor: %v", err))
		}
		return encoder
	})
	toolDetailsDecoder = sync.OnceValue(func() *zstd.Decoder {
		decoder, err := zstd.NewReader(nil, zstd.WithDecoderConcurrency(0))
		if err != nil {
			panic(fmt.Sprintf("zstd decompressor: %v", err))
		}
		return decoder
	})
)

func ReadToolDetail(dataDir, sessionID, expectedStableID string, offset uint64, length uint32) (core.TranscriptBody, error) {
	path, err := detailPath(dataDir, sessionID)
	if err != nil {
		return nil, err
	}
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("open tool details %s: %w", path, err)
	}
	defer file.Close()
	if int(length) < toolDetailsHeaderLen {
		return nil, errors.New("tool detail frame is shorter than its header")
	}
	info, err := file.Stat()
	if err != nil {
		return nil, fmt.Errorf("stat tool details %s: %w", path, err)
	}
	shortRead := fmt.Sprintf("short read of tool details %s at %d, expected %d bytes", path, offset, length)
	end := offset + uint64(length)
	if end < offset || end > uint64(info.Size()) {
		return nil, errors.New(shortRead)
	}
	frame := make([]byte, length)
	if err := readExactAt(file, frame, offset); err != nil {
		return nil, fmt.Errorf("%s: %w", shortRead, err)
	}
	body, err := decodeFrame(frame, expectedStableID)
	if err != nil {
		return nil, fmt.Errorf("read tool detail in %s at %d: %w", path, offset, err)
	}
	return body, nil
}

// readExactAt fills buf from offset, failing on a short read. Go's ReadAt
// reads fully or reports why on every platform, so this serves Rust's Unix
// read_exact_at and its Windows seek_read loop alike.
func readExactAt(file *os.File, buf []byte, offset uint64) error {
	_, err := file.ReadAt(buf, int64(offset))
	return err
}

func decodeFrame(frame []byte, expectedStableID string) (core.TranscriptBody, error) {
	if !bytes.Equal(frame[:4], toolDetailsMagic[:]) {
		return nil, errors.New("bad tool detail magic")
	}
	if frame[4] != toolDetailsVersion {
		return nil, fmt.Errorf("unsupported tool detail version %d", frame[4])
	}
	idLen := int(binary.BigEndian.Uint16(frame[5:7]))
	if len(frame) < toolDetailsHeaderLen+idLen {
		return nil, errors.New("truncated tool detail header")
	}
	idEnd := 7 + idLen
	if !utf8.Valid(frame[7:idEnd]) {
		return nil, errors.New("tool detail stable ID is not UTF-8")
	}
	id := string(frame[7:idEnd])
	if id != expectedStableID {
		return nil, fmt.Errorf("tool detail stable ID mismatch: expected %s, got %s",
			core.RustDebugString(expectedStableID), core.RustDebugString(id))
	}
	checksum := binary.BigEndian.Uint32(frame[idEnd : idEnd+4])
	payloadLen := binary.BigEndian.Uint32(frame[idEnd+4 : idEnd+8])
	payload := frame[idEnd+8:]
	if len(payload) != int(payloadLen) {
		return nil, errors.New("tool detail payload length mismatch")
	}
	if crc32.ChecksumIEEE(payload) != checksum {
		return nil, errors.New("tool detail CRC32 mismatch")
	}
	body, err := toolDetailsDecoder().DecodeAll(payload, nil)
	if err != nil {
		return nil, fmt.Errorf("decompress tool detail: %w", err)
	}
	decoded, err := core.DecodeTranscriptBody(body)
	if err != nil {
		return nil, fmt.Errorf("decode tool detail transcript body: %w", err)
	}
	return decoded, nil
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
