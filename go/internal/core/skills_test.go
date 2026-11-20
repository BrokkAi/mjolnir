// Port of the tests in mj-core/src/skills/mod.rs.

package core

import (
	"bytes"
	"compress/gzip"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"testing"
)

func writeSkill(t *testing.T, home, relative string, data []byte) {
	t.Helper()
	path := filepath.Join(home, filepath.FromSlash(relative))
	if err := os.MkdirAll(filepath.Dir(path), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, data, 0o666); err != nil {
		t.Fatal(err)
	}
}

type skillFile struct {
	path string
	data []byte
}

func skillsArchiveOf(files []skillFile) SkillsArchive {
	entries := make([]SkillsEntry, 0, len(files))
	for _, file := range files {
		entries = append(entries, SkillsEntry{Path: file.path, Bytes: slices.Clone(file.data)})
	}
	sortSkillsEntries(entries)
	return SkillsArchive{entries: entries}
}

// skillsMust is Rust's unwrap in these tests.
func skillsMust[T any](value T, err error) T {
	if err != nil {
		panic(err)
	}
	return value
}

func skillsEntriesEqual(left, right []SkillsEntry) bool {
	return slices.EqualFunc(left, right, func(a, b SkillsEntry) bool {
		return a.Path == b.Path && bytes.Equal(a.Bytes, b.Bytes)
	})
}

func skillsArchivesEqual(left, right SkillsArchive) bool {
	return skillsEntriesEqual(left.entries, right.entries)
}

func TestAHomeWithoutSkillsCollectsAnEmptyArchive(t *testing.T) {
	archive := skillsMust(CollectSkills(HarnessKindClaude, t.TempDir()))
	if !archive.IsEmpty() {
		t.Fatal("archive is not empty")
	}
	if archive.State().Present {
		t.Fatal("empty archive is present")
	}
}

func TestCollectionIsSortedDeterministicAndSkipsSymlinks(t *testing.T) {
	home := t.TempDir()
	writeSkill(t, home, "skills/review/SKILL.md", []byte("review"))
	writeSkill(t, home, "skills/review/checklist.md", []byte("check"))
	writeSkill(t, home, "skills/audit/SKILL.md", []byte("audit"))
	writeSkill(t, home, "skills/.DS_Store", []byte("junk but kept: it is a real file"))
	if runtime.GOOS != "windows" {
		if err := os.Symlink(filepath.Join(home, "skills/review"), filepath.Join(home, "skills/linked")); err != nil {
			t.Fatal(err)
		}
	}

	first := skillsMust(CollectSkills(HarnessKindCodex, home))
	second := skillsMust(CollectSkills(HarnessKindCodex, home))
	if !skillsArchivesEqual(first, second) {
		t.Fatal("collection is not deterministic")
	}
	want := []string{
		"skills/.DS_Store",
		"skills/audit/SKILL.md",
		"skills/review/SKILL.md",
		"skills/review/checklist.md",
	}
	if got := skillsPaths(&first); !reflect.DeepEqual(got, want) {
		t.Fatalf("paths = %v", got)
	}
	if !first.State().Present {
		t.Fatal("archive is not present")
	}
	if first.Fingerprint() != second.Fingerprint() {
		t.Fatal("fingerprints differ")
	}
}

func TestSessionSkillsOfAnEmptyHomeIsTheManagedSet(t *testing.T) {
	home := t.TempDir()
	archive := skillsMust(SessionSkills(HarnessKindClaude, home, SkillsArchiveFormatGzip, SkillsScopeLocalhost))
	if !skillsEntriesEqual(archive.Entries(), ManagedSkills(HarnessKindClaude)) {
		t.Fatalf("entries = %v", skillsPaths(&archive))
	}
	if !archive.State().Present {
		t.Fatal("archive is not present")
	}
	// Collection is unchanged: it still reports the user tree alone.
	collected := skillsMust(CollectSkills(HarnessKindClaude, home))
	if !collected.IsEmpty() {
		t.Fatal("collection includes the managed skill")
	}
}

