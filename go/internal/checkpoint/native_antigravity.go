// Port of mj-checkpoint/src/native/antigravity.rs.
//
// Workspace relocation for Antigravity (`agy`) conversation databases.
//
// agy keeps one SQLite database per conversation. The row `main` of its
// `trajectory_metadata_blob` table is a protobuf message, written when the
// conversation is created and never rewritten by a later load, that names
// every workspace root the conversation was given: the launch directory and
// each additional directory, in alphabetical order with nothing marking the
// launch directory. Each root appears twice, as the URI inside one repeated
// message (field 1, whose field 1 is the URI) and as one repeated string of
// its own (field 7). agy runs commands in the alphabetically first of these
// roots and the roots of the request that opens the conversation, so a
// recorded root that is not where the conversation now runs, whether it is
// gone or is another copy of the tree, points agy's commands at the wrong
// directory. Relocation records the new launch directory as the only root
// and copies every other byte of the record as it was. Every request that
// opens the conversation names its additional directories again, so agy's
// roots are then the new launch directory and the request's.
//
// `relocate` exists in native/antigravity and native/muse; this module, the
// first, keeps the bare name Relocate and Muse's is MuseRelocate.

package checkpoint

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"maps"
	"math"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"unicode/utf8"

	// The SQLite driver (Rust's rusqlite), registered as "sqlite".
	_ "modernc.org/sqlite"
)

// rootMessageField is the record's repeated field of root messages, and
// rootMessageURIField the field of such a message that holds the URI.
const (
	rootMessageField    uint64 = 1
	rootMessageURIField uint64 = 1
)

// rootField is the record's repeated field of root URIs.
const rootField uint64 = 7

// wireLengthDelimited is protobuf's length-delimited wire type (Rust's
// LENGTH_DELIMITED, renamed beside the lengthDelimited function).
const wireLengthDelimited uint8 = 2

// Relocate returns the conversation database with its launch root moved to
// targetCwd: the database is copied to a scratch directory, its `main`
// workspace record rewritten (rewriteWorkspaceRoots), the WAL checkpointed
// with TRUNCATE so the update is in the file, and the file read back.
func Relocate(database []byte, targetCwd string) ([]byte, error) {
	uri, err := workspaceURI(targetCwd)
	if err != nil {
		return nil, err
	}
	scratch, err := os.MkdirTemp("", "")
	if err != nil {
		return nil, fmt.Errorf("create Antigravity relocation directory: %w", err)
	}
	defer os.RemoveAll(scratch)
	path := filepath.Join(scratch, "conversation.db")
	if err := os.WriteFile(path, database, 0o666); err != nil {
		return nil, fmt.Errorf("write Antigravity conversation database: %w", err)
	}
	if err := relocateDatabaseFile(path, uri); err != nil {
		return nil, err
	}
	relocated, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read the relocated Antigravity conversation database: %w", err)
	}
	return relocated, nil
}

// relocateDatabaseFile rewrites the workspace record of the conversation
// database at path in place, with its update checkpointed into the file. The
// connection is closed before it returns, as Rust drops it before reading the
// file back.
func relocateDatabaseFile(path, uri string) error {
	db, err := sql.Open("sqlite", antigravityDSN(path))
	if err != nil {
		return fmt.Errorf("open the checkpointed Antigravity conversation database: %w", err)
	}
	defer db.Close()
	ctx := context.Background()
	// One connection, as Rust holds one: the checkpoint must see the update.
	connection, err := db.Conn(ctx)
	if err != nil {
		return fmt.Errorf("open the checkpointed Antigravity conversation database: %w", err)
	}
	defer connection.Close()
	var data any
	err = connection.QueryRowContext(ctx, "SELECT data FROM trajectory_metadata_blob WHERE id = 'main'").Scan(&data)
	if err != nil {
		return fmt.Errorf("read the Antigravity conversation's workspace record: %w", err)
	}
	blob, ok := data.([]byte)
	if !ok {
		// rusqlite reads a Vec<u8> only from a BLOB.
		return fmt.Errorf("read the Antigravity conversation's workspace record: Invalid column type %s at index: 0, name: data", sqliteValueType(data))
	}
	blob, err = rewriteWorkspaceRoots(blob, uri)
	if err != nil {
		return err
	}
	result, err := connection.ExecContext(ctx, "UPDATE trajectory_metadata_blob SET data = ?1 WHERE id = 'main'", blob)
	if err != nil {
		return fmt.Errorf("write the Antigravity conversation's workspace record: %w", err)
	}
	changed, err := result.RowsAffected()
	if err != nil {
		return fmt.Errorf("write the Antigravity conversation's workspace record: %w", err)
	}
	if changed != 1 {
		return errors.New("the Antigravity conversation has no single workspace record")
	}
	// Move the update out of the log, which is not carried with the file.
	if _, err := connection.ExecContext(ctx, "PRAGMA wal_checkpoint(TRUNCATE);"); err != nil {
		return err
	}
	if err := connection.Close(); err != nil {
		return err
	}
	return db.Close()
}

