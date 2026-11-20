// Port of mj-core/src/skills/mod.rs.

// Harness skills whitelist and its deterministic wire archive.
//
// A profile's skills tree is user-managed content, not a rotating secret, so
// unlike credentials it syncs in one direction only: the controller-side
// canonical home is authoritative and every live session converges to it.
// This module owns the whole interpretation — which directories each harness
// syncs, how a tree becomes a fingerprinted archive, and how an archive is
// installed — with no relay, async, or process dependencies, so every rule
// is testable in isolation.

package core

import (
	"compress/gzip"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"strings"
	"sync"
)

// SkillsScope is whether an agent can use the user's host CLI and
// configuration.
type SkillsScope int

const (
	SkillsScopeLocalhost SkillsScope = iota
	SkillsScopeIsolated
)

// MaxSkillsArchiveBytes: skills archives travel base64-encoded inside an
// 8 MiB relay frame. The cap applies to the archive as it is sent, so to the
// compressed size of a HELSKIL2 archive, and keeps the encoded payload,
// envelope, and a credential payload comfortably inside one frame each way.
const MaxSkillsArchiveBytes = 4 * 1024 * 1024

// MaxSkillsFileBytes: a single skill file whose compressed size is above this
// is almost certainly a checked-in binary, not a skill. The size is measured
// by compressing the file alone. A file of at most this many bytes fits
// without being measured, and a text file of several megabytes compresses
// well under it.
const MaxSkillsFileBytes uint64 = 1024 * 1024

const MaxSkillsFiles = 1024

// MaxSkillsTreeBytes is the uncompressed size of a whole skills tree, as its
// HELSKIL1 encoding. Real skills reach the compressed archive limit long
// before this one. It stops a small compressed archive from inflating into
// more memory than a worker should spend, and a file larger than this is
// never read.
const MaxSkillsTreeBytes uint64 = 64 * 1024 * 1024

// plainMagic marks an archive whose entries are stored as they are.
const plainMagic = "HELSKIL1"

// gzipMagic marks an archive whose body, everything a HELSKIL1 archive holds
// after its magic, is gzip-compressed.
const gzipMagic = "HELSKIL2"

// SkillsArchiveFormat is how an archive is written for the wire. Both formats
// carry the same entries, and an archive's fingerprint does not depend on
// which one carries it. Each format has its own size limits, applied when a
// tree is collected for it and again when an archive is read.
type SkillsArchiveFormat int

const (
	// SkillsArchiveFormatPlain is HELSKIL1: every file stored as it is. The
	// per-file and per-archive limits apply to raw sizes.
	SkillsArchiveFormatPlain SkillsArchiveFormat = iota
	// SkillsArchiveFormatGzip is HELSKIL2: the HELSKIL1 body, gzip-compressed
	// at the default level. The per-file and per-archive limits apply to
	// compressed sizes.
	SkillsArchiveFormatGzip
)

// sizeProblem says why a file of length bytes cannot travel in this format,
// judged before reading it.
func (f SkillsArchiveFormat) sizeProblem(length uint64) (string, bool) {
	switch f {
	case SkillsArchiveFormatPlain:
		if length > MaxSkillsFileBytes {
			return fmt.Sprintf("is %d bytes, above the %d byte limit of an uncompressed skills archive", length, MaxSkillsFileBytes), true
		}
	case SkillsArchiveFormatGzip:
		if length > MaxSkillsTreeBytes {
			return fmt.Sprintf("is %d bytes, above the %d byte limit for a whole uncompressed skills tree", length, MaxSkillsTreeBytes), true
		}
	}
	return "", false
}

// contentProblem says why a file holding bytes cannot travel in this format.
func (f SkillsArchiveFormat) contentProblem(bytes []byte) (string, bool) {
	length := uint64(len(bytes))
	if problem, ok := f.sizeProblem(length); ok {
		return problem, true
	}
	if f == SkillsArchiveFormatPlain || length <= MaxSkillsFileBytes {
		return "", false
	}
	compressed := gzipLen(func(w io.Writer) error { _, err := w.Write(bytes); return err })
	if compressed > MaxSkillsFileBytes {
		return fmt.Sprintf("is %d bytes and compresses to %d bytes, above the %d byte limit", length, compressed, MaxSkillsFileBytes), true
	}
	return "", false
}