func TestUserRecallAndProvenanceSkillsRemainUserOwned(t *testing.T) {
	home := t.TempDir()
	writeSkill(t, home, "skills/recall/SKILL.md", []byte("user recall"))
	writeSkill(t, home, "skills/provenance/SKILL.md", []byte("user provenance"))
	archive := skillsMust(SessionSkills(HarnessKindCodex, home, SkillsArchiveFormatGzip, SkillsScopeLocalhost))
	for _, name := range []string{"recall", "provenance"} {
		index := slices.IndexFunc(archive.Entries(), func(entry SkillsEntry) bool {
			return entry.Path == fmt.Sprintf("skills/%s/SKILL.md", name)
		})
		if index < 0 {
			t.Fatalf("%s is missing", name)
		}
		if got := string(archive.Entries()[index].Bytes); got != "user "+name {
			t.Fatalf("%s = %q", name, got)
		}
	}
}

var skillsFormats = []SkillsArchiveFormat{SkillsArchiveFormatPlain, SkillsArchiveFormatGzip}

// skillsCarried is body (an archive after its magic) as each format carries
// it, written without the encoder so a hostile body can be tried.
func skillsCarried(body []byte) [2][]byte {
	plain := append([]byte(plainMagic), body...)
	var compressed bytes.Buffer
	compressed.WriteString(gzipMagic)
	encoder := gzip.NewWriter(&compressed)
	if _, err := encoder.Write(body); err != nil {
		panic(err)
	}
	if err := encoder.Close(); err != nil {
		panic(err)
	}
	return [2][]byte{plain, compressed.Bytes()}
}

func TestEncodingRoundtripsAndRejectsTampering(t *testing.T) {
	notes := bytes.Repeat([]byte("notes on the audit "), 1000)
	original := skillsArchiveOf([]skillFile{
		{"skills/review/SKILL.md", []byte("review")},
		{"skills/audit/SKILL.md", []byte("audit")},
		{"skills/audit/notes.md", notes},
	})
	for index, format := range skillsFormats {
		magic := []string{"HELSKIL1", "HELSKIL2"}[index]
		encoded := original.Encode(format)
		if !bytes.HasPrefix(encoded, []byte(magic)) {
			t.Fatalf("%v: magic", format)
		}
		decoded := skillsMust(DecodeSkillsArchive(encoded))
		if !skillsArchivesEqual(original, decoded) {
			t.Fatalf("%v: round trip", format)
		}
		if original.Fingerprint() != decoded.Fingerprint() {
			t.Fatalf("%v: fingerprint", format)
		}

		truncated := slices.Clone(encoded[:len(encoded)-1])
		if _, err := DecodeSkillsArchive(truncated); err == nil {
			t.Fatalf("%v: truncated archive decoded", format)
		}
		trailing := append(slices.Clone(encoded), 0)
		_, err := DecodeSkillsArchive(trailing)
		if err == nil || !strings.Contains(err.Error(), "trailing bytes") {
			t.Fatalf("%v: %v", format, err)
		}
	}
	if len(original.Encode(SkillsArchiveFormatGzip)) >= len(original.Encode(SkillsArchiveFormatPlain)) {
		t.Fatal("gzip is not smaller")
	}
	if _, err := DecodeSkillsArchive([]byte("NOTSKILL")); err == nil {
		t.Fatal("bad magic decoded")
	}
	if _, err := DecodeSkillsArchive([]byte("HELSKIL2 is not gzip")); err == nil {
		t.Fatal("non-gzip body decoded")
	}
}

