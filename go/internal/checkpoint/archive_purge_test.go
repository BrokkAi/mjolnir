// Port of the tests in mj-checkpoint/src/archive/purge.rs.

package checkpoint

import (
	"bytes"
	"compress/zlib"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// purgeKnownCommit's `git hash-object -t commit` is purgeKnownCommitSha.
const purgeKnownCommit = "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n" +
	"parent 0000000000000000000000000000000000000001\n" +
	"author T <t@t> 1 +0000\n" +
	"committer T <t@t> 1 +0000\n" +
	"\n" +
	"note\n"

const (
	purgeKnownCommitSha    = "48ca35318d1399e84e52432c1b0d1b0f76444c35"
	purgeKnownCommitSha256 = "ec74e4e8cec8bd9cf3d2592a1f86c17308a4aaf4becf116d5187757c3f39b242"
)

func TestPurgePackReaderMatchesGitObjectIDsAndInPackDeltas(t *testing.T) {
	known := []byte(purgeKnownCommit)
	if got := gitObjectID("commit", known, false); got != purgeKnownCommitSha {
		t.Fatalf("sha1 = %s", got)
	}
	if got := gitObjectID("commit", known, true); got != purgeKnownCommitSha256 {
		t.Fatalf("sha256 = %s", got)
	}

	threeParents := purgeCommitWithParents(3, "holds untracked")
	twoParents := purgeCommitWithParents(2, "tracked only")
	chained := purgeCommitWithParents(3, "chained untracked")
	threeID := gitObjectID("commit", threeParents, false)
	twoID := gitObjectID("commit", twoParents, false)
	chainedID := gitObjectID("commit", chained, false)

	pack := purgePackHeader(6)
	knownAt := len(pack)
	pack = purgePushRaw(t, pack, 1, known)
	threeAt := len(pack)
	pack = purgePushOfsDelta(t, pack, knownAt, known, threeParents)
	pack = purgePushRefDelta(t, pack, purgeDecodeOID(t, purgeKnownCommitSha), known, twoParents)
	blobAt := len(pack)
	pack = purgePushRaw(t, pack, 3, []byte("not-a-commit"))
	pack = purgePushOfsDelta(t, pack, blobAt, []byte("not-a-commit"), []byte("not-a-commit!"))
	pack = purgePushOfsDelta(t, pack, threeAt, threeParents, chained)
	pack = append(pack, make([]byte, 20)...)

	parents, err := commitParentCounts(purgeBundle(pack))
	if err != nil {
		t.Fatal(err)
	}
	for oid, want := range map[string]int{purgeKnownCommitSha: 1, threeID: 3, twoID: 2, chainedID: 3} {
		if got, ok := parents[oid]; !ok || got != want {
			t.Fatalf("parents of %s = %d (%v), want %d", oid, got, ok, want)
		}
	}
	if len(parents) != 4 {
		t.Fatalf("parents = %v", parents)
	}

	manifest := purgeStashManifest([][2]string{
		{strings.ToUpper(threeID), "holds untracked"},
		{twoID, "tracked only"},
		{strings.Repeat("ab", 20), "missing"},
	})
	notes, warnings, err := inspectBundleStashes(purgeBundle(pack), &manifest)
	if err != nil {
		t.Fatal(err)
	}
	if len(notes) != 1 {
		t.Fatalf("notes = %+v", notes)
	}
	if notes[0].RepositoryID != "repo" || notes[0].Commit != strings.ToUpper(threeID) || notes[0].Message != "holds untracked" {
		t.Fatalf("note = %+v", notes[0])
	}
	if len(warnings) != 1 || !strings.Contains(warnings[0], strings.Repeat("ab", 20)) {
		t.Fatalf("warnings = %q", warnings)
	}
}

func TestPurgePackReaderSkipsADeltaWhoseBaseIsOutsideThePack(t *testing.T) {
	threeParents := purgeCommitWithParents(3, "holds untracked")
	pack := purgePackHeader(1)
	pack = purgePushRefDelta(t, pack, bytes.Repeat([]byte{0xab}, 20), []byte(purgeKnownCommit), threeParents)
	pack = append(pack, make([]byte, 20)...)
	parents, err := commitParentCounts(purgeBundle(pack))
	if err != nil {
		t.Fatal(err)
	}
	if len(parents) != 0 {
		t.Fatalf("parents = %v", parents)
	}
}

func TestPurgePackReaderDiscardsALargeBlobInBoundedChunks(t *testing.T) {
	blob := bytes.Repeat([]byte{'x'}, 200*1024)
	pack := purgePackHeader(2)
	pack = purgePushRaw(t, pack, 1, []byte(purgeKnownCommit))
	pack = purgePushRaw(t, pack, 3, blob)
	pack = append(pack, make([]byte, 20)...)
	resetBundleDiscardStats()
	parents, err := commitParentCounts(purgeBundle(pack))
	if err != nil {
		t.Fatal(err)
	}
	if got, ok := parents[purgeKnownCommitSha]; !ok || got != 1 {
		t.Fatalf("parents = %v", parents)
	}
	peak, total := bundleDiscardStats()
	if peak == 0 || peak > 8*1024 {
		t.Fatalf("discard peak %d", peak)
	}
	if total < uint64(len(blob)) {
		t.Fatalf("discarded %d bytes of a %d byte blob", total, len(blob))
	}
}

func TestPurgePackReaderRejectsATruncatedBundle(t *testing.T) {
	_, err := commitParentCounts([]byte("# v2 git bundle\n\nPACK"))
	if err == nil {
		t.Fatal("a truncated bundle was read")
	}
	if message := err.Error(); !strings.Contains(message, "truncated") && !strings.Contains(message, "pack") {
		t.Fatal(message)
	}
}

func purgeStashManifest(entries [][2]string) core.RepositoryManifest {
	stash := make([]core.SavedStashEntry, 0, len(entries))
	for _, entry := range entries {
		stash = append(stash, core.SavedStashEntry{Commit: entry[0], Message: entry[1]})
	}
	return core.RepositoryManifest{
		Metadata: core.RepositoryMetadata{
			ID:                  "repo",
			RelativeDestination: "repo",
			Origin:              "https://example.test/repo.git",
			BaseCommit:          strings.Repeat("a", 40),
			HeadCommit:          strings.Repeat("b", 40),
			Branch:              archiveRef("main"),
			SavedRefs:           map[string]string{},
			StashStack:          stash,
		},
		CommittedBundlePath: "bundle",
		UntrackedTarPath:    "untracked.tar",
	}
}

func purgeCommitWithParents(parents int, message string) []byte {
	body := []byte("tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n")
	for index := range parents {
		body = fmt.Appendf(body, "parent %040x\n", index+1)
	}
	return fmt.Appendf(body, "author T <t@t> 1 +0000\ncommitter T <t@t> 1 +0000\n\n%s\n", message)
}

func purgeBundle(pack []byte) []byte {
	return append([]byte("# v2 git bundle\n\n"), pack...)
}

func purgePackHeader(count uint32) []byte {
	pack := binary.BigEndian.AppendUint32([]byte("PACK"), 2)
	return binary.BigEndian.AppendUint32(pack, count)
}

func purgePushRaw(t testing.TB, pack []byte, kind byte, body []byte) []byte {
	pack = purgePushHeader(pack, kind, len(body))
	return append(pack, purgeZlib(t, body)...)
}

func purgePushOfsDelta(t testing.TB, pack []byte, baseAt int, base, target []byte) []byte {
	headerAt := len(pack)
	delta := purgeEncodeDelta(base, target)
	pack = purgePushHeader(pack, 6, len(delta))
	pack = purgePushOfs(pack, headerAt-baseAt)
	return append(pack, purgeZlib(t, delta)...)
}

func purgePushRefDelta(t testing.TB, pack []byte, baseOID []byte, base, target []byte) []byte {
	delta := purgeEncodeDelta(base, target)
	pack = purgePushHeader(pack, 7, len(delta))
	pack = append(pack, baseOID...)
	return append(pack, purgeZlib(t, delta)...)
}

func purgePushHeader(pack []byte, kind byte, size int) []byte {
	first := kind<<4 | byte(size&0x0f)
	size >>= 4
	if size > 0 {
		first |= 0x80
	}
	pack = append(pack, first)
	for size > 0 {
		b := byte(size & 0x7f)
		size >>= 7
		if size > 0 {
			b |= 0x80
		}
		pack = append(pack, b)
	}
	return pack
}

func purgePushOfs(pack []byte, offset int) []byte {
	encoded := []byte{byte(offset & 0x7f)}
	offset >>= 7
	for offset > 0 {
		offset--
		encoded = append(encoded, 0x80|byte(offset&0x7f))
		offset >>= 7
	}
	for left, right := 0, len(encoded)-1; left < right; left, right = left+1, right-1 {
		encoded[left], encoded[right] = encoded[right], encoded[left]
	}
	return append(pack, encoded...)
}

func purgeEncodeDelta(base, target []byte) []byte {
	var delta []byte
	delta = purgePushSize(delta, len(base))
	delta = purgePushSize(delta, len(target))
	prefix := 0
	for prefix < len(base) && prefix < len(target) && base[prefix] == target[prefix] {
		prefix++
	}
	if prefix > 0 {
		delta = purgePushCopy(delta, 0, prefix)
	}
	rest := target[prefix:]
	for position := 0; position < len(rest); {
		length := min(len(rest)-position, 127)
		delta = append(delta, byte(length))
		delta = append(delta, rest[position:position+length]...)
		position += length
	}
	return delta
}

func purgePushSize(out []byte, size int) []byte {
	for {
		b := byte(size & 0x7f)
		size >>= 7
		if size > 0 {
			b |= 0x80
		}
		out = append(out, b)
		if size == 0 {
			return out
		}
	}
}

func purgePushCopy(out []byte, offset, size int) []byte {
	command := byte(0x80)
	var operands []byte
	for bit := range 4 {
		if b := (offset >> (8 * bit)) & 0xff; b != 0 {
			command |= 1 << bit
			operands = append(operands, byte(b))
		}
	}
	for bit := range 3 {
		if b := (size >> (8 * bit)) & 0xff; b != 0 {
			command |= 1 << (4 + bit)
			operands = append(operands, byte(b))
		}
	}
	out = append(out, command)
	return append(out, operands...)
}

func purgeZlib(t testing.TB, body []byte) []byte {
	t.Helper()
	var compressed bytes.Buffer
	encoder := zlib.NewWriter(&compressed)
	if _, err := encoder.Write(body); err != nil {
		t.Fatal(err)
	}
	if err := encoder.Close(); err != nil {
		t.Fatal(err)
	}
	return compressed.Bytes()
}

func purgeDecodeOID(t testing.TB, text string) []byte {
	t.Helper()
	oid, err := hex.DecodeString(text)
	if err != nil || len(oid) != 20 {
		t.Fatalf("bad object id %q", text)
	}
	return oid
}