// gzipLen is the number of bytes gzip writes, at the archive's level, for
// what write puts into it.
func gzipLen(write func(w io.Writer) error) uint64 {
	var count byteCount
	encoder := gzip.NewWriter(&count)
	if err := write(encoder); err != nil {
		panic("counting bytes cannot fail")
	}
	if err := encoder.Close(); err != nil {
		panic("counting bytes cannot fail")
	}
	return uint64(count)
}

// byteCount is a writer that keeps only the number of bytes written to it.
type byteCount uint64

func (c *byteCount) Write(buf []byte) (int, error) {
	*c += byteCount(len(buf))
	return len(buf), nil
}

// SkillsSyncState is non-secret metadata about one copy of a skills tree.
// Fingerprints compare trees; there is no freshness concept because the
// controller copy always wins.
type SkillsSyncState struct {
	Present     bool
	Fingerprint string
}

type SkillsEntry struct {
	// Path is home-relative, /-separated, always inside a synced directory.
	Path  string
	Bytes []byte
}

// SkillsArchive is a deterministic, fingerprintable snapshot of every synced
// skills tree in one harness home.
type SkillsArchive struct {
	entries []SkillsEntry
}

// ForSession selects the managed skill for this target without scanning the
// profile again.
func (a SkillsArchive) ForSession(kind HarnessKind, scope SkillsScope, format SkillsArchiveFormat) (SkillsArchive, error) {
	panic("unported: mj-core/src/skills/mod.rs SkillsArchive::for_session")
}

func (a *SkillsArchive) Entries() []SkillsEntry { return a.entries }

func (a *SkillsArchive) IsEmpty() bool { return len(a.entries) == 0 }

func (a *SkillsArchive) State() SkillsSyncState {
	return SkillsSyncState{Present: len(a.entries) != 0, Fingerprint: a.Fingerprint()}
}

// Fingerprint is SHA-256 over the HELSKIL1 encoding, whichever format carries
// the archive, so a fingerprint does not change with the wire format.
// Collection sorts entries, so two homes holding the same tree fingerprint
// identically.
func (a *SkillsArchive) Fingerprint() string {
	sum := sha256.Sum256(a.Encode(SkillsArchiveFormatPlain))
	return LowerHex(sum[:])
}

func (a *SkillsArchive) Encode(format SkillsArchiveFormat) []byte {
	panic("unported: mj-core/src/skills/mod.rs SkillsArchive::encode")
}

// plainLen is the number of bytes in the HELSKIL1 encoding, counted without
// writing it.
func (a *SkillsArchive) plainLen() int {
	total := len(plainMagic) + 4
	for _, entry := range a.entries {
		total += 4 + len(entry.Path) + 8 + len(entry.Bytes)
	}
	return total
}

// ensureFits fails unless this archive can travel in format. tree names the
// tree in the error.
func (a *SkillsArchive) ensureFits(format SkillsArchiveFormat, tree string) error {
	panic("unported: mj-core/src/skills/mod.rs SkillsArchive::ensure_fits")
}

// writeBody writes everything an archive holds after its magic: the entry
// count, then each entry's path and contents, each preceded by its length.
func (a *SkillsArchive) writeBody(out io.Writer) error {
	var buf [8]byte
	binary.LittleEndian.PutUint32(buf[:4], uint32(len(a.entries)))
	if _, err := out.Write(buf[:4]); err != nil {
		return err
	}
	for _, entry := range a.entries {
		binary.LittleEndian.PutUint32(buf[:4], uint32(len(entry.Path)))
		binary.LittleEndian.PutUint64(buf[4:], uint64(len(entry.Bytes)))
		for _, part := range [][]byte{buf[:4], []byte(entry.Path), buf[4:], entry.Bytes} {
			if _, err := out.Write(part); err != nil {
				return err
			}
		}
	}
	return nil
}