// A HELSKIL1 archive exactly as releases before HELSKIL2 wrote it. It still
// decodes, and its fingerprint is still the SHA-256 of these bytes, which is
// what those releases compare. Carried as HELSKIL2, the same tree has the
// same fingerprint, so a controller and a worker that use different formats
// still agree that a tree is in sync.
func TestAnArchiveFromAnEarlierReleaseDecodesWithTheSameFingerprint(t *testing.T) {
	fixture := []byte("HELSKIL1\x02\x00\x00\x00" +
		"\x15\x00\x00\x00skills/audit/SKILL.md\x05\x00\x00\x00\x00\x00\x00\x00audit" +
		"\x16\x00\x00\x00skills/review/SKILL.md\x06\x00\x00\x00\x00\x00\x00\x00review")

	decoded := skillsMust(DecodeSkillsArchive(fixture))

	want := skillsArchiveOf([]skillFile{
		{"skills/audit/SKILL.md", []byte("audit")},
		{"skills/review/SKILL.md", []byte("review")},
	})
	if !skillsArchivesEqual(decoded, want) {
		t.Fatalf("decoded = %v", skillsPaths(&decoded))
	}
	sum := sha256.Sum256(fixture)
	earlier := LowerHex(sum[:])
	if decoded.Fingerprint() != earlier {
		t.Fatal("fingerprint changed")
	}
	if !bytes.Equal(decoded.Encode(SkillsArchiveFormatPlain), fixture) {
		t.Fatal("plain encoding changed")
	}
	compressed := decoded.Encode(SkillsArchiveFormatGzip)
	recompressed := skillsMust(DecodeSkillsArchive(compressed))
	if recompressed.Fingerprint() != earlier {
		t.Fatal("gzip fingerprint differs")
	}
}

func skillsBody(paths ...string) []byte {
	body := binary.LittleEndian.AppendUint32(nil, uint32(len(paths)))
	for _, path := range paths {
		body = binary.LittleEndian.AppendUint32(body, uint32(len(path)))
		body = append(body, path...)
		body = binary.LittleEndian.AppendUint64(body, 0)
	}
	return body
}

func TestDecodeRejectsUnsafeAndUnsortedPaths(t *testing.T) {
	for _, path := range []string{
		"../escape",
		"skills/../escape",
		"/absolute",
		"skills\\windows",
		"skills//double",
		"skills/",
		"",
	} {
		for _, encoded := range skillsCarried(skillsBody(path)) {
			if _, err := DecodeSkillsArchive(encoded); err == nil {
				t.Fatalf("path %q must be rejected", path)
			}
		}
	}

	for _, encoded := range skillsCarried(skillsBody("skills/b", "skills/a")) {
		if _, err := DecodeSkillsArchive(encoded); err == nil {
			t.Fatal("unsorted paths decoded")
		}
	}
}

func TestDecodeRejectsOversizedArchives(t *testing.T) {
	oversized := bytes.Repeat([]byte("x"), MaxSkillsArchiveBytes+1)
	if _, err := DecodeSkillsArchive(oversized); err == nil {
		t.Fatal("oversized archive decoded")
	}
}

// skillsIncompressible is length bytes that gzip cannot shrink.
func skillsIncompressible(length int) []byte {
	data := make([]byte, length)
	if _, err := rand.Read(data); err != nil {
		panic(err)
	}
	return data
}

// skillsHTML is length bytes of generated HTML, text that compresses well.
func skillsHTML(length int) []byte {
	page := []byte("<!doctype html>\n<table>\n")
	row := uint64(0)
	for len(page) < length {
		page = fmt.Appendf(page, "<tr><td class=\"month\">%d-%02d</td><td class=\"count\">%d.%d</td></tr>\n",
			1749+row/12, row%12+1, row*7919%300, row%10)
		row++
	}
	return page[:length]
}

func skillsPaths(archive *SkillsArchive) []string {
	paths := make([]string, 0, len(archive.Entries()))
	for _, entry := range archive.Entries() {
		paths = append(paths, entry.Path)
	}
	return paths
}