// sqliteValueType names a column value's SQLite type as rusqlite's
// InvalidColumnType error does.
func sqliteValueType(value any) string {
	switch value.(type) {
	case nil:
		return "Null"
	case int64:
		return "Integer"
	case float64:
		return "Real"
	case string:
		return "Text"
	}
	return "Blob"
}

// antigravityDSN is the SQLite URI of the database file at path, so no
// character of the path reads as a URI query or fragment.
func antigravityDSN(path string) string {
	slashed := filepath.ToSlash(path)
	if !strings.HasPrefix(slashed, "/") {
		slashed = "/" + slashed
	}
	return "file://" + (&url.URL{Path: slashed}).EscapedPath()
}

// workspaceURI is the `file://` URI agy writes for a workspace directory. agy
// escapes as Go's `url.URL` does for a path: every byte but ASCII letters and
// digits, `-_.~` and `$&+,/:;=@` becomes `%XX`. Observed with agy 1.5.8: a
// session started in a directory named
// `a'b(c)[d]!e*f~g$h&i+j,k;l=m@n:o{p}q^r|s<t>u"v`w?x y日本` (plus the `%` and
// `#` of an earlier probe) had `trajectory_metadata_blob` record `%27 %28 %29
// %5B %5D %21 %2A %7B %7D %5E %7C %3C %3E %22 %60 %3F %20
// %E6%97%A5%E6%9C%AC %25 %23` for those characters and left `~ $ & + , ; = @
// :` as they were. The `url` crate leaves `'()[]!*^|` unescaped, so the URI
// it builds differs from agy's for such a directory, and agy would see the
// recorded root and the request's own directory as two roots.
func workspaceURI(directory string) (string, error) {
	if !filepath.IsAbs(directory) {
		return "", fmt.Errorf("Antigravity workspace %s is not an absolute path", directory)
	}
	if !utf8.ValidString(directory) {
		return "", fmt.Errorf("Antigravity workspace %s is not UTF-8", strings.ToValidUTF8(directory, "�"))
	}
	var uri strings.Builder
	uri.WriteString("file://")
	for i := 0; i < len(directory); i++ {
		b := directory[i]
		switch {
		case 'a' <= b && b <= 'z', 'A' <= b && b <= 'Z', '0' <= b && b <= '9',
			strings.IndexByte("-_.~$&+,/:;=@", b) >= 0:
			uri.WriteByte(b)
		default:
			fmt.Fprintf(&uri, "%%%02X", b)
		}
	}
	return uri.String(), nil
}

// field is one field of a protobuf message: where it sits, and its value.
// Its byte slices alias the parsed message.
type field struct {
	number   uint64
	wireType uint8
	// encoded is the field exactly as it was encoded, tag included.
	encoded []byte
	// tag is the tag as it was encoded.
	tag []byte
	// payload is a length-delimited field's bytes; empty for any other wire
	// type.
	payload []byte
}

// readVarint reads the varint at *at, advancing it.
func readVarint(bytes []byte, at *int) (uint64, error) {
	var value uint64
	for shift := 0; shift < 70; shift += 7 {
		if *at >= len(bytes) {
			return 0, errors.New("truncated protobuf varint")
		}
		b := bytes[*at]
		*at++
		value |= uint64(b&0x7f) << shift
		if b&0x80 == 0 {
			return value, nil
		}
	}
	return 0, errors.New("protobuf varint is too long")
}

// writeVarint appends value to output as a varint.
func writeVarint(value uint64, output *[]byte) {
	for {
		b := byte(value & 0x7f)
		value >>= 7
		if value == 0 {
			*output = append(*output, b)
			return
		}
		*output = append(*output, b|0x80)
	}
}

// fields splits a message into its fields. Groups are refused: a field this
// editor cannot measure cannot be copied byte for byte.
func fields(message []byte) ([]field, error) {
	parsed := []field{}
	at := 0
	for at < len(message) {
		start := at
		tag, err := readVarint(message, &at)
		if err != nil {
			return nil, err
		}
		tagEnd := at
		wireType := uint8(tag & 7)
		payload := []byte{}
		switch wireType {
		case 0:
			if _, err := readVarint(message, &at); err != nil {
				return nil, err
			}
		case 1:
			at += 8
		case wireLengthDelimited:
			length, err := readVarint(message, &at)
			if err != nil {
				return nil, err
			}
			if length > math.MaxInt {
				return nil, errors.New("protobuf field is too long")
			}
			if int(length) > math.MaxInt-at {
				return nil, errors.New("protobuf field overflows")
			}
			end := at + int(length)
			if end > len(message) {
				return nil, errors.New("protobuf field runs past its message")
			}
			payload = message[at:end]
			at = end
		case 5:
			at += 4
		default:
			return nil, fmt.Errorf("protobuf wire type %d is not supported", wireType)
		}
		if at > len(message) {
			return nil, errors.New("protobuf field runs past its message")
		}
		parsed = append(parsed, field{
			number:   tag >> 3,
			wireType: wireType,
			encoded:  message[start:at],
			tag:      message[start:tagEnd],
			payload:  payload,
		})
	}
	return parsed, nil
}