// DecodeSkillsArchive parses an untrusted archive in either format. Every
// rule that keeps an install inside the whitelist is enforced here and again
// at install time: relative /-separated paths, no traversal, sorted and
// unique, within caps.
func DecodeSkillsArchive(bytes []byte) (SkillsArchive, error) {
	panic("unported: mj-core/src/skills/mod.rs SkillsArchive::decode")
}

// decodeSkillsArchiveBody parses the body of an archive that arrived in
// format.
func decodeSkillsArchiveBody(body []byte, format SkillsArchiveFormat) (SkillsArchive, error) {
	panic("unported: mj-core/src/skills/mod.rs SkillsArchive::decode_body")
}

// inflate inflates the gzip body of a HELSKIL2 archive. limit bounds the
// inflated size, so a small hostile archive cannot make the reader allocate
// without bound.
func inflate(compressed []byte, limit uint64) ([]byte, error) {
	panic("unported: mj-core/src/skills/mod.rs inflate")
}

// links is how a collection treats a symbolic link inside a skills tree.
type links int

const (
	// linksSkip leaves it out. A session's own home holds only the regular
	// files staging and installs wrote, and its worker never reads outside
	// them.
	linksSkip links = iota
	// linksFollow reads through it, as launch staging copies a profile home.
	linksFollow
)

// CollectSkills snapshots the synced skills trees of one home. A home without
// any synced directory collects as an empty archive, which compares equal to
// a session in the same state. Symlinks inside the tree are skipped: this is
// how a worker reads its session's staged home. CollectProfileSkills reads
// the profile home that launch staging copies from instead.
//
// Both leave out the paths the harness maintains itself
// (HarnessKind.HarnessOwnedSkillPaths), such as the skills Claude Code syncs
// from the user's claude.ai account: the harness keeps its own copy current
// in every home, so Mjolnir neither compares nor copies it.
//
// The tree is collected for the compressed format a current worker reads.
func CollectSkills(kind HarnessKind, home string) (SkillsArchive, error) {
	return collect(kind, home, linksSkip, SkillsArchiveFormatGzip)
}

// CollectProfileSkills snapshots a profile home's synced skills trees the way
// launch staging copies them: through symbolic links, skipping a link whose
// target is missing and a directory that links back into itself. A session
// whose home was staged from this profile then fingerprints the same as this
// archive.
func CollectProfileSkills(kind HarnessKind, home string) (SkillsArchive, error) {
	return collect(kind, home, linksFollow, SkillsArchiveFormatGzip)
}

func collect(kind HarnessKind, home string, links links, format SkillsArchiveFormat) (SkillsArchive, error) {
	archive, err := collectFiles(kind, home, links, format)
	if err != nil {
		return SkillsArchive{}, err
	}
	if err := archive.ensureFits(format, "skills tree under "+home); err != nil {
		return SkillsArchive{}, err
	}
	return archive, nil
}

// collectFiles collects every file of the synced trees that can travel in
// format on its own, without the limits on the whole archive.
func collectFiles(kind HarnessKind, home string, links links, format SkillsArchiveFormat) (SkillsArchive, error) {
	panic("unported: mj-core/src/skills/mod.rs collect_files")
}

// SessionSkills is the skills tree a session gets: the user's own skills from
// home, plus the host CLI skill on localhost. The reserved mj directory is
// replaced on localhost and excluded entirely on isolated targets.
//
// Launch staging and the credential-sync push both reserve the complete mj
// directory. They must agree on scope or the first reconciliation would
// reinstall a host skill on an isolated target.
//
// The tree is collected for format, the format the session's worker reads, so
// a file too large for that format is left out on both sides.
func SessionSkills(kind HarnessKind, home string, format SkillsArchiveFormat, scope SkillsScope) (SkillsArchive, error) {
	collected, err := collectFiles(kind, home, linksFollow, format)
	if err != nil {
		return SkillsArchive{}, err
	}
	return collected.ForSession(kind, scope, format)
}