// Launch finding R4-8: one 2.2 MB demo file under a skill made every worker
// fail skills_state once a minute. A file that stays above the limit once
// compressed is skipped, reported, and the rest of the tree still collects.
//
// Hard-won: 17a0618f: An oversized linked demo made every worker skills sync fail once a minute.
func TestAnIncompressibleFileOverTheLimitIsSkippedRatherThanFailingTheTree(t *testing.T) {
	home := t.TempDir()
	writeSkill(t, home, "skills/viz/SKILL.md", []byte("viz"))
	writeSkill(t, home, "skills/viz/demos/large.bin", skillsIncompressible(1_200_000))

	archive := skillsMust(CollectSkills(HarnessKindClaude, home))

	if got := skillsPaths(&archive); !reflect.DeepEqual(got, []string{"skills/viz/SKILL.md"}) {
		t.Fatalf("paths = %v", got)
	}
	if firstReportOfSkippedSkill(filepath.Join(home, "skills/viz/demos/large.bin")) {
		t.Fatal("collection reports the file it leaves out")
	}
	// The next poll collects the same tree, so the fingerprint is stable.
	again := skillsMust(CollectSkills(HarnessKindClaude, home))
	if again.Fingerprint() != archive.Fingerprint() {
		t.Fatal("fingerprint changed")
	}
}

// An HTML page the size of the R4-8 demo file, 2,208,818 bytes, is well
// under the per-file limit once compressed.
func TestALargeFileThatCompressesUnderTheLimitIsCollected(t *testing.T) {
	home := t.TempDir()
	writeSkill(t, home, "skills/viz/SKILL.md", []byte("viz"))
	page := skillsHTML(2_208_818)
	writeSkill(t, home, "skills/viz/demos/sunspot-pretty.html", page)

	archive := skillsMust(CollectSkills(HarnessKindClaude, home))

	want := []string{"skills/viz/SKILL.md", "skills/viz/demos/sunspot-pretty.html"}
	if got := skillsPaths(&archive); !reflect.DeepEqual(got, want) {
		t.Fatalf("paths = %v", got)
	}
	session := skillsMust(SessionSkills(HarnessKindClaude, home, SkillsArchiveFormatGzip, SkillsScopeLocalhost))
	if !slices.ContainsFunc(session.Entries(), func(entry SkillsEntry) bool { return bytes.Equal(entry.Bytes, page) }) {
		t.Fatal("the page is missing from the session tree")
	}
	wire := session.Encode(SkillsArchiveFormatGzip)
	if len(wire) > MaxSkillsArchiveBytes {
		t.Fatalf("wire is %d bytes", len(wire))
	}
	if !skillsArchivesEqual(skillsMust(DecodeSkillsArchive(wire)), session) {
		t.Fatal("round trip")
	}
}

// The tree limit counts compressed bytes: 6 MB of text fits.
func TestATreeOverTheLimitRawButUnderItCompressedIsCollected(t *testing.T) {
	home := t.TempDir()
	for index := range 6 {
		writeSkill(t, home, fmt.Sprintf("skills/viz/demos/page-%d.html", index), skillsHTML(1_000_000))
	}

	archive := skillsMust(SessionSkills(HarnessKindClaude, home, SkillsArchiveFormatGzip, SkillsScopeLocalhost))

	if len(archive.Encode(SkillsArchiveFormatPlain)) <= 6_000_000 {
		t.Fatal("the raw tree is not over 6 MB")
	}
	wire := archive.Encode(SkillsArchiveFormatGzip)
	if len(wire) > MaxSkillsArchiveBytes {
		t.Fatalf("wire is %d bytes", len(wire))
	}
	if !skillsArchivesEqual(skillsMust(DecodeSkillsArchive(wire)), archive) {
		t.Fatal("round trip")
	}
	collected := skillsMust(CollectSkills(HarnessKindClaude, home))
	if len(collected.Entries()) != 6 {
		t.Fatalf("%d entries", len(collected.Entries()))
	}
}