// requireString refuses a field that is not length-delimited.
func requireString(f field, what string) error {
	if f.wireType != wireLengthDelimited {
		return errors.New("the Antigravity workspace record's " + what + " is not a string")
	}
	return nil
}

// lengthDelimited appends a length-delimited field with tag and payload.
func lengthDelimited(tag, payload []byte, output *[]byte) {
	*output = append(*output, tag...)
	writeVarint(uint64(len(payload)), output)
	*output = append(*output, payload...)
}

// rewriteWorkspaceRoots is the workspace record with newURI as its only
// root.
//
// Nothing in the record marks the launch root, and the checkpoint does not
// record where it was, so no root is kept: a root that exists where the
// conversation is restored may be the old tree, and agy would run commands
// in it. The first root message and the first root URI keep their places
// and every other field of the record is copied as it was. A record that is
// not of the shape agy writes (every root a file URI, the two lists naming
// the same roots) is not one this relocation understands, and the restore
// fails instead of leaving the conversation in a directory nobody chose.
func rewriteWorkspaceRoots(blob []byte, newURI string) ([]byte, error) {
	top, err := fields(blob)
	if err != nil {
		return nil, fmt.Errorf("parse the Antigravity workspace record: %w", err)
	}
	var messages, strs []int
	for index, f := range top {
		switch f.number {
		case rootMessageField:
			messages = append(messages, index)
		case rootField:
			strs = append(strs, index)
		}
	}
	if len(messages) == 0 || len(strs) == 0 {
		return nil, fmt.Errorf("the Antigravity workspace record holds %d root messages and %d root URIs", len(messages), len(strs))
	}

	fileURI := func(bytes []byte, what string) (string, error) {
		if !utf8.Valid(bytes) {
			return "", fmt.Errorf("the Antigravity workspace record's %s is not UTF-8", what)
		}
		uri := string(bytes)
		if !strings.HasPrefix(uri, "file://") {
			return "", fmt.Errorf("the Antigravity workspace record's %s %s is not a file URI", what, nativeDebugString(uri))
		}
		return uri, nil
	}
	type messagePart struct {
		index       int
		nested      []field
		nestedIndex int
	}
	nestedRoots := map[string]struct{}{}
	messageParts := make([]messagePart, 0, len(messages))
	for _, index := range messages {
		message := top[index]
		if err := requireString(message, "root message"); err != nil {
			return nil, err
		}
		nested, err := fields(message.payload)
		if err != nil {
			return nil, fmt.Errorf("parse an Antigravity root message: %w", err)
		}
		nestedIndex := -1
		for candidate, f := range nested {
			if f.number != rootMessageURIField {
				continue
			}
			if nestedIndex >= 0 {
				return nil, errors.New("an Antigravity root message has more than one URI")
			}
			nestedIndex = candidate
		}
		if nestedIndex < 0 {
			return nil, errors.New("an Antigravity root message has no URI")
		}
		uri := nested[nestedIndex]
		if err := requireString(uri, "root message URI"); err != nil {
			return nil, err
		}
		root, err := fileURI(uri.payload, "root message URI")
		if err != nil {
			return nil, err
		}
		nestedRoots[root] = struct{}{}
		messageParts = append(messageParts, messagePart{index, nested, nestedIndex})
	}
	roots := map[string]struct{}{}
	for _, index := range strs {
		if err := requireString(top[index], "root URI"); err != nil {
			return nil, err
		}
		root, err := fileURI(top[index].payload, "root URI")
		if err != nil {
			return nil, err
		}
		roots[root] = struct{}{}
	}
	if !maps.Equal(nestedRoots, roots) {
		return nil, fmt.Errorf("the Antigravity workspace record names two different sets of workspaces (%s and %s)",
			debugStringSet(nestedRoots), debugStringSet(roots))
	}

	first := messageParts[0]
	rootMessage := []byte{}
	for index, f := range first.nested {
		if index == first.nestedIndex {
			lengthDelimited(f.tag, []byte(newURI), &rootMessage)
		} else {
			rootMessage = append(rootMessage, f.encoded...)
		}
	}
	firstString := strs[0]
	output := make([]byte, 0, len(blob)+len(newURI))
	for index, f := range top {
		switch {
		case index == first.index:
			lengthDelimited(f.tag, rootMessage, &output)
		case index == firstString:
			lengthDelimited(f.tag, []byte(newURI), &output)
		case f.number != rootMessageField && f.number != rootField:
			output = append(output, f.encoded...)
		}
	}
	return output, nil
}

// debugStringSet renders a set of strings as Rust's `{:?}` does for a
// BTreeSet<String>: sorted, braced and comma-separated.
func debugStringSet(set map[string]struct{}) string {
	members := slices.Sorted(maps.Keys(set))
	for index, member := range members {
		members[index] = nativeDebugString(member)
	}
	return "{" + strings.Join(members, ", ") + "}"
}