// walk is how one collection walks a skills tree.
type walk struct {
	links links
	// format is the format the tree is collected for, which decides the
	// per-file limit.
	format SkillsArchiveFormat
	// harnessOwned holds home-relative paths the harness maintains itself;
	// the walk leaves them out (HarnessKind.HarnessOwnedSkillPaths).
	harnessOwned []string
}

// collectTree walks root. entered holds the resolved directories already
// entered on this branch of the walk, which stops a followed link that points
// back at an ancestor.
func collectTree(root string, prefix string, entries *[]SkillsEntry, walk walk, entered []string) error {
	panic("unported: mj-core/src/skills/mod.rs collect_tree")
}

// skipSkillFile leaves one skill file out of a collection, saying so the
// first time.
func skipSkillFile(path string, problem string) {
	panic("unported: mj-core/src/skills/mod.rs skip_skill_file")
}

var (
	reportedSkippedSkillsMu sync.Mutex
	reportedSkippedSkills   = map[string]struct{}{}
)

// firstReportOfSkippedSkill is whether this process has not yet reported
// skipping path. Collection runs on every sync poll, so a file that stays too
// large would otherwise be reported once a minute for the life of the worker.
func firstReportOfSkippedSkill(path string) bool {
	reportedSkippedSkillsMu.Lock()
	defer reportedSkippedSkillsMu.Unlock()
	if _, ok := reportedSkippedSkills[path]; ok {
		return false
	}
	reportedSkippedSkills[path] = struct{}{}
	return true
}

// InstallSkills replaces a session home's synced skills trees with an
// archive's contents.
//
// Each synced directory is built beside the destination and swapped in, so a
// failure mid-install never leaves a half-written tree. A symlinked
// destination is refused rather than followed, and entry paths outside the
// harness's synced directories are rejected outright.
//
// The paths the harness maintains itself (HarnessKind.HarnessOwnedSkillPaths)
// are not Mjolnir's to replace: an archive entry inside one is ignored like
// any other entry outside the whitelist, and whatever the session home holds
// there moves into the new tree unchanged.
func InstallSkills(kind HarnessKind, home string, archive *SkillsArchive) error {
	panic("unported: mj-core/src/skills/mod.rs install_skills")
}

// within is whether a /-separated path is directory or lies beneath it.
func within(path, directory string) bool {
	rest, ok := strings.CutPrefix(path, directory)
	return ok && (rest == "" || strings.HasPrefix(rest, "/"))
}

// installTree swaps entries in as the tree at dir, carrying the destination's
// kept children (names directly under dir) into the new tree as they are.
func installTree(home string, dir string, entries []*SkillsEntry, kept []string) error {
	panic("unported: mj-core/src/skills/mod.rs install_tree")
}

// moveChildren moves the named children of from into to. A failure moves the
// children already moved back, so the harness's own directories stay in one
// tree.
func moveChildren(from, to string, names []string) error {
	panic("unported: mj-core/src/skills/mod.rs move_children")
}

func validateArchivePath(path string) error {
	panic("unported: mj-core/src/skills/mod.rs validate_archive_path")
}

// cursor is a byte cursor with checked bounds so decode failures are errors,
// not panics.
type cursor struct {
	data []byte
}

func (c *cursor) take(count int) ([]byte, error) {
	if count < 0 || len(c.data) < count {
		return nil, errors.New("skills archive is truncated")
	}
	taken := c.data[:count]
	c.data = c.data[count:]
	return taken, nil
}

func (c *cursor) u32() (uint32, error) {
	b, err := c.take(4)
	if err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint32(b), nil
}

func (c *cursor) u64() (uint64, error) {
	b, err := c.take(8)
	if err != nil {
		return 0, err
	}
	return binary.LittleEndian.Uint64(b), nil
}

func (c *cursor) rest() []byte { return c.data }