// A worker from before relay protocol 23 reads only HELSKIL1, whose limits
// count raw bytes. The tree collected for it leaves out a file above 1 MiB,
// and a tree above 4 MiB fails with the reason named.
func TestATreeCollectedForAnUncompressedArchiveKeepsTheRawLimits(t *testing.T) {
	home := t.TempDir()
	writeSkill(t, home, "skills/viz/SKILL.md", []byte("viz"))
	writeSkill(t, home, "skills/viz/demos/sunspot-pretty.html", skillsHTML(2_208_818))
	hasPage := func(archive *SkillsArchive) bool {
		return slices.Contains(skillsPaths(archive), "skills/viz/demos/sunspot-pretty.html")
	}

	plain := skillsMust(SessionSkills(HarnessKindClaude, home, SkillsArchiveFormatPlain, SkillsScopeLocalhost))
	compressed := skillsMust(SessionSkills(HarnessKindClaude, home, SkillsArchiveFormatGzip, SkillsScopeLocalhost))

	if hasPage(&plain) {
		t.Fatal("the plain tree holds the large page")
	}
	if !hasPage(&compressed) {
		t.Fatal("the compressed tree lacks the page")
	}
	if !skillsArchivesEqual(skillsMust(DecodeSkillsArchive(plain.Encode(SkillsArchiveFormatPlain))), plain) {
		t.Fatal("round trip")
	}

	for index := range 5 {
		writeSkill(t, home, fmt.Sprintf("skills/viz/demos/page-%d.html", index), skillsHTML(1_000_000))
	}
	_, err := SessionSkills(HarnessKindClaude, home, SkillsArchiveFormatPlain, SkillsScopeLocalhost)
	if err == nil || !strings.Contains(err.Error(), "byte limit of an uncompressed skills archive") {
		t.Fatalf("error = %v", err)
	}
	if _, err := SessionSkills(HarnessKindClaude, home, SkillsArchiveFormatGzip, SkillsScopeLocalhost); err != nil {
		t.Fatal(err)
	}
}

// Five files of 1 MiB that do not compress are 5 MiB on the wire, which is
// still too much for one archive. The tree is not trimmed.
func TestAnIncompressibleTreeOverTheLimitStillFails(t *testing.T) {
	home := t.TempDir()
	for index := range 5 {
		writeSkill(t, home, fmt.Sprintf("skills/blobs/blob-%d", index), skillsIncompressible(int(MaxSkillsFileBytes)))
	}

	_, collectError := CollectSkills(HarnessKindClaude, home)
	_, sessionError := SessionSkills(HarnessKindClaude, home, SkillsArchiveFormatGzip, SkillsScopeLocalhost)
	for _, err := range []error{collectError, sessionError} {
		want := fmt.Sprintf("above the %d byte limit", MaxSkillsArchiveBytes)
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Fatalf("error = %v", err)
		}
	}
}

// A compressed archive small enough for the wire can still inflate to far
// more; the reader stops at the uncompressed tree limit.
func TestDecodeRefusesAnArchiveThatInflatesPastTheTreeLimit(t *testing.T) {
	var bomb bytes.Buffer
	bomb.WriteString(gzipMagic)
	encoder, err := gzip.NewWriterLevel(&bomb, gzip.BestSpeed)
	if err != nil {
		t.Fatal(err)
	}
	zeros := make([]byte, 1024*1024)
	for range MaxSkillsTreeBytes/(1024*1024) + 1 {
		if _, err := encoder.Write(zeros); err != nil {
			t.Fatal(err)
		}
	}
	if err := encoder.Close(); err != nil {
		t.Fatal(err)
	}
	if bomb.Len() > MaxSkillsArchiveBytes {
		t.Fatalf("bomb is %d bytes", bomb.Len())
	}

	_, err = DecodeSkillsArchive(bomb.Bytes())

	if err == nil || !strings.Contains(err.Error(), "inflates to more than") {
		t.Fatalf("error = %v", err)
	}
}

