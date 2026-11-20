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
	"errors"
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
	panic("unported: mj-checkpoint/src/native/antigravity.rs relocate")
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
	panic("unported: mj-checkpoint/src/native/antigravity.rs workspace_uri")
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
	panic("unported: mj-checkpoint/src/native/antigravity.rs fields")
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
	panic("unported: mj-checkpoint/src/native/antigravity.rs rewrite_workspace_roots")
}
