// Port of the tests in mj-store/src/database/tool_details.rs.

package store

import (
	"errors"
	"io/fs"
	"os"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestPositionalFramesRejectWrongIDsCorruptionAndTruncation(t *testing.T) {
	data := t.TempDir()
	seed := uint32(0x1234_abcd)
	text := make([]byte, 0, 192*1024)
	for range 192 * 1024 {
		seed ^= seed << 13
		seed ^= seed >> 17
		seed ^= seed << 5
		text = append(text, '!'+byte(seed%90))
	}
	encoded, err := databaseMarshalJSON(core.TranscriptBodySystem{Text: string(text)})
	if err != nil {
		t.Fatal(err)
	}
	body := string(encoded)
	first, err := AppendToolDetail(data, "session", "first", body)
	if err != nil {
		t.Fatal(err)
	}
	second, err := AppendToolDetail(data, "session", "second", body)
	if err != nil {
		t.Fatal(err)
	}
	offset, length := second.Offset, second.Len
	if offset != uint64(first.Len) {
		t.Fatalf("second frame at %d, first frame is %d bytes", offset, first.Len)
	}
	if length <= 64*1024 {
		t.Fatalf("frame is only %d bytes", length)
	}
	read := func(id string) (core.TranscriptBody, error) {
		return ReadToolDetail(data, "session", id, offset, length)
	}
	decoded, err := read("second")
	if err != nil {
		t.Fatal(err)
	}
	reencoded, err := databaseMarshalJSON(decoded)
	if err != nil {
		t.Fatal(err)
	}
	if string(reencoded) != body {
		t.Fatal("the read body differs from the appended one")
	}
	if _, err := read("first"); err == nil || !strings.Contains(err.Error(), "stable ID mismatch") {
		t.Fatalf("reading under the wrong ID: %v", err)
	}

	path, err := detailPath(data, "session")
	if err != nil {
		t.Fatal(err)
	}
	original, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	for _, corruption := range []struct {
		index    int
		expected string
	}{{0, "magic"}, {4, "version"}, {toolDetailsHeaderLen + 6, "CRC32"}} {
		corrupt := append([]byte(nil), original...)
		corrupt[int(offset)+corruption.index] ^= 1
		if err := os.WriteFile(path, corrupt, 0o600); err != nil {
			t.Fatal(err)
		}
		_, err := read("second")
		if err == nil || !strings.Contains(err.Error(), corruption.expected) {
			t.Fatalf("%v", err)
		}
	}
	if err := os.WriteFile(path, original[:len(original)-1], 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := read("second"); err == nil || !strings.Contains(err.Error(), "short read") {
		t.Fatalf("truncated file: %v", err)
	}
	if err := RemoveSessionToolDetails(data, "session"); err != nil {
		t.Fatal(err)
	}
	_, missing := read("second")
	if !errors.Is(missing, fs.ErrNotExist) {
		t.Fatalf("missing file: %v", missing)
	}
	if _, err := AppendToolDetail(data, "../session", "first", body); err == nil {
		t.Fatal("an invalid session ID must be refused")
	}
}