// Hard-won: 17a0618f: A skipped oversized file repeated the same failed-sync warning every minute.
func TestASkippedSkillFileIsReportedOnce(t *testing.T) {
	path := "/nonexistent/skills-report-once/large.html"
	if !firstReportOfSkippedSkill(path) {
		t.Fatal("first report")
	}
	if firstReportOfSkippedSkill(path) {
		t.Fatal("second report")
	}
	if !firstReportOfSkippedSkill("/nonexistent/skills-report-once/other.html") {
		t.Fatal("other file")
	}
}

// A profile home that links a skill from elsewhere is staged with the link's
// contents, so the canonical tree the sync compares against has to read
// through the link too. Otherwise the first successful sync removes the
// linked skill from every session.
//
// Hard-won: 17a0618f: Canonical sync omitted linked skills that launch staging copied into each session.
func TestProfileCollectionFollowsLinksAsStagingDoes(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only")
	}
	outside := t.TempDir()
	writeSkill(t, outside, "viz/SKILL.md", []byte("linked viz"))
	home := t.TempDir()
	writeSkill(t, home, "skills/own/SKILL.md", []byte("own"))
	for link, target := range map[string]string{
		"skills/viz":  filepath.Join(outside, "viz"),
		"skills/gone": filepath.Join(outside, "missing"),
		// A link back to an ancestor is entered once, not forever.
		"skills/own/loop": filepath.Join(home, "skills"),
	} {
		if err := os.Symlink(target, filepath.Join(home, link)); err != nil {
			t.Fatal(err)
		}
	}

	archive := skillsMust(CollectProfileSkills(HarnessKindClaude, home))

	if got := skillsPaths(&archive); !reflect.DeepEqual(got, []string{"skills/own/SKILL.md", "skills/viz/SKILL.md"}) {
		t.Fatalf("paths = %v", got)
	}
	// The session side still ignores links: a worker never reads outside
	// the tree it was given.
	if got := skillsMust(CollectSkills(HarnessKindClaude, home)); len(got.Entries()) != 1 {
		t.Fatalf("%d entries", len(got.Entries()))
	}
}

// harnessOwnedSkills is each harness's own skills, laid out as the harness
// writes them under its home. Claude Code keeps a `.bucket-…` marker and an
// `<org>_<user>` directory holding a manifest and one directory per skill it
// syncs from claude.ai, plus the `.trash` directory it moves removed skills
// into. The Codex CLI keeps its built-in skills in `.system`, beside a marker
// file.
func harnessOwnedSkills(kind HarnessKind) []skillFile {
	switch kind {
	case HarnessKindClaude:
		return []skillFile{
			{"skills/synced/.bucket-org_user", []byte("")},
			{"skills/synced/org_user/manifest.json", []byte("{}")},
			{"skills/synced/org_user/docx/SKILL.md", []byte("docx")},
			{"skills/synced/org_user/docx/ooxml/schema.xsd", []byte("schema")},
			{"skills/.trash/1789646711611/pdf/SKILL.md", []byte("old pdf")},
		}
	case HarnessKindCodex:
		return []skillFile{
			{"skills/.system/.codex-system-skills.marker", []byte("marker")},
			{"skills/.system/imagegen/SKILL.md", []byte("imagegen")},
			{"skills/.system/skill-creator/SKILL.md", []byte("creator")},
		}
	case HarnessKindKimi, HarnessKindGrok, HarnessKindMuse, HarnessKindOpenCode, HarnessKindDsh, HarnessKindAntigravity:
		return nil
	}
	panic("unknown harness kind")
}

// writeHarnessOwnedSkills writes harnessOwnedSkills into home, first checking
// that it puts a file in every path the harness owns.
func writeHarnessOwnedSkills(t *testing.T, kind HarnessKind, home string) {
	t.Helper()
	files := harnessOwnedSkills(kind)
	for _, owned := range kind.HarnessOwnedSkillPaths() {
		if !slices.ContainsFunc(files, func(file skillFile) bool { return within(file.path, owned) }) {
			t.Fatalf("no test file under %v %s", kind, owned)
		}
	}
	for _, file := range files {
		writeSkill(t, home, file.path, file.data)
	}
}

// Claude Code provisions skills/synced/ from the user's claude.ai account and
// re-syncs it on its own; on the launch host its Office schemas alone were
// 4.2 MB, enough to push the tree over the archive limit. The Codex CLI writes
// its built-in skills into skills/.system/ (about 600 KB on the launch host).
// Mjolnir leaves every such path out of every collection of that harness's
// home.
//
// Hard-won: 864ae4f8: Codex built-in skills were copied into every session and inflated the synchronized tree.
func TestHarnessOwnedSkillsAreLeftOutOfCollection(t *testing.T) {
	for _, kind := range HarnessKindAll {
		home := t.TempDir()
		user := []SkillsEntry{{
			Path:  kind.SyncedSkillDirs()[0] + "/review/SKILL.md",
			Bytes: []byte("review"),
		}}
		writeSkill(t, home, user[0].Path, []byte("review"))
		writeHarnessOwnedSkills(t, kind, home)

		if got := skillsMust(CollectSkills(kind, home)); !skillsEntriesEqual(got.Entries(), user) {
			t.Fatalf("%v: collect = %v", kind, skillsPaths(&got))
		}
		if got := skillsMust(CollectProfileSkills(kind, home)); !skillsEntriesEqual(got.Entries(), user) {
			t.Fatalf("%v: collect profile = %v", kind, skillsPaths(&got))
		}
		session := append(slices.Clone(user), ManagedSkills(kind)...)
		sortSkillsEntries(session)
		if got := skillsMust(SessionSkills(kind, home, SkillsArchiveFormatGzip, SkillsScopeLocalhost)); !skillsEntriesEqual(got.Entries(), session) {
			t.Fatalf("%v: session = %v", kind, skillsPaths(&got))
		}
	}

	// The directories belong to one harness, not a rule about the name: in
	// another harness's home a skill called `synced` or `.system` is the
	// user's.
	home := t.TempDir()
	writeHarnessOwnedSkills(t, HarnessKindClaude, home)
	writeHarnessOwnedSkills(t, HarnessKindCodex, home)
	paths := func(kind HarnessKind) []string {
		archive := skillsMust(CollectSkills(kind, home))
		return skillsPaths(&archive)
	}
	codex := paths(HarnessKindCodex)
	if !slices.Contains(codex, "skills/synced/org_user/docx/SKILL.md") {
		t.Fatalf("%v", codex)
	}
	if slices.ContainsFunc(codex, func(path string) bool { return within(path, "skills/.system") }) {
		t.Fatalf("%v", codex)
	}
	claude := paths(HarnessKindClaude)
	if !slices.Contains(claude, "skills/.system/imagegen/SKILL.md") {
		t.Fatalf("%v", claude)
	}
	if slices.ContainsFunc(claude, func(path string) bool { return within(path, "skills/synced") }) {
		t.Fatalf("%v", claude)
	}
}

func skillsExists(path string) bool {
	_, err := os.Stat(path)
	return !errors.Is(err, os.ErrNotExist)
}

// A session's harness keeps its own skills under the session home, and a copy
// that an earlier sync or launch put there cannot be told apart from it. An
// install replaces only what Mjolnir owns, and never writes into the
// harness's directories.
//
// Hard-won: 864ae4f8: An upgrade sync removed harness-owned skills after excluding their directory from the archive.
func TestInstallLeavesHarnessOwnedSkillsInPlace(t *testing.T) {
	for _, kind := range HarnessKindAll {
		home := t.TempDir()
		dir := kind.SyncedSkillDirs()[0]
		review := dir + "/review/SKILL.md"
		old := dir + "/old/SKILL.md"
		writeSkill(t, home, old, []byte("old"))
		writeHarnessOwnedSkills(t, kind, home)
		owned := harnessOwnedSkills(kind)

		// An archive entry over one of the harness's files is ignored.
		pushed := []skillFile{{review, []byte("review")}}
		for _, file := range owned {
			pushed = append(pushed, skillFile{file.path, []byte("pushed over")})
		}
		archive := skillsArchiveOf(pushed)
		if err := InstallSkills(kind, home, &archive); err != nil {
			t.Fatal(err)
		}

		readSkill := func(relative string) string {
			data, err := os.ReadFile(filepath.Join(home, filepath.FromSlash(relative)))
			if err != nil {
				t.Fatal(err)
			}
			return string(data)
		}
		if got := readSkill(review); got != "review" {
			t.Fatalf("%v: review = %q", kind, got)
		}
		if skillsExists(filepath.Join(home, dir, "old")) {
			t.Fatalf("%v: old survived", kind)
		}
		for _, file := range owned {
			if got := readSkill(file.path); got != string(file.data) {
				t.Fatalf("%v %s = %q", kind, file.path, got)
			}
		}
		if skillsExists(filepath.Join(home, dir+".hel-incoming")) || skillsExists(filepath.Join(home, dir+".hel-retired")) {
			t.Fatalf("%v: staging trees left behind", kind)
		}
		// The session reports what Mjolnir pushed, so the next reconcile
		// finds nothing to do.
		collected := skillsMust(CollectSkills(kind, home))
		if !skillsEntriesEqual(collected.Entries(), []SkillsEntry{{Path: review, Bytes: []byte("review")}}) {
			t.Fatalf("%v: collected = %v", kind, skillsPaths(&collected))
		}

		// Removing every Mjolnir skill still leaves the harness's own.
		if err := InstallSkills(kind, home, &SkillsArchive{}); err != nil {
			t.Fatal(err)
		}
		if skillsExists(filepath.Join(home, "skills/review")) {
			t.Fatalf("%v: review survived", kind)
		}
		for _, file := range owned {
			if got := readSkill(file.path); got != string(file.data) {
				t.Fatalf("%v %s = %q", kind, file.path, got)
			}
		}
	}
}

func TestCollectionEnforcesTheFileCountCap(t *testing.T) {
	home := t.TempDir()
	for index := range MaxSkillsFiles + 1 {
		writeSkill(t, home, fmt.Sprintf("skills/skill-%d", index), []byte("x"))
	}
	if _, err := CollectSkills(HarnessKindKimi, home); err == nil {
		t.Fatal("collection exceeded the file cap")
	}
}

func TestInstallRefusesASymlinkedDestination(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only")
	}
	home := t.TempDir()
	elsewhere := t.TempDir()
	if err := os.Symlink(elsewhere, filepath.Join(home, "skills")); err != nil {
		t.Fatal(err)
	}
	archive := skillsArchiveOf([]skillFile{{"skills/evil", []byte("payload")}})
	err := InstallSkills(HarnessKindCodex, home, &archive)
	if err == nil || !strings.Contains(err.Error(), "symbolic link") {
		t.Fatalf("error = %v", err)
	}
	if skillsExists(filepath.Join(elsewhere, "evil")) {
		t.Fatal("wrote through the link")
	}
}

func TestInstallRejectsEntriesOutsideTheWhitelist(t *testing.T) {
	home := t.TempDir()
	hostile := SkillsArchive{entries: []SkillsEntry{
		{Path: "skills/ok", Bytes: []byte("ok")},
		{Path: "plugins/not-skills", Bytes: []byte("evil")},
	}}
	// Whitelist filtering keeps the foreign entry out of every synced
	// directory, so it is ignored rather than written.
	if err := InstallSkills(HarnessKindKimi, home, &hostile); err != nil {
		t.Fatal(err)
	}
	if !skillsExists(filepath.Join(home, "skills/ok")) {
		t.Fatal("skills/ok is missing")
	}
	if skillsExists(filepath.Join(home, "plugins")) {
		t.Fatal("plugins was written")
	}
}
