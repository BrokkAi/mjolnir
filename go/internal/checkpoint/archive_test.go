// Port of mj-checkpoint/src/archive/tests.rs, with the archive.rs helpers
// that Rust compiles only for tests.

package checkpoint

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"compress/zlib"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"maps"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func prepareArchive(input *ArchiveInput) (core.ArchiveManifest, []pendingPayload, error) {
	return prepareArchiveWithPartSize(input, payloadPartBytes)
}

func prepareArchiveWithPartSize(input *ArchiveInput, partBytes int) (core.ArchiveManifest, []pendingPayload, error) {
	return prepareArchiveViewWithPartSize(input.view(), partBytes)
}

// collectionGit answers the Git commands snapshot collection runs, without
// a repository. With concurrentPayloads, the four payload commands wait for
// each other, so collection only finishes when it runs them concurrently.
type collectionGit struct {
	deltaCount     uint64
	payloadBarrier *collectionBarrier
}

func newCollectionGit(deltaCount uint64, concurrentPayloads bool) *collectionGit {
	runner := &collectionGit{deltaCount: deltaCount}
	if concurrentPayloads {
		runner.payloadBarrier = newCollectionBarrier(4)
	}
	return runner
}

func (g *collectionGit) Run(repository string, command *GitCommand) (GitOutput, error) {
	arguments := command.Arguments
	has := func(value string) bool { return slices.Contains(arguments, value) }
	first := ""
	if len(arguments) > 0 {
		first = arguments[0]
	}
	status, stdout := 0, []byte{}
	switch {
	// The collected directory is its checkout's top level.
	case first == "rev-parse" && has("--show-toplevel"):
		stdout = []byte(repository + "\n\n")
	case first == "rev-parse" && has("--absolute-git-dir"):
		stdout = []byte(repository + "\n" + repository + "\n")
	case first == "show-ref":
		status = 1
	// Only the checkout itself: no linked worktrees.
	case first == "worktree":
		stdout = []byte("worktree " + repository + "\n")
	case first == "rev-parse" && slices.ContainsFunc(arguments, func(argument string) bool {
		return strings.HasPrefix(argument, "refs/hel/")
	}):
		status = 1
	case first == "for-each-ref" && has("refs/mj/"):
	case first == "remote":
		stdout = []byte("https://token@github.com/example/repo.git\n")
	case first == "rev-parse":
		stdout = []byte(strings.Repeat("b", 40) + "\n")
	case first == "merge-base":
		stdout = []byte(strings.Repeat("b", 40) + "\n")
	case first == "for-each-ref":
		stdout = []byte(strings.Repeat("c", 40) + "\n")
	case first == "symbolic-ref":
		stdout = []byte("feature/hel\n")
	case first == "rev-list":
		stdout = []byte(fmt.Sprintf("%d\n", g.deltaCount))
	case first == "bundle":
		g.payloadBarrier.wait()
		stdout = []byte("bundle")
	case first == "diff":
		if g.payloadBarrier != nil {
			g.payloadBarrier.wait()
		}
		if has("--cached") {
			stdout = []byte("staged")
		} else {
			stdout = []byte("unstaged")
		}
	case first == "ls-files":
		if g.payloadBarrier != nil {
			g.payloadBarrier.wait()
		}
		stdout = []byte("note.txt\x00.env\x00")
	case first == "config":
		status = 1
	default:
		other := "None"
		if len(arguments) > 0 {
			other = "Some(" + archiveDebugString(first) + ")"
		}
		return GitOutput{}, fmt.Errorf("unexpected Git command: %s", other)
	}
	return GitOutput{Status: status, Stdout: stdout, Stderr: []byte{}}, nil
}

// collectionBarrier is std::sync::Barrier: every wait blocks until parties
// goroutines are waiting, and then it is ready for the next round.
type collectionBarrier struct {
	mutex      sync.Mutex
	condition  *sync.Cond
	parties    int
	waiting    int
	generation int
}

func newCollectionBarrier(parties int) *collectionBarrier {
	barrier := &collectionBarrier{parties: parties}
	barrier.condition = sync.NewCond(&barrier.mutex)
	return barrier
}

func (b *collectionBarrier) wait() {
	b.mutex.Lock()
	defer b.mutex.Unlock()
	generation := b.generation
	b.waiting++
	if b.waiting == b.parties {
		b.waiting = 0
		b.generation++
		b.condition.Broadcast()
		return
	}
	for generation == b.generation {
		b.condition.Wait()
	}
}

// initializeRepository creates a Git repository at path with branch main
// and a test identity.
func initializeRepository(t testing.TB, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o777); err != nil {
		t.Fatal(err)
	}
	git(path, "init", "-q", "-b", "main")
	git(path, "config", "user.name", "Hel Test")
	git(path, "config", "user.email", "hel@example.test")
}

// cloneRepository clones origin as parent/name with a test identity and
// returns the clone's path.
func cloneRepository(t testing.TB, parent, origin, name string) string {
	t.Helper()
	destination := filepath.Join(parent, name)
	git(parent, "clone", "-q", origin, destination)
	git(destination, "config", "user.name", "Hel Test")
	git(destination, "config", "user.email", "hel@example.test")
	return destination
}

func commitFile(t testing.TB, repository, name string, contents []byte, message string) {
	t.Helper()
	archiveWriteFile(t, filepath.Join(repository, name), contents)
	git(repository, "add", ".")
	git(repository, "commit", "-qm", message)
}

func archiveWriteFile(t testing.TB, path string, contents []byte) {
	t.Helper()
	if err := os.WriteFile(path, contents, 0o666); err != nil {
		t.Fatal(err)
	}
}

func archiveReadFile(t testing.TB, path string) []byte {
	t.Helper()
	contents, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return contents
}

// tarWithFile is a tar holding one regular file, byte for byte as the Rust
// tar crate writes a `Header::new_gnu` entry: magic "ustar " and version
// " \0", octal fields ending in NUL, a zero mtime, and no owner fields.
func tarWithFile(path string, contents []byte, mode uint32) []byte {
	header := make([]byte, 512)
	copy(header[0:100], path)
	tarOctalInto(header[100:108], uint64(mode))
	tarOctalInto(header[124:136], uint64(len(contents)))
	tarOctalInto(header[136:148], 0)
	copy(header[257:263], "ustar ")
	copy(header[263:265], " \x00")
	checksum := 0
	for index, b := range header {
		if index >= 148 && index < 156 {
			checksum += ' '
		} else {
			checksum += int(b)
		}
	}
	tarOctalInto(header[148:156], uint64(checksum))
	output := append(header, contents...)
	if remainder := len(contents) % 512; remainder != 0 {
		output = append(output, make([]byte, 512-remainder)...)
	}
	return append(output, make([]byte, 1024)...)
}

// tarOctalInto writes value as the tar crate's octal_into does: a final NUL,
// the digits right-aligned before it, and '0' padding on the left.
func tarOctalInto(field []byte, value uint64) {
	digits := strconv.FormatUint(value, 8)
	for index := range field {
		field[index] = '0'
	}
	field[len(field)-1] = 0
	copy(field[len(field)-1-len(digits):], digits)
}

// emptyTar is `tar::Builder::new(Vec::new()).into_inner()`: the two zero
// blocks that end an archive.
func emptyTar() []byte {
	return make([]byte, 1024)
}

func archiveRef[T any](value T) *T {
	return &value
}

func repositoryFixture(id string) RepositorySnapshot {
	return RepositorySnapshot{
		Metadata: core.RepositoryMetadata{
			ID:                  id,
			RelativeDestination: id,
			Origin:              fmt.Sprintf("https://github.com/example/%s.git", id),
			BaseCommit:          strings.Repeat("a", 40),
			HeadCommit:          strings.Repeat("b", 40),
			Branch:              archiveRef("feature/hel"),
		},
		CommittedBundle: []byte("bundle-" + id),
		StagedPatch:     []byte("staged-" + id),
		UnstagedPatch:   []byte("unstaged-" + id),
		UntrackedTar:    tarWithFile("scripts/tool.sh", []byte("#!/bin/sh\n"), 0o755),
	}
}

func checkpointBundle(head string, contents []byte) *CheckpointRepositoryBundle {
	return &CheckpointRepositoryBundle{
		Metadata: core.RepositoryMetadata{
			ID:                  "project",
			RelativeDestination: "project",
			Origin:              "https://github.com/example/project.git",
			HeadCommit:          head,
			Branch:              archiveRef("main"),
		},
		CommittedBundle: contents,
	}
}

func archiveAssertErrorContains(t testing.TB, err error, text string) {
	t.Helper()
	if err == nil {
		t.Fatalf("expected an error containing %q, got none", text)
	}
	if !strings.Contains(err.Error(), text) {
		t.Fatalf("expected an error containing %q, got: %v", text, err)
	}
}

func archiveMust[T any](t testing.TB) func(value T, err error) T {
	return func(value T, err error) T {
		t.Helper()
		if err != nil {
			t.Fatal(err)
		}
		return value
	}
}

// archiveAssertSameJSON compares two values as Rust compares them with ==, by
// what they serialize to.
func archiveAssertSameJSON(t testing.TB, want, got any) {
	t.Helper()
	wantJSON, err := archiveSerdeJSON(want)
	if err != nil {
		t.Fatal(err)
	}
	gotJSON, err := archiveSerdeJSON(got)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(wantJSON, gotJSON) {
		t.Fatalf("values differ:\nwant %s\ngot  %s", wantJSON, gotJSON)
	}
}

// archiveJSONValue decodes data as serde_json::Value, keeping numbers exact.
func archiveJSONValue(t testing.TB, data []byte) any {
	t.Helper()
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		t.Fatal(err)
	}
	return value
}

func archiveJSONObject(t testing.TB, value any) map[string]any {
	t.Helper()
	encoded, err := archiveSerdeJSON(value)
	if err != nil {
		t.Fatal(err)
	}
	return archiveJSONValue(t, encoded).(map[string]any)
}

func archiveMarshal(t testing.TB, value any) []byte {
	t.Helper()
	encoded, err := json.Marshal(value)
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

func TestCheckpointBundleHeaderReportsDeclaredPrerequisitesWithoutImportingPack(t *testing.T) {
	first := strings.Repeat("a", 40)
	second := strings.Repeat("B", 40)
	head := strings.Repeat("c", 40)
	contents := fmt.Sprintf("# v2 git bundle\n-%s first base\n-%s second base\n%s HEAD\n\nPACKnot-read", first, second, head)

	prerequisites, err := CheckpointBundlePrerequisites(checkpointBundle(head, []byte(contents)))
	if err != nil {
		t.Fatal(err)
	}
	if want := []string{first, strings.ToLower(second)}; !slices.Equal(prerequisites, want) {
		t.Fatalf("prerequisites = %q, want %q", prerequisites, want)
	}
}

func TestCheckpointBundleHeaderAcceptsV3Sha256AndSelfContainedBundles(t *testing.T) {
	prerequisite := strings.Repeat("a", 64)
	head := strings.Repeat("b", 64)
	contents := fmt.Sprintf("# v3 git bundle\n@object-format=sha256\n-%s base\n%s HEAD\n\nPACKnot-read", prerequisite, head)
	prerequisites, err := CheckpointBundlePrerequisites(checkpointBundle(head, []byte(contents)))
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(prerequisites, []string{prerequisite}) {
		t.Fatalf("prerequisites = %q", prerequisites)
	}

	selfContained := fmt.Sprintf("# v2 git bundle\n%s HEAD\n\nPACKnot-read", head)
	prerequisites, err = CheckpointBundlePrerequisites(checkpointBundle(head, []byte(selfContained)))
	if err != nil {
		t.Fatal(err)
	}
	if len(prerequisites) != 0 {
		t.Fatalf("prerequisites = %q", prerequisites)
	}
}

func TestCheckpointBundleHeaderRejectsMetadataMismatchAndMalformedPayloads(t *testing.T) {
	head := strings.Repeat("b", 40)
	other := strings.Repeat("c", 40)
	mismatch := fmt.Sprintf("# v2 git bundle\n%s HEAD\n\nPACKnot-read", other)
	_, err := CheckpointBundlePrerequisites(checkpointBundle(head, []byte(mismatch)))
	archiveAssertErrorContains(t, err, "HEAD does not match")

	malformed := fmt.Sprintf("# v2 git bundle\n%s HEAD\n\nnot-a-pack", head)
	_, err = CheckpointBundlePrerequisites(checkpointBundle(head, []byte(malformed)))
	archiveAssertErrorContains(t, err, "no pack payload")
}

func TestCheckpointWithoutABundleUsesItsHeadAsTheSourceBoundary(t *testing.T) {
	head := strings.Repeat("b", 40)
	prerequisites, err := CheckpointBundlePrerequisites(checkpointBundle(head, nil))
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(prerequisites, []string{head}) {
		t.Fatalf("prerequisites = %q", prerequisites)
	}
}

// input is the archive input the archive tests start from: a Codex session
// with a transcript, a queued prompt, one native artifact and the
// repositories "hel" and "worker".
func input() ArchiveInput {
	return ArchiveInput{
		Session: core.SessionManifest{
			ID:              "session-1",
			Title:           "Forge Hel",
			HarnessKind:     core.HarnessKindCodex,
			ProfileID:       "codex-1",
			NativeSessionID: "native-1",
			CreatedAt:       "2026-08-09T10:00:00Z",
			CheckpointedAt:  "2026-08-09T10:05:00Z",
			HelVersion:      "0.1.0",
			RelayVersion:    "0.1.0",
			AdapterVersion:  "0.1.0",
		},
		Target: core.TargetManifest{
			TemplateID: "podman-rust",
			TargetKind: "local_podman",
			Details:    map[string]string{"image": "fedora:latest"},
		},
		Bundle: core.BundleManifest{ID: "hel", PrimaryRepository: "hel"},
		CanonicalSession: core.CanonicalSessionSnapshot{
			EventFrontier:       4,
			EventFrontierDigest: strings.Repeat("a", 64),
			Session: core.CanonicalSessionState{
				Execution:        core.CanonicalExecutionState{State: core.CanonicalExecutionStateIdle},
				LastActivityAtMs: archiveRef(int64(104)),
				SessionTitle:     archiveRef("Forge Hel"),
				Configuration: core.SessionConfiguration{
					Values: map[string]any{"reasoning_effort": "high"},
				},
			},
			// The workspace builds serde_json with preserve_order (through
			// agent-client-protocol), so json! objects keep the key order they
			// are written in.
			Transcript: []core.CanonicalTranscriptItem{
				{
					StableID:        "user-1",
					Position:        1,
					CreatedAtMs:     100,
					LastChangedAtMs: 100,
					Body: core.CanonicalTranscriptBodyUser{
						Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"hello"}`)},
					},
				},
				{
					StableID:                  "agent-2",
					Position:                  2,
					LatestContentEventOrdinal: archiveRef(uint64(2)),
					CreatedAtMs:               101,
					LastChangedAtMs:           103,
					Body: core.CanonicalTranscriptBodyAgent{
						Chunks: []json.RawMessage{json.RawMessage(`{"content":{"type":"text","text":"hi"}}`)},
					},
				},
			},
			QueuedPrompts: []core.CanonicalQueuedPrompt{{
				CommandID:  "prompt-4",
				Kind:       core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindPrompt},
				Content:    []json.RawMessage{json.RawMessage(`{"type":"text","text":"next"}`)},
				QueuedAtMs: 104,
			}},
		},
		NativeArtifacts: []core.NativeArtifact{{
			RelativePath: "sessions/native-1/rollout.jsonl",
			Data:         core.NativeArtifactDataBytes("native transcript"),
			Mode:         0o600,
		}},
		Repositories: []RepositorySnapshot{repositoryFixture("hel"), repositoryFixture("worker")},
	}
}

func TestArchiveRoundTripVerifiesMultiRepoPayloadsAndMode(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.hel.zip")
	archiveInput := input()
	verified := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))
	if len(verified.Manifest.Repositories) != 2 {
		t.Fatalf("repositories = %d", len(verified.Manifest.Repositories))
	}
	archiveAssertSameJSON(t, input().CanonicalSession, verified.CanonicalSession)
	if verified.Manifest.SchemaVersion != core.ArchiveSchemaVersion {
		t.Fatalf("schema = %d", verified.Manifest.SchemaVersion)
	}
	if len(verified.ArchiveSha256) != 64 {
		t.Fatalf("archive digest %q", verified.ArchiveSha256)
	}
	repositoryBundles := archiveMust[VerifiedRepositoryBundles](t)(VerifyRepositoryBundlesStreaming(path))
	if repositoryBundles.ArchiveSha256 != verified.ArchiveSha256 {
		t.Fatalf("bundle digest %q, want %q", repositoryBundles.ArchiveSha256, verified.ArchiveSha256)
	}
	want := [][2]string{{"hel", "bundle-hel"}, {"worker", "bundle-worker"}}
	var read [][2]string
	for _, repository := range archiveMust[[]CheckpointRepositoryBundle](t)(ReadCheckpointRepositoryBundles(path)) {
		read = append(read, [2]string{repository.Metadata.ID, string(repository.CommittedBundle)})
	}
	if !slices.Equal(read, want) {
		t.Fatalf("checkpoint bundles = %q", read)
	}
	var verifiedBundles [][2]string
	for _, repository := range repositoryBundles.Repositories {
		verifiedBundles = append(verifiedBundles, [2]string{repository.Metadata.ID, string(repository.CommittedBundle)})
	}
	if !slices.Equal(verifiedBundles, want) {
		t.Fatalf("verified bundles = %q", verifiedBundles)
	}
	if runtime.GOOS != "windows" {
		info, err := os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		if info.Mode().Perm() != 0o600 {
			t.Fatalf("archive mode %o", info.Mode().Perm())
		}
	}
}

func TestStreamingVerificationDoesNotRetainLargeNoncanonicalPayloads(t *testing.T) {
	const largePayloadBytes = 16 * 1024 * 1024
	path := filepath.Join(t.TempDir(), "large.hel.zip")
	archiveInput := input()
	archiveInput.Repositories = nil
	archiveInput.NativeArtifacts = []core.NativeArtifact{{
		RelativePath: "sessions/native-1/large-rollout.jsonl",
		Data:         core.NativeArtifactDataBytes(bytes.Repeat([]byte("x"), largePayloadBytes)),
		Mode:         0o600,
	}}

	verified := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))

	index := slices.IndexFunc(verified.Manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role.Kind == core.PayloadRoleNativeArtifact
	})
	if verified.Manifest.Payloads[index].Size != largePayloadBytes {
		t.Fatalf("native size %d", verified.Manifest.Payloads[index].Size)
	}
	archiveAssertSameJSON(t, input().CanonicalSession, verified.CanonicalSession)
	retainedMetadataBytes := len(archiveMarshal(t, verified.Manifest)) +
		len(archiveMarshal(t, verified.CanonicalSession)) + len(verified.ArchiveSha256)
	if retainedMetadataBytes >= largePayloadBytes/100 {
		t.Fatalf("retained %d bytes of metadata", retainedMetadataBytes)
	}
	// A preparation cancellation reaches readers sharing the archive for
	// parallel verification.
	cancelled := new(atomic.Bool)
	archive, err := openArchiveCancellable(path, cancelled, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer archive.close()
	cancelled.Store(true)
	reader, err := archive.files[0].Open()
	if err == nil {
		_, err = io.ReadAll(reader)
		reader.Close()
	}
	if err == nil {
		t.Fatal("a cancelled archive was still readable")
	}
	if _, err := VerifyArchiveStreamingCancellable(path, cancelled); err == nil {
		t.Fatal("a cancelled verification succeeded")
	}
}

const testPartBytes = 4096

func archiveZipEntryNames(t testing.TB, path string) []string {
	t.Helper()
	reader, err := zip.OpenReader(path)
	if err != nil && !errors.Is(err, zip.ErrInsecurePath) {
		t.Fatal(err)
	}
	defer reader.Close()
	var names []string
	for _, file := range reader.File {
		names = append(names, file.Name)
	}
	return names
}

func archiveZipEntry(t testing.TB, path, name string) (*zip.ReadCloser, *zip.File) {
	t.Helper()
	reader, err := zip.OpenReader(path)
	if err != nil && !errors.Is(err, zip.ErrInsecurePath) {
		t.Fatal(err)
	}
	reader.RegisterDecompressor(zipMethodZstd, zstdDecompressor)
	for _, file := range reader.File {
		if file.Name == name {
			return reader, file
		}
	}
	reader.Close()
	t.Fatalf("archive %s has no entry %s", path, name)
	return nil, nil
}

func archiveZipEntryMethod(t testing.TB, path, name string) uint16 {
	t.Helper()
	reader, file := archiveZipEntry(t, path, name)
	defer reader.Close()
	return file.Method
}

func archiveZipEntryText(t testing.TB, path, name string) string {
	t.Helper()
	reader, file := archiveZipEntry(t, path, name)
	defer reader.Close()
	contents, err := readZipEntry(file, file.UncompressedSize64)
	if err != nil {
		t.Fatal(err)
	}
	return string(contents)
}

// archiveOverwriteEntryData writes data over the start of an entry's stored body.
func archiveOverwriteEntryData(t testing.TB, path, name string, data []byte) {
	t.Helper()
	reader, file := archiveZipEntry(t, path, name)
	offset, err := file.DataOffset()
	reader.Close()
	if err != nil {
		t.Fatal(err)
	}
	archive, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer archive.Close()
	if _, err := archive.WriteAt(data, offset); err != nil {
		t.Fatal(err)
	}
}

// shardedInput writes an archive whose native artifact and first untracked
// tar are both larger than testPartBytes, so the sharded paths are exercised
// without allocating the production 16 MiB threshold.
func shardedInput() (ArchiveInput, []byte, []byte) {
	archiveInput := input()
	native := bytes.Repeat([]byte("native rollout line\n"), 2_000)
	untracked := tarWithFile("notes/large.txt", bytes.Repeat([]byte("untracked payload line\n"), 1_000), 0o644)
	archiveInput.NativeArtifacts[0].Data = core.NativeArtifactDataBytes(slices.Clone(native))
	archiveInput.Repositories[0].UntrackedTar = slices.Clone(untracked)
	return archiveInput, native, untracked
}

func archiveWriteZip(t testing.TB, path, spoolParent string, manifest *core.ArchiveManifest, payloads []pendingPayload) {
	t.Helper()
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if err := writeZip(file, spoolParent, manifest, payloads); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: 4cc32efd0: last-part EOF checking preceded completion of earlier compression.
func TestOversizedPayloadsShardIntoPartsAndReadBackWhole(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "sharded.hel.zip")
	archiveInput, native, untracked := shardedInput()
	if err := writeArchiveInstalledWithPartSize(path, &archiveInput, testPartBytes); err != nil {
		t.Fatal(err)
	}

	nativePath := "native/sessions/native-1/rollout.jsonl"
	names := archiveZipEntryNames(t, path)
	if slices.Contains(names, nativePath) {
		t.Fatalf("a sharded payload owns no entry of its own: %q", names)
	}
	partNames := 0
	for _, name := range names {
		if strings.HasPrefix(name, nativePath+payloadPartSuffix) {
			partNames++
		}
	}
	if want := (len(native) + testPartBytes - 1) / testPartBytes; partNames != want {
		t.Fatalf("%d part entries, want %d", partNames, want)
	}
	if partNames <= 1 {
		t.Fatalf("%d part entries", partNames)
	}

	metadata := archiveMust[VerifiedArchiveMetadata](t)(VerifyArchiveStreaming(path))
	if metadata.Manifest.SchemaVersion != core.ArchiveSchemaVersionSharded {
		t.Fatalf("schema = %d", metadata.Manifest.SchemaVersion)
	}
	archiveAssertSameJSON(t, archiveInput.CanonicalSession, metadata.CanonicalSession)

	verified := archiveMust[VerifiedArchive](t)(ReadArchiveVerified(path))
	if verified.ArchiveSha256 != metadata.ArchiveSha256 {
		t.Fatalf("digest %q, want %q", verified.ArchiveSha256, metadata.ArchiveSha256)
	}
	payload := archiveMust[[]byte](t)(verified.PayloadByRole(&core.PayloadRole{
		Kind: core.PayloadRoleNativeArtifact, RelativePath: "sessions/native-1/rollout.jsonl",
	}))
	if !bytes.Equal(payload, native) {
		t.Fatal("native payload differs")
	}
	payload = archiveMust[[]byte](t)(verified.PayloadByRole(&core.PayloadRole{
		Kind: core.PayloadRoleGitUntrackedTar, RepositoryID: "hel",
	}))
	if !bytes.Equal(payload, untracked) {
		t.Fatal("untracked payload differs")
	}
	archiveAssertSameJSON(t, archiveInput.CanonicalSession, archiveMust[core.CanonicalSessionSnapshot](t)(verified.CanonicalSession()))
	for path := range verified.Payloads {
		if strings.Contains(path, payloadPartSuffix) {
			t.Fatalf("restore consumers only ever see whole payload paths: %s", path)
		}
	}
	if _, ok := verified.Payloads[nativePath]; !ok {
		t.Fatalf("no payload %s", nativePath)
	}

	// Force the schedule where the last part finishes before the source grows
	// and the earlier parts finish. Completion belongs to ordered assembly.
	sourcePath := filepath.Join(directory, "growing-native.jsonl")
	archiveWriteFile(t, sourcePath, bytes.Repeat(native, 4))
	fileInput := archiveInput
	fileInput.NativeArtifacts = slices.Clone(archiveInput.NativeArtifacts)
	fileInput.NativeArtifacts[0].Data = core.NativeArtifactDataFile(sourcePath)
	_, payloads, err := prepareArchiveWithPartSize(&fileInput, testPartBytes)
	if err != nil {
		t.Fatal(err)
	}
	pending := &payloads[slices.IndexFunc(payloads, func(payload pendingPayload) bool {
		return payload.descriptor.Path == nativePath
	})]
	var entries []streamEntry
	var offset uint64
	for _, part := range pending.descriptor.Parts {
		entries = append(entries, streamEntry{payload: pending, name: part.Path, size: part.Size, digest: part.Sha256, offset: offset})
		offset += part.Size
	}
	var compressed []*os.File
	for index := len(entries) - 1; index >= 0; index-- {
		file, err := compressStreamEntry(&entries[index], directory)
		if err != nil {
			t.Fatal(err)
		}
		compressed = append(compressed, file)
		if len(compressed) == 1 {
			source, err := os.OpenFile(sourcePath, os.O_APPEND|os.O_WRONLY, 0)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := source.WriteString("late source append\n"); err != nil {
				t.Fatal(err)
			}
			source.Close()
		}
	}
	slices.Reverse(compressed)
	output, err := os.Create(filepath.Join(directory, "growing.hel.zip"))
	if err != nil {
		t.Fatal(err)
	}
	defer output.Close()
	writer := zip.NewWriter(output)
	err = appendStreamEntries(writer, entries, compressed)
	archiveAssertErrorContains(t, err, "payload source grew during export")
}

func TestPayloadPartsFollowTheThresholdAndNeverSplitStoredPayloads(t *testing.T) {
	bundle := core.PayloadRole{Kind: core.PayloadRoleGitBundle, RepositoryID: "hel"}
	artifact := core.PayloadRole{Kind: core.PayloadRoleNativeArtifact, RelativePath: "rollout.jsonl"}
	if payloadCompression(&bundle) != zip.Store {
		t.Fatal("bundles are not stored")
	}
	if payloadCompression(&artifact) != zipMethodZstd {
		t.Fatal("artifacts are not Zstandard")
	}

	body := bytes.Repeat([]byte("x"), 10)
	if parts := planPayloadParts("repositories/hel/committed.bundle", body, zip.Store, 4); len(parts) != 0 {
		t.Fatalf("stored payload split into %d parts", len(parts))
	}
	if parts := planPayloadParts("native/rollout.jsonl", body, zipMethodZstd, 10); len(parts) != 0 {
		t.Fatal("a payload at the threshold stays whole")
	}
	parts := planPayloadParts("native/rollout.jsonl", body, zipMethodZstd, 4)
	var paths []string
	var sizes []uint64
	for _, part := range parts {
		paths = append(paths, part.Path)
		sizes = append(sizes, part.Size)
	}
	if want := []string{
		"native/rollout.jsonl.helpart.00000",
		"native/rollout.jsonl.helpart.00001",
		"native/rollout.jsonl.helpart.00002",
	}; !slices.Equal(paths, want) {
		t.Fatalf("part paths = %q", paths)
	}
	if !slices.Equal(sizes, []uint64{4, 4, 2}) {
		t.Fatalf("part sizes = %v", sizes)
	}
	if parts[2].Sha256 != digestBytes(body[8:]) {
		t.Fatal("last part digest differs")
	}
}

func TestGitBundlesAreStoredAndOtherPayloadsUseZstandard(t *testing.T) {
	path := filepath.Join(t.TempDir(), "stored-bundle.hel.zip")
	archiveInput := input()
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))

	if method := archiveZipEntryMethod(t, path, "repositories/hel/committed.bundle"); method != zip.Store {
		t.Fatalf("bundle method %d", method)
	}
	if method := archiveZipEntryMethod(t, path, canonicalSessionPath); method != zipMethodZstd {
		t.Fatalf("canonical session method %d", method)
	}
	if method := archiveZipEntryMethod(t, path, "repositories/hel/untracked.tar"); method != zipMethodZstd {
		t.Fatalf("untracked tar method %d", method)
	}

	verified := archiveMust[VerifiedArchive](t)(ReadArchiveVerified(path))
	payload := archiveMust[[]byte](t)(verified.PayloadByRole(&core.PayloadRole{Kind: core.PayloadRoleGitBundle, RepositoryID: "hel"}))
	if string(payload) != "bundle-hel" {
		t.Fatalf("bundle %q", payload)
	}
}

func archiveNativeDescriptor(t testing.TB, manifest *core.ArchiveManifest) core.PayloadDescriptor {
	t.Helper()
	index := slices.IndexFunc(manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role.Kind == core.PayloadRoleNativeArtifact
	})
	if index < 0 {
		t.Fatal("no native artifact payload")
	}
	return manifest.Payloads[index]
}

func TestVerifiedPayloadReaderRefusesDescriptorOverLimit(t *testing.T) {
	path := filepath.Join(t.TempDir(), "bounded.hel.zip")
	archiveInput := input()
	verified := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))
	descriptor := archiveNativeDescriptor(t, &verified.Manifest)

	_, err := ReadVerifiedCheckpointPayload(path, &descriptor, descriptor.Size-1)
	archiveAssertErrorContains(t, err, "exceeds read limit")
}

func TestVerifiedPayloadReaderRejectsTamperedNonShardedDigest(t *testing.T) {
	path := filepath.Join(t.TempDir(), "digest.hel.zip")
	archiveInput := input()
	verified := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))
	descriptor := archiveNativeDescriptor(t, &verified.Manifest)
	if len(descriptor.Parts) != 0 {
		t.Fatal("native payload is sharded")
	}
	descriptor.Sha256 = strings.Repeat("0", 64)

	_, err := ReadVerifiedCheckpointPayload(path, &descriptor, descriptor.Size)
	archiveAssertErrorContains(t, err, "SHA-256 mismatch")
}

func TestVerifiedPayloadReaderReassemblesShardedPayload(t *testing.T) {
	path := filepath.Join(t.TempDir(), "sharded-reader.hel.zip")
	archiveInput, native, _ := shardedInput()
	if err := writeArchiveInstalledWithPartSize(path, &archiveInput, testPartBytes); err != nil {
		t.Fatal(err)
	}
	manifest := archiveMust[core.ArchiveManifest](t)(ReadCheckpointManifest(path))
	descriptor := archiveNativeDescriptor(t, &manifest)
	if len(descriptor.Parts) == 0 {
		t.Fatal("native payload is not sharded")
	}

	payload := archiveMust[[]byte](t)(ReadVerifiedCheckpointPayload(path, &descriptor, uint64(len(native))))
	if !bytes.Equal(payload, native) {
		t.Fatal("reassembled payload differs")
	}
}

func TestVerifiedPayloadReaderRefusesIncompleteShardCoverage(t *testing.T) {
	path := filepath.Join(t.TempDir(), "sharded-coverage.hel.zip")
	archiveInput, _, _ := shardedInput()
	if err := writeArchiveInstalledWithPartSize(path, &archiveInput, testPartBytes); err != nil {
		t.Fatal(err)
	}
	manifest := archiveMust[core.ArchiveManifest](t)(ReadCheckpointManifest(path))
	descriptor := archiveNativeDescriptor(t, &manifest)
	if len(descriptor.Parts) == 0 {
		t.Fatal("native payload is not sharded")
	}
	descriptor.Parts = descriptor.Parts[:len(descriptor.Parts)-1]

	_, err := ReadVerifiedCheckpointPayload(path, &descriptor, descriptor.Size)
	archiveAssertErrorContains(t, err, "payload parts do not cover declared size")
}

// replaceTarPayload replaces a tar payload in an already prepared archive,
// resharding it so the manifest and the payload body stay consistent.
func replaceTarPayload(t testing.TB, manifest *core.ArchiveManifest, payloads []pendingPayload, role core.PayloadRole, tarBytes []byte, partBytes int) {
	t.Helper()
	index := slices.IndexFunc(manifest.Payloads, func(payload core.PayloadDescriptor) bool { return payload.Role == role })
	if index < 0 {
		t.Fatalf("no payload with role %s", payloadRoleDebug(&role))
	}
	descriptor := &manifest.Payloads[index]
	descriptor.Size = uint64(len(tarBytes))
	descriptor.Sha256 = digestBytes(tarBytes)
	descriptor.Parts = planPayloadParts(descriptor.Path, tarBytes, zipMethodZstd, partBytes)
	replacement := *descriptor
	replacement.Parts = slices.Clone(descriptor.Parts)
	payload := &payloads[slices.IndexFunc(payloads, func(payload pendingPayload) bool {
		return payload.descriptor.Path == replacement.Path
	})]
	payload.descriptor = replacement
	payload.data = tarBytes
}

func TestStreamingVerificationParsesAShardedUntrackedTarForSafety(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "unsafe-sharded-untracked.hel.zip")
	archiveInput, _, _ := shardedInput()
	manifest, payloads, err := prepareArchiveWithPartSize(&archiveInput, testPartBytes)
	if err != nil {
		t.Fatal(err)
	}
	malicious := tarWithFile(".git/config", bytes.Repeat([]byte("unsafe\n"), 2_000), 0o600)
	if len(malicious) <= testPartBytes {
		t.Fatalf("malicious tar is only %d bytes", len(malicious))
	}
	replaceTarPayload(t, &manifest, payloads,
		core.PayloadRole{Kind: core.PayloadRoleGitUntrackedTar, RepositoryID: "hel"}, malicious, testPartBytes)
	archiveWriteZip(t, path, directory, &manifest, payloads)

	_, err = VerifyArchiveStreaming(path)
	archiveAssertErrorContains(t, err, "inside a Git directory")
}

// A restore writes the operation state tar into the Git directory, so the
// verifier must refuse anything in it that is not Git operation state: a
// hook written there would run on the restored clone's next Git command.
func TestStreamingVerificationRefusesOperationStateThatIsNotGitOperationState(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "unsafe-operation-state.hel.zip")
	archiveInput := input()
	repository := &archiveInput.Repositories[0]
	repository.Metadata.SavedRefs = map[string]string{"refs/heads/feature/hel": strings.Repeat("b", 40)}
	repository.Metadata.GitOperation = &core.SavedGitOperation{Objects: []string{strings.Repeat("c", 40)}}
	repository.OperationPayloads = &GitOperationPayloads{
		ConflictedWorktreeTar: emptyTar(),
		StateTar:              tarWithFile("MERGE_HEAD", []byte(strings.Repeat("c", 40)+"\n"), 0o644),
	}
	manifest, payloads, err := prepareArchive(&archiveInput)
	if err != nil {
		t.Fatal(err)
	}
	replaceTarPayload(t, &manifest, payloads,
		core.PayloadRole{Kind: core.PayloadRoleGitOperationStateTar, RepositoryID: "hel"},
		tarWithFile("hooks/post-checkout", []byte("#!/bin/sh\n"), 0o755), payloadPartBytes)
	archiveWriteZip(t, path, directory, &manifest, payloads)

	_, err = VerifyArchiveStreaming(path)
	archiveAssertErrorContains(t, err, "not Git operation state")
}

// A restore writes the untracked and conflicted tars into the working tree
// and removes the missing unmerged paths there, so none of them may reach
// into the Git directory: a planted hook would run on the restored clone's
// next Git command, and a removed `.git/index` would wreck the clone.
//
// Hard-won: 5a3457a30: restored working-tree paths could write into or remove from `.git`
func TestStreamingVerificationRefusesWorkingTreePathsInsideTheGitDirectory(t *testing.T) {
	directory := t.TempDir()
	for _, tampered := range []string{"untracked", "conflicted", "missing"} {
		archiveInput := input()
		repository := &archiveInput.Repositories[0]
		repository.Metadata.GitOperation = &core.SavedGitOperation{Objects: []string{strings.Repeat("c", 40)}}
		repository.OperationPayloads = &GitOperationPayloads{
			ConflictedWorktreeTar: emptyTar(),
			StateTar:              tarWithFile("MERGE_HEAD", []byte(strings.Repeat("c", 40)+"\n"), 0o644),
		}
		manifest, payloads, err := prepareArchive(&archiveInput)
		if err != nil {
			t.Fatal(err)
		}
		switch tampered {
		case "untracked":
			replaceTarPayload(t, &manifest, payloads,
				core.PayloadRole{Kind: core.PayloadRoleGitUntrackedTar, RepositoryID: "hel"},
				tarWithFile(".git/hooks/post-checkout", []byte("#!/bin/sh\n"), 0o755), payloadPartBytes)
		case "conflicted":
			replaceTarPayload(t, &manifest, payloads,
				core.PayloadRole{Kind: core.PayloadRoleGitConflictedWorktreeTar, RepositoryID: "hel"},
				tarWithFile(".GIT/hooks/post-checkout", []byte("#!/bin/sh\n"), 0o755), payloadPartBytes)
		default:
			index := filepath.Join(".git", "index")
			manifest.Repositories[0].Metadata.GitOperation = &core.SavedGitOperation{
				Unmerged: []core.SavedUnmergedEntry{{
					Path:   index,
					Stage:  2,
					Mode:   "100644",
					Object: strings.Repeat("c", 40),
				}},
				MissingWorktreePaths: []string{index},
				Objects:              []string{strings.Repeat("c", 40)},
			}
		}
		path := filepath.Join(directory, tampered+".hel.zip")
		archiveWriteZip(t, path, directory, &manifest, payloads)

		_, err = VerifyArchiveStreaming(path)
		if err == nil || !strings.Contains(err.Error(), "inside a Git directory") {
			t.Fatalf("%s: %v", tampered, err)
		}
	}
}

func TestShardedManifestsRejectReorderedOrMissingParts(t *testing.T) {
	directory := t.TempDir()
	archiveInput, _, _ := shardedInput()

	manifest, payloads, err := prepareArchiveWithPartSize(&archiveInput, testPartBytes)
	if err != nil {
		t.Fatal(err)
	}
	descriptor := &manifest.Payloads[slices.IndexFunc(manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return len(payload.Parts) > 0
	})]
	descriptor.Parts[0], descriptor.Parts[1] = descriptor.Parts[1], descriptor.Parts[0]
	reordered := filepath.Join(directory, "reordered.hel.zip")
	archiveWriteZip(t, reordered, directory, &manifest, payloads)
	_, err = VerifyArchiveStreaming(reordered)
	archiveAssertErrorContains(t, err, "part 0 has unexpected path")

	manifest, payloads, err = prepareArchiveWithPartSize(&archiveInput, testPartBytes)
	if err != nil {
		t.Fatal(err)
	}
	descriptor = &manifest.Payloads[slices.IndexFunc(manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return len(payload.Parts) > 0
	})]
	descriptor.Parts = descriptor.Parts[:len(descriptor.Parts)-1]
	truncated := filepath.Join(directory, "truncated.hel.zip")
	archiveWriteZip(t, truncated, directory, &manifest, payloads)
	_, err = VerifyArchiveStreaming(truncated)
	archiveAssertErrorContains(t, err, "parts cover")
}

// legacyPayloadDescriptor is the schema-2 payload type as a build that
// predates sharding sees it: unknown fields are refused.
type legacyPayloadDescriptor struct {
	Path   string           `json:"path"`
	Sha256 string           `json:"sha256"`
	Size   uint64           `json:"size"`
	Mode   uint32           `json:"mode"`
	Role   core.PayloadRole `json:"role"`
}

func decodeLegacyManifest(data []byte) (uint32, []legacyPayloadDescriptor, error) {
	var manifest struct {
		SchemaVersion uint32            `json:"schema_version"`
		Payloads      []json.RawMessage `json:"payloads"`
	}
	if err := json.Unmarshal(data, &manifest); err != nil {
		return 0, nil, err
	}
	var payloads []legacyPayloadDescriptor
	for _, raw := range manifest.Payloads {
		decoder := json.NewDecoder(bytes.NewReader(raw))
		decoder.DisallowUnknownFields()
		var payload legacyPayloadDescriptor
		if err := decoder.Decode(&payload); err != nil {
			return 0, nil, err
		}
		payloads = append(payloads, payload)
	}
	return manifest.SchemaVersion, payloads, nil
}

func TestReadersWithoutPartSupportRejectShardedArchives(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "sharded.hel.zip")
	archiveInput, _, _ := shardedInput()
	if err := writeArchiveInstalledWithPartSize(path, &archiveInput, testPartBytes); err != nil {
		t.Fatal(err)
	}
	manifestBytes := []byte(archiveZipEntryText(t, path, manifestPath))

	// Gate 1: the version an old build compares for equality against 2.
	header := archiveJSONValue(t, manifestBytes).(map[string]any)
	if header["schema_version"] != json.Number(strconv.Itoa(int(core.ArchiveSchemaVersionSharded))) {
		t.Fatalf("schema_version = %v", header["schema_version"])
	}

	// Gate 2: even ignoring the version, the old payload type cannot parse a
	// descriptor that carries parts.
	_, _, err := decodeLegacyManifest(manifestBytes)
	archiveAssertErrorContains(t, err, `unknown field "parts"`)

	// Gate 3: an archive that claims schema 2 while carrying parts, or schema
	// 3 while carrying none, is rejected by this build too.
	manifest, payloads, err := prepareArchiveWithPartSize(&archiveInput, testPartBytes)
	if err != nil {
		t.Fatal(err)
	}
	manifest.SchemaVersion = core.ArchiveSchemaVersion
	downgraded := filepath.Join(directory, "downgraded.hel.zip")
	archiveWriteZip(t, downgraded, directory, &manifest, payloads)
	_, err = ReadArchiveVerified(downgraded)
	archiveAssertErrorContains(t, err, "incompatible Mjolnir archive schema 2; this build requires schema 3")

	// An archive with no sharded payload stays schema 2 and still parses with
	// the old wire types, so small sessions keep full compatibility.
	whole := filepath.Join(directory, "whole.hel.zip")
	wholeInput := input()
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(whole, &wholeInput))
	schema, legacyPayloads, err := decodeLegacyManifest([]byte(archiveZipEntryText(t, whole, manifestPath)))
	if err != nil {
		t.Fatal(err)
	}
	if schema != core.ArchiveSchemaVersion {
		t.Fatalf("schema = %d", schema)
	}
	if len(legacyPayloads) == 0 {
		t.Fatal("no payloads")
	}
}

func TestACorruptPartFailsTheParallelReadWithThePartName(t *testing.T) {
	path := filepath.Join(t.TempDir(), "corrupt-part.hel.zip")
	archiveInput, _, _ := shardedInput()
	if err := writeArchiveInstalledWithPartSize(path, &archiveInput, testPartBytes); err != nil {
		t.Fatal(err)
	}

	corrupt := "native/sessions/native-1/rollout.jsonl.helpart.00001"
	archiveOverwriteEntryData(t, path, corrupt, []byte{0xff, 0xff, 0xff, 0xff})

	_, verifyErr := VerifyArchiveStreaming(path)
	_, readErr := ReadArchiveVerified(path)
	for _, err := range []error{verifyErr, readErr} {
		archiveAssertErrorContains(t, err, corrupt)
	}
}

func TestWritingTheSameInputTwiceProducesIdenticalArchives(t *testing.T) {
	directory := t.TempDir()
	archiveInput, _, _ := shardedInput()
	first := filepath.Join(directory, "first.hel.zip")
	second := filepath.Join(directory, "second.hel.zip")
	if err := writeArchiveInstalledWithPartSize(first, &archiveInput, testPartBytes); err != nil {
		t.Fatal(err)
	}
	if err := writeArchiveInstalledWithPartSize(second, &archiveInput, testPartBytes); err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(archiveReadFile(t, first), archiveReadFile(t, second)) {
		t.Fatal("archives differ")
	}
	if !slices.Equal(archiveZipEntryNames(t, first), archiveZipEntryNames(t, second)) {
		t.Fatal("entry names differ")
	}
}

// The manifest an independent clone with saved refs and a stash, and no
// unfinished Git operation, is written with. Archives that carry no operation
// must stay readable by every build that reads clone refs, so this layout may
// not change when operation capture does.
func TestCloneManifestWithoutAGitOperationMatchesGolden(t *testing.T) {
	path := filepath.Join(t.TempDir(), "clone.hel.zip")
	archiveInput := input()
	metadata := &archiveInput.Repositories[0].Metadata
	metadata.SavedRefs = map[string]string{
		"refs/heads/feature/hel": strings.Repeat("b", 40),
		"refs/tags/v1":           strings.Repeat("c", 40),
	}
	metadata.StashStack = []core.SavedStashEntry{{
		Commit:  strings.Repeat("d", 40),
		Message: "On feature/hel: draft",
	}}
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))
	core.AssertGolden(t, "testdata", "clone-archive-manifest", archiveZipEntryText(t, path, manifestPath))
}

func TestStreamingVerificationRejectsNoncanonicalCorruption(t *testing.T) {
	path := filepath.Join(t.TempDir(), "corrupt-native.hel.zip")
	archiveInput := input()
	archiveInput.Repositories = nil
	archiveInput.NativeArtifacts[0].Data = core.NativeArtifactDataBytes("native payload")
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))

	archiveOverwriteEntryData(t, path, "native/sessions/native-1/rollout.jsonl", []byte("X"))

	if _, err := VerifyArchiveStreaming(path); err == nil {
		t.Fatal("a corrupted native payload verified")
	}
}

func TestStreamingVerificationRejectsUnsafeExtraZipEntry(t *testing.T) {
	path := filepath.Join(t.TempDir(), "unsafe-extra.hel.zip")
	archiveInput := input()
	manifest, payloads, err := prepareArchive(&archiveInput)
	if err != nil {
		t.Fatal(err)
	}
	file, err := os.Create(path)
	if err != nil {
		t.Fatal(err)
	}
	writer := zip.NewWriter(file)
	write := func(name string, mode fs.FileMode, data []byte) {
		header := &zip.FileHeader{Name: name, Method: zip.Deflate}
		if mode != 0 {
			header.SetMode(mode)
		}
		entry, err := writer.CreateHeader(header)
		if err == nil {
			_, err = entry.Write(data)
		}
		if err != nil {
			t.Fatal(err)
		}
	}
	write(manifestPath, 0o600, archiveMust[[]byte](t)(archiveSerdeJSONPretty(&manifest)))
	for _, payload := range payloads {
		write(payload.descriptor.Path, fs.FileMode(payload.descriptor.Mode), payload.data)
	}
	write("../escape", 0, []byte("unsafe"))
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	file.Close()

	_, err = VerifyArchiveStreaming(path)
	archiveAssertErrorContains(t, err, "unsafe ZIP entry path")
}

func TestStreamingVerificationParsesUntrackedTarForSafety(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "unsafe-untracked.hel.zip")
	archiveInput := input()
	manifest, payloads, err := prepareArchive(&archiveInput)
	if err != nil {
		t.Fatal(err)
	}
	malicious := tarWithFile(".git/config", []byte("unsafe"), 0o600)
	descriptor := &manifest.Payloads[slices.IndexFunc(manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role.Kind == core.PayloadRoleGitUntrackedTar && payload.Role.RepositoryID == "hel"
	})]
	descriptor.Size = uint64(len(malicious))
	descriptor.Sha256 = digestBytes(malicious)
	payload := &payloads[slices.IndexFunc(payloads, func(payload pendingPayload) bool {
		return payload.descriptor.Path == descriptor.Path
	})]
	payload.descriptor = *descriptor
	payload.data = malicious
	archiveWriteZip(t, path, directory, &manifest, payloads)

	_, err = VerifyArchiveStreaming(path)
	archiveAssertErrorContains(t, err, "inside a Git directory")
}

// Hard-won: 0d7aa0f1e: streaming restore skipped the whole native digest before mutating targets.
func TestCorruptionIsDetectedByPayloadDigest(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "session.hel.zip")
	archiveInput := input()
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))

	archiveOverwriteEntryData(t, path, canonicalSessionPath, []byte("X"))

	if _, err := ReadArchiveVerified(path); err == nil {
		t.Fatal("a corrupted canonical session verified")
	}

	// Keep every part intact but corrupt the whole native digest. Every
	// selective reader must reject it before the caller can mutate a target.
	shardedArchiveInput, _, _ := shardedInput()
	manifest, payloads, err := prepareArchiveWithPartSize(&shardedArchiveInput, testPartBytes)
	if err != nil {
		t.Fatal(err)
	}
	native := &manifest.Payloads[slices.IndexFunc(manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role.Kind == core.PayloadRoleNativeArtifact
	})]
	if len(native.Parts) == 0 {
		t.Fatal("native payload is not sharded")
	}
	native.Sha256 = strings.Repeat("0", 64)
	relative := native.Role.RelativePath
	archiveWriteZip(t, path, directory, &manifest, payloads)
	_, verifyErr := VerifyArchiveStreaming(path)
	_, repositoriesErr := readArchiveRepositories(path)
	for _, err := range []error{verifyErr, repositoriesErr} {
		archiveAssertErrorContains(t, err, "SHA-256 mismatch")
	}
	workspace := filepath.Join(directory, "workspace")
	home := filepath.Join(directory, "harness")
	relay := filepath.Join(directory, "relay")
	nativeFile := filepath.Join(home, filepath.FromSlash(relative))
	if err := os.MkdirAll(workspace, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Dir(nativeFile), 0o777); err != nil {
		t.Fatal(err)
	}
	archiveWriteFile(t, nativeFile, []byte("existing native state"))
	archiveWriteFile(t, filepath.Join(workspace, "keep"), []byte("existing work"))
	for _, restoreNative := range []bool{false, true} {
		err := RestoreCheckpoint(&CheckpointRestoreSpec{
			ArchivePath:         path,
			WorkspaceRoot:       workspace,
			RelayRoot:           relay,
			HarnessHome:         home,
			RestoreRepositories: true,
			RestoreNative:       restoreNative,
			QueuePolicy:         QueueRestorePolicyDiscard,
		}, SystemGit{})
		archiveAssertErrorContains(t, err, "SHA-256 mismatch")
		if got := archiveReadFile(t, nativeFile); string(got) != "existing native state" {
			t.Fatalf("native state changed: %q", got)
		}
		if got := archiveReadFile(t, filepath.Join(workspace, "keep")); string(got) != "existing work" {
			t.Fatalf("workspace changed: %q", got)
		}
		if _, err := os.Stat(relay); !errors.Is(err, fs.ErrNotExist) {
			t.Fatalf("relay root exists: %v", err)
		}
		if _, err := os.Stat(filepath.Join(workspace, "hel")); !errors.Is(err, fs.ErrNotExist) {
			t.Fatalf("repository was restored: %v", err)
		}
	}
}

func TestOldAndFutureSchemasAreRejectedExplicitly(t *testing.T) {
	for _, schemaVersion := range []uint32{1, core.ArchiveSchemaVersion + 1} {
		directory := t.TempDir()
		path := filepath.Join(directory, "session.hel.zip")
		archiveInput := input()
		manifest, payloads, err := prepareArchive(&archiveInput)
		if err != nil {
			t.Fatal(err)
		}
		manifest.SchemaVersion = schemaVersion
		archiveWriteZip(t, path, directory, &manifest, payloads)

		_, err = ReadArchiveVerified(path)
		archiveAssertErrorContains(t, err, fmt.Sprintf(
			"incompatible Mjolnir archive schema %d; this build requires schema %d", schemaVersion, core.ArchiveSchemaVersion))
	}
}

func TestSchemaTwoWireRejectsUnknownAndOmittedRequiredFields(t *testing.T) {
	canonical := archiveJSONObject(t, input().CanonicalSession)
	canonical["revision"] = 4
	var snapshot core.CanonicalSessionSnapshot
	if err := json.Unmarshal(archiveMarshal(t, canonical), &snapshot); err == nil {
		t.Fatal("an unknown canonical session field was accepted")
	}

	canonical = archiveJSONObject(t, input().CanonicalSession)
	delete(canonical["session"].(map[string]any), "configuration")
	if err := json.Unmarshal(archiveMarshal(t, canonical), &snapshot); err == nil {
		t.Fatal("a canonical session without its configuration was accepted")
	}

	target := archiveJSONObject(t, input().Target)
	delete(target, "details")
	var targetManifest core.TargetManifest
	if err := json.Unmarshal(archiveMarshal(t, target), &targetManifest); err == nil {
		t.Fatal("a target without details was accepted")
	}

	metadata := archiveJSONObject(t, repositoryFixture("repo").Metadata)
	delete(metadata, "base_commit")
	var repositoryMetadata core.RepositoryMetadata
	if err := json.Unmarshal(archiveMarshal(t, metadata), &repositoryMetadata); err == nil {
		t.Fatal("repository metadata without a base commit was accepted")
	}
}

func TestRepositoryMetadataDefaultsNetworkFieldsForOlderArchives(t *testing.T) {
	metadata := fmt.Sprintf(`{
		"id": "repo",
		"relative_destination": "repo",
		"origin": "https://github.com/example/repo.git",
		"base_commit": "",
		"head_commit": %q,
		"branch": null
	}`, strings.Repeat("b", 40))
	var parsed core.RepositoryMetadata
	if err := json.Unmarshal([]byte(metadata), &parsed); err != nil {
		t.Fatal(err)
	}
	if len(parsed.PushURLs) != 0 {
		t.Fatalf("push URLs %q", parsed.PushURLs)
	}
	if parsed.RemoteWorkspace {
		t.Fatal("remote workspace defaulted to true")
	}
}

func TestContentMatchingIgnoresTheFrontierAndTheActivityWatermark(t *testing.T) {
	archived := input().CanonicalSession
	latched := archived
	// Checkpoint bookkeeping alone moves the frontier and the watermark.
	latched.EventFrontier += 6
	latched.EventFrontierDigest = strings.Repeat("b", 64)
	latched.Session.LastActivityAtMs = archiveRef(int64(9_999))

	if !archived.ContentMatches(&latched) {
		t.Fatal("archived does not match latched")
	}
	if !latched.ContentMatches(&archived) {
		t.Fatal("latched does not match archived")
	}
}

func TestContentMatchingRejectsNewTranscriptQueueOrTitleContent(t *testing.T) {
	archived := input().CanonicalSession

	extraTranscript := archived
	extraTranscript.Transcript = append(slices.Clone(archived.Transcript), core.CanonicalTranscriptItem{
		StableID:        "user-3",
		Position:        3,
		CreatedAtMs:     105,
		LastChangedAtMs: 105,
		Body: core.CanonicalTranscriptBodyUser{
			Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"again"}`)},
		},
	})
	if archived.ContentMatches(&extraTranscript) {
		t.Fatal("an extra transcript item matched")
	}

	extraPrompt := archived
	extraPrompt.QueuedPrompts = append(slices.Clone(archived.QueuedPrompts), core.CanonicalQueuedPrompt{
		CommandID:  "prompt-5",
		Kind:       core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindPrompt},
		Content:    []json.RawMessage{json.RawMessage(`{"type":"text","text":"later"}`)},
		QueuedAtMs: 105,
	})
	if archived.ContentMatches(&extraPrompt) {
		t.Fatal("an extra queued prompt matched")
	}

	retitled := archived
	retitled.Session.SessionTitle = archiveRef("Reforge Hel")
	if archived.ContentMatches(&retitled) {
		t.Fatal("a new title matched")
	}

	reconfigured := archived
	reconfigured.Session.Configuration.Values = maps.Clone(archived.Session.Configuration.Values)
	reconfigured.Session.Configuration.Values["reasoning_effort"] = "low"
	if archived.ContentMatches(&reconfigured) {
		t.Fatal("a new configuration matched")
	}
}

func archiveAssertNotExists(t testing.TB, path string) {
	t.Helper()
	if _, err := os.Stat(path); !errors.Is(err, fs.ErrNotExist) {
		t.Fatalf("%s exists: %v", path, err)
	}
}

func TestCanonicalSessionRejectsDuplicateOrOutOfFrontierItems(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.hel.zip")
	invalid := input()
	invalid.CanonicalSession.Transcript[1].StableID = "user-1"
	_, err := WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "duplicate canonical transcript item id")

	invalid = input()
	invalid.CanonicalSession.Transcript[1].Position = 5
	_, err = WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "beyond event frontier")
	archiveAssertNotExists(t, path)
}

func TestCanonicalSessionRequiresAValidLatestAgentContentOrdinal(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.hel.zip")
	invalid := input()
	invalid.CanonicalSession.Transcript[1].LatestContentEventOrdinal = nil
	_, err := WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "has no latest content ordinal")

	invalid = input()
	invalid.CanonicalSession.Transcript[1].LatestContentEventOrdinal = archiveRef(uint64(5))
	_, err = WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "invalid latest content ordinal")

	invalid = input()
	invalid.CanonicalSession.Transcript[0].LatestContentEventOrdinal = archiveRef(uint64(1))
	_, err = WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "non-agent transcript item")
}

func TestCanonicalSessionRejectsAStreamStillOpenAtTheBarrier(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.hel.zip")
	invalid := input()
	invalid.CanonicalSession.Transcript[1].Body = core.CanonicalTranscriptBodyAgent{
		Chunks:    []json.RawMessage{json.RawMessage(`{"content":{"type":"text","text":"partial"}}`)},
		Streaming: true,
	}

	_, err := WriteArchiveAtomic(path, &invalid)

	archiveAssertErrorContains(t, err, "still streaming at the checkpoint barrier")
	archiveAssertNotExists(t, path)
}

func TestCanonicalSessionRejectsNonIdleExecutionAtTheBarrier(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.hel.zip")
	invalid := input()
	invalid.CanonicalSession.Session.Execution = core.CanonicalExecutionState{
		State: core.CanonicalExecutionStateRunning, StartedAtMs: 105,
	}

	_, err := WriteArchiveAtomic(path, &invalid)

	archiveAssertErrorContains(t, err, "not idle at the checkpoint barrier")
	archiveAssertNotExists(t, path)
}

func TestCanonicalSessionRejectsAnInvalidEventFrontierDigest(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.hel.zip")
	invalid := input()
	invalid.CanonicalSession.EventFrontierDigest = strings.Repeat("A", 64)

	_, err := WriteArchiveAtomic(path, &invalid)

	archiveAssertErrorContains(t, err, "64 lowercase hexadecimal characters")
	archiveAssertNotExists(t, path)

	invalid = input()
	invalid.CanonicalSession.EventFrontierDigest = core.EventFrontierGenesisDigest
	_, err = WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "frontier and digest are inconsistent")
	archiveAssertNotExists(t, path)
}

func TestCanonicalSessionRejectsUnrestorableQueuedPrompts(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.hel.zip")
	invalid := input()
	invalid.CanonicalSession.QueuedPrompts[0].Content = []json.RawMessage{}

	_, err := WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "has no content")
	archiveAssertNotExists(t, path)

	invalid.CanonicalSession.QueuedPrompts[0].Content = []json.RawMessage{json.RawMessage(`{"type":"not_an_acp_content_block"}`)}
	_, err = WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "has invalid ACP content block 0")
	archiveAssertNotExists(t, path)

	invalid = input()
	invalid.CanonicalSession.QueuedPrompts[0].Kind = core.CanonicalQueuedCommandKind{
		Kind: core.CanonicalQueuedCommandKindSetConfig, Key: "model", Value: "  ",
	}
	_, err = WriteArchiveAtomic(path, &invalid)
	archiveAssertErrorContains(t, err, "is incomplete")
	archiveAssertNotExists(t, path)
}

func TestQueuedEntriesWrittenBeforeConfigChangesLoadAsPrompts(t *testing.T) {
	stored := []byte(`{
		"command_id": "queued-1",
		"content": [{"type": "text", "text": "hello"}],
		"queued_at_ms": 5
	}`)
	var prompt core.CanonicalQueuedPrompt
	if err := json.Unmarshal(stored, &prompt); err != nil {
		t.Fatal(err)
	}
	// Core keeps "prompt" as the kind's zero value, so equality with
	// Rust's Prompt is equality of what the two serialize to.
	archiveAssertSameJSON(t, core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindPrompt}, prompt.Kind)
	// A prompt entry still serializes exactly as it did before.
	if encoded := archiveMarshal(t, prompt); !reflect.DeepEqual(archiveJSONValue(t, encoded), archiveJSONValue(t, stored)) {
		t.Fatalf("prompt serializes as %s", encoded)
	}

	config := core.CanonicalQueuedPrompt{
		CommandID: "queued-2",
		Kind: core.CanonicalQueuedCommandKind{
			Kind: core.CanonicalQueuedCommandKindSetConfig, Key: "model", Value: "sonnet",
		},
		Content:    []json.RawMessage{json.RawMessage(`{"type":"text","text":"/model sonnet"}`)},
		QueuedAtMs: 6,
	}
	var decoded core.CanonicalQueuedPrompt
	if err := json.Unmarshal(archiveMarshal(t, config), &decoded); err != nil {
		t.Fatal(err)
	}
	archiveAssertSameJSON(t, config, decoded)
	if decoded.Kind != config.Kind {
		t.Fatalf("kind = %+v", decoded.Kind)
	}
}

func TestTraversalIsRejectedBeforeDestinationChanges(t *testing.T) {
	path := filepath.Join(t.TempDir(), "session.hel.zip")
	archiveWriteFile(t, path, []byte("existing archive"))
	unsafeInput := input()
	unsafeInput.NativeArtifacts[0].RelativePath = "../auth.json"
	if _, err := WriteArchiveAtomic(path, &unsafeInput); err == nil {
		t.Fatal("a traversing native artifact was archived")
	}
	if got := archiveReadFile(t, path); string(got) != "existing archive" {
		t.Fatalf("destination changed: %q", got)
	}
}

func TestCloseVerificationBlocksTeardownOnArchiveFailure(t *testing.T) {
	unsafeInput := input()
	unsafeInput.Repositories[0].Metadata.Origin = "https://secret@github.com/example/hel.git"
	result := CheckpointForClose(filepath.Join(t.TempDir(), "x.hel.zip"), &unsafeInput)
	if result.TeardownAllowed() {
		t.Fatal("teardown allowed after a failed archive")
	}
	if _, ok := result.(CloseVerificationBlocked); !ok {
		t.Fatalf("result = %#v", result)
	}
}

func TestUntrackedTarPreservesExecutableModeAndSafeSymlink(t *testing.T) {
	source := t.TempDir()
	if err := os.MkdirAll(filepath.Join(source, "scripts"), 0o777); err != nil {
		t.Fatal(err)
	}
	tool := filepath.Join(source, "scripts", "tool")
	archiveWriteFile(t, tool, []byte("tool"))
	paths := []byte("scripts/tool\x00")
	if runtime.GOOS != "windows" {
		if err := os.Chmod(tool, 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.Symlink("tool", filepath.Join(source, "scripts", "current")); err != nil {
			t.Fatal(err)
		}
		paths = []byte("scripts/tool\x00scripts/current\x00")
	}
	tarBytes, err := buildUntrackedTar(source, paths, UntrackedFilesInclude, map[string]struct{}{},
		func(GitSnapshotProgress) error { return nil })
	if err != nil {
		t.Fatal(err)
	}
	if err := validateUntrackedTar(tarBytes); err != nil {
		t.Fatal(err)
	}

	destination := t.TempDir()
	if err := restoreUntrackedTar(destination, tarBytes); err != nil {
		t.Fatal(err)
	}
	if got := archiveReadFile(t, filepath.Join(destination, "scripts", "tool")); string(got) != "tool" {
		t.Fatalf("tool = %q", got)
	}
	if runtime.GOOS != "windows" {
		info, err := os.Stat(filepath.Join(destination, "scripts", "tool"))
		if err != nil {
			t.Fatal(err)
		}
		if info.Mode().Perm() != 0o755 {
			t.Fatalf("tool mode %o", info.Mode().Perm())
		}
		target, err := os.Readlink(filepath.Join(destination, "scripts", "current"))
		if err != nil {
			t.Fatal(err)
		}
		if target != "tool" {
			t.Fatalf("link target %q", target)
		}
	}
}

func TestUntrackedTarProgressCanCancelBeforeOpeningTheNextFile(t *testing.T) {
	source := t.TempDir()
	archiveWriteFile(t, filepath.Join(source, "first.txt"), []byte("first"))
	paths := []byte("first.txt\x00missing.txt\x00")
	var mutex sync.Mutex
	var seen [][2]int

	_, err := buildUntrackedTar(source, paths, UntrackedFilesInclude, map[string]struct{}{},
		func(progress GitSnapshotProgress) error {
			file := progress.(GitSnapshotProgressUntrackedFile)
			mutex.Lock()
			seen = append(seen, [2]int{file.Current, file.Total})
			mutex.Unlock()
			if file.Current >= 2 {
				return errors.New("cancelled by test")
			}
			return nil
		})

	archiveAssertErrorContains(t, err, "cancelled by test")
	if want := [][2]int{{1, 2}, {2, 2}}; !slices.Equal(seen, want) {
		t.Fatalf("progress = %v", seen)
	}
}

func archiveTarEntryPaths(t testing.TB, data []byte) []string {
	t.Helper()
	reader := tar.NewReader(bytes.NewReader(data))
	var paths []string
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			return paths
		}
		if err != nil {
			t.Fatal(err)
		}
		paths = append(paths, header.Name)
	}
}

func TestGitCollectionIsAbstractedAndRedactsOrigin(t *testing.T) {
	repository := t.TempDir()
	archiveWriteFile(t, filepath.Join(repository, "note.txt"), []byte("keep"))
	archiveWriteFile(t, filepath.Join(repository, ".env"), []byte("SECRET=nope"))
	runner := newCollectionGit(0, false)
	snapshot, err := CollectGitSnapshot(runner, repository, &GitCollectionSpec{
		ID:                  "repo",
		RelativeDestination: "repo",
		History:             GitHistoryModeDeltaFrom(strings.Repeat("a", 40)),
	})
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.Metadata.Origin != "https://github.com/example/repo.git" {
		t.Fatalf("origin = %q", snapshot.Metadata.Origin)
	}
	if paths := archiveTarEntryPaths(t, snapshot.UntrackedTar); !slices.Equal(paths, []string{"note.txt", ".env"}) {
		t.Fatalf("untracked paths = %q", paths)
	}
}

func TestGitCollectionBuildsIndependentPayloadsConcurrently(t *testing.T) {
	repository := t.TempDir()
	archiveWriteFile(t, filepath.Join(repository, "note.txt"), []byte("keep"))
	archiveWriteFile(t, filepath.Join(repository, ".env"), []byte("SECRET=nope"))
	runner := newCollectionGit(1, true)

	snapshot, err := CollectGitSnapshot(runner, repository, &GitCollectionSpec{
		ID:                  "repo",
		RelativeDestination: "repo",
		History:             GitHistoryModeDeltaFrom(strings.Repeat("a", 40)),
	})
	if err != nil {
		t.Fatal(err)
	}

	if string(snapshot.CommittedBundle) != "bundle" {
		t.Fatalf("bundle = %q", snapshot.CommittedBundle)
	}
	if string(snapshot.StagedPatch) != "staged" {
		t.Fatalf("staged = %q", snapshot.StagedPatch)
	}
	if string(snapshot.UnstagedPatch) != "unstaged" {
		t.Fatalf("unstaged = %q", snapshot.UnstagedPatch)
	}
}

// Checkpoint work runs with nobody watching the terminal it inherits, so a
// Git child that would ask for a password or a host key has to fail instead
// of holding the checkpoint open until its deadline.
//
// Hard-won: 4e68009d: unattended checkpoint Git could wedge on a credential or host-key prompt.
func TestSystemGitChildrenCannotStopOnAPrompt(t *testing.T) {
	repository := t.TempDir()
	initializeRepository(t, repository)

	// A `!` alias runs through the shell with the environment Git handed its
	// children, so this reports what a real fetch would inherit.
	output, err := SystemGit{}.Run(repository, &GitCommand{
		Arguments: []string{"-c", "alias.helenv=!printenv", "helenv"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("status %d: %s", output.Status, output.Stderr)
	}

	environment := string(output.Stdout)
	value := func(name string) (string, bool) {
		for _, line := range strings.Split(environment, "\n") {
			if rest, ok := strings.CutPrefix(line, name+"="); ok {
				return rest, true
			}
		}
		return "", false
	}
	if got, _ := value("GIT_TERMINAL_PROMPT"); got != "0" {
		t.Fatalf("GIT_TERMINAL_PROMPT = %q", got)
	}
	if got, _ := value("GIT_NO_LAZY_FETCH"); got != "1" {
		t.Fatalf("GIT_NO_LAZY_FETCH = %q", got)
	}
	want, ok := os.LookupEnv("GIT_SSH_COMMAND")
	if !ok {
		want = core.NonInteractiveGitSSHCommand
	}
	// An operator's own SSH command wins; otherwise SSH runs in batch mode.
	if got, ok := value("GIT_SSH_COMMAND"); !ok || got != want {
		t.Fatalf("GIT_SSH_COMMAND = %q, want %q", got, want)
	}
}

func TestMetadataSnapshotRequiresGitHeadButOmitsRepositoryContents(t *testing.T) {
	repository := t.TempDir()
	git(repository, "init", "-q", "-b", "main")
	git(repository, "config", "user.name", "Hel Test")
	git(repository, "config", "user.email", "hel@example.test")
	archiveWriteFile(t, filepath.Join(repository, "tracked.txt"), []byte("base\n"))
	git(repository, "add", ".")
	git(repository, "commit", "-qm", "base")
	archiveWriteFile(t, filepath.Join(repository, "tracked.txt"), []byte("dirty\n"))
	archiveWriteFile(t, filepath.Join(repository, "untracked.txt"), []byte("untracked\n"))
	spec := &GitCollectionSpec{ID: "project", RelativeDestination: "project", History: GitHistoryModeNoBundle{}}

	snapshot, err := CollectGitMetadataSnapshot(SystemGit{}, repository, spec)
	if err != nil {
		t.Fatal(err)
	}

	if snapshot.Metadata.BaseCommit != snapshot.Metadata.HeadCommit {
		t.Fatalf("base %q, head %q", snapshot.Metadata.BaseCommit, snapshot.Metadata.HeadCommit)
	}
	if len(snapshot.CommittedBundle)+len(snapshot.StagedPatch)+len(snapshot.UnstagedPatch)+len(snapshot.UntrackedTar) != 0 {
		t.Fatal("a metadata snapshot carried repository contents")
	}

	if err := os.RemoveAll(filepath.Join(repository, ".git")); err != nil {
		t.Fatal(err)
	}
	_, err = CollectGitMetadataSnapshot(SystemGit{}, repository, spec)
	archiveAssertErrorContains(t, err, "repository has no valid Git HEAD")
}

func TestSessionDeltaBundlesCommitsMissingFromEveryOriginRef(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "base.txt", []byte("base\n"), "base")
	git(origin, "checkout", "-q", "-b", "release")
	commitFile(t, origin, "release.txt", []byte("release\n"), "release")
	git(origin, "checkout", "-q", "main")
	source := cloneRepository(t, directory, origin, "source")
	commitFile(t, source, "first.txt", []byte("first\n"), "first")
	commitFile(t, source, "second.txt", []byte("second\n"), "second")
	head := gitLine(source, "rev-parse", "HEAD")

	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeSessionDelta{},
	})
	if err != nil {
		t.Fatal(err)
	}

	if len(snapshot.CommittedBundle) == 0 {
		t.Fatal("no committed bundle")
	}
	if snapshot.Metadata.BaseCommit != "" {
		t.Fatalf("base %q", snapshot.Metadata.BaseCommit)
	}
	if snapshot.Metadata.HeadCommit != head {
		t.Fatalf("head %q", snapshot.Metadata.HeadCommit)
	}

	destination := cloneRepository(t, directory, origin, "restored")
	if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
		t.Fatal(err)
	}

	if got := gitLine(destination, "rev-parse", "HEAD"); got != head {
		t.Fatalf("HEAD %q", got)
	}
	if got := gitLine(destination, "rev-list", "--count", "HEAD", "--not", "--remotes=origin"); got != "2" {
		t.Fatalf("commits beyond origin: %s", got)
	}
	archiveAssertNotExists(t, filepath.Join(destination, "release.txt"))
}

// archiveXorshiftBytes is count bytes of the xorshift sequence the Rust fixtures
// draw incompressible content from.
func archiveXorshiftBytes(state uint64, count int) []byte {
	output := make([]byte, count)
	for index := range output {
		state ^= state << 13
		state ^= state >> 7
		state ^= state << 17
		output[index] = byte(state)
	}
	return output
}

func TestManagedCloneSnapshotRestoresSecondaryBranchAndFullStashStack(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "tracked.txt", []byte("base\n"), "base")
	base := gitLine(origin, "rev-parse", "HEAD")
	source := cloneRepository(t, directory, origin, "source")
	git(source, "switch", "-q", "-c", "side")
	commitFile(t, source, "side.bin", archiveXorshiftBytes(1, 100_000), "side work")
	sideTip := gitLine(source, "rev-parse", "HEAD")
	git(source, "tag", "-a", "side-tag", "-m", "side tag")
	git(source, "switch", "-q", "main")
	commitFile(t, source, "main.txt", []byte("main work\n"), "main work")
	archiveWriteFile(t, filepath.Join(source, "tracked.txt"), []byte("first stash\n"))
	git(source, "stash", "push", "-q", "-m", "first")
	archiveWriteFile(t, filepath.Join(source, "tracked.txt"), []byte("second stash\n"))
	git(source, "stash", "push", "-q", "-m", "second")
	stashLatest := gitLine(source, "rev-parse", "stash@{0}")
	stashOlder := gitLine(source, "rev-parse", "stash@{1}")
	archiveWriteFile(t, filepath.Join(source, "tracked.txt"), []byte("working change\n"))

	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeCloneFrom(base),
	})
	if err != nil {
		t.Fatal(err)
	}
	var stashCommits []string
	for _, entry := range snapshot.Metadata.StashStack {
		stashCommits = append(stashCommits, entry.Commit)
	}
	if !slices.Equal(stashCommits, []string{stashLatest, stashOlder}) {
		t.Fatalf("stash stack %q", stashCommits)
	}
	if !strings.Contains(snapshot.Metadata.StashStack[0].Message, "second") {
		t.Fatalf("newest stash message %q", snapshot.Metadata.StashStack[0].Message)
	}
	if snapshot.Metadata.SavedRefs["refs/heads/side"] != sideTip {
		t.Fatalf("saved side %q", snapshot.Metadata.SavedRefs["refs/heads/side"])
	}
	if len(snapshot.CommittedBundle) == 0 {
		t.Fatal("no committed bundle")
	}

	destination := cloneRepository(t, directory, origin, "restored")
	if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
		t.Fatal(err)
	}
	for _, arguments := range [][]string{
		{"status", "--porcelain=v1", "-z"},
		{"rev-parse", "HEAD"},
		{"ls-files", "--stage", "-z"},
	} {
		if !bytes.Equal(git(destination, arguments...), git(source, arguments...)) {
			t.Fatalf("%q differs", arguments)
		}
	}
	if got := archiveReadFile(t, filepath.Join(destination, "main.txt")); string(got) != "main work\n" {
		t.Fatalf("main.txt = %q", got)
	}
	if got := gitLine(destination, "rev-parse", "refs/heads/side"); got != sideTip {
		t.Fatalf("side = %q", got)
	}
	archiveAssertNotExists(t, filepath.Join(destination, "side.bin"))
	if got := gitLine(destination, "rev-parse", "stash@{0}"); got != stashLatest {
		t.Fatalf("stash@{0} = %q", got)
	}
	if got := gitLine(destination, "rev-parse", "stash@{1}"); got != stashOlder {
		t.Fatalf("stash@{1} = %q", got)
	}
	if stashList := gitLine(destination, "stash", "list"); !strings.Contains(stashList, "second") || !strings.Contains(stashList, "first") {
		t.Fatalf("stash list %q", stashList)
	}
	if got := archiveReadFile(t, filepath.Join(destination, "tracked.txt")); string(got) != "working change\n" {
		t.Fatalf("tracked.txt = %q", got)
	}
	if gitLine(destination, "rev-parse", "refs/tags/side-tag") != gitLine(source, "rev-parse", "refs/tags/side-tag") {
		t.Fatal("side-tag differs")
	}
}

// mainScope is the scope name of a main worktree's session-start refs, and
// nonCommitTip a private ref that names a bare tree or a tag rather than a
// commit.
const (
	mainScope    = "refs/mj/review-baseline"
	nonCommitTip = "refs/mj/non-commit-tip"
)

// Hard-won: 0fefbee48: review refs disappeared when a checkpoint moved its checkout.
func TestReviewRefsRoundTripWithNonCommitTipsAndRestoreConflicts(t *testing.T) {
	for _, metadataOnly := range []bool{false, true} {
		for _, tagged := range []bool{false, true} {
			directory := t.TempDir()
			origin := filepath.Join(directory, "origin")
			initializeRepository(t, origin)
			commitFile(t, origin, "tracked", []byte("base\n"), "base")
			head := gitLine(origin, "rev-parse", "HEAD")
			source := cloneRepository(t, directory, origin, "source")
			archiveWriteFile(t, filepath.Join(source, "tracked"), []byte("baseline only\n"))
			tree, err := CaptureWorktreeTree(SystemGit{}, source)
			if err != nil {
				t.Fatal(err)
			}
			git(source, "checkout", "--", "tracked")
			point := tree
			if tagged {
				point, err = RecordReviewPoint(SystemGit{}, source, &ReviewPoint{
					Tree: tree, Head: archiveRef(head), Session: archiveRef("moving"),
				})
				if err != nil {
					t.Fatal(err)
				}
			}
			reviewed := "refs/mj/reviewed/" + gitLine(source, "rev-parse", "HEAD^{tree}")
			overridden := strings.Replace(reviewed, "refs/mj/reviewed/", "refs/mj/overridden/", 1)
			start := fmt.Sprintf("refs/mj/session-start/%s/%s",
				digestBytes([]byte("moving")), digestBytes([]byte(mainScope)))
			refs := [][2]string{{reviewed, head}, {overridden, head}, {start, head}, {nonCommitTip, point}}
			for _, ref := range refs {
				git(source, "update-ref", ref[0], ref[1])
			}
			spec := &GitCollectionSpec{ID: "repo", RelativeDestination: "repo", History: GitHistoryModeCloneFrom(head)}
			var snapshot RepositorySnapshot
			if metadataOnly {
				snapshot, err = CollectGitMetadataSnapshot(SystemGit{}, source, spec)
			} else {
				snapshot, err = CollectGitSnapshot(SystemGit{}, source, spec)
			}
			if err != nil {
				t.Fatal(err)
			}
			if (len(snapshot.CommittedBundle) == 0) != metadataOnly {
				t.Fatalf("bundle of %d bytes, metadata only %v", len(snapshot.CommittedBundle), metadataOnly)
			}
			destination := source
			if metadataOnly {
				for name := range snapshot.Metadata.SavedRefs {
					git(source, "update-ref", "-d", name)
				}
			} else {
				destination = cloneRepository(t, directory, origin, "restored")
			}
			if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
				t.Fatal(err)
			}
			for _, ref := range refs {
				if got := gitLine(destination, "rev-parse", ref[0]); got != ref[1] {
					t.Fatalf("%s = %q, want %q", ref[0], got, ref[1])
				}
			}
			if got := git(destination, "show", tree+":tracked"); string(got) != "baseline only\n" {
				t.Fatalf("tree content %q", got)
			}
			// A reviewed tree may be recorded at a different equivalent commit;
			// another session's start must remain untouched during restore.
			git(destination, "commit", "--allow-empty", "-qm", "equivalent reviewed tree")
			later := gitLine(destination, "rev-parse", "HEAD")
			git(destination, "update-ref", reviewed, later)
			git(destination, "update-ref", overridden, later)
			git(destination, "update-ref", start, later)
			otherStart := fmt.Sprintf("refs/mj/session-start/%s/other", digestBytes([]byte("other")))
			git(destination, "update-ref", otherStart, later)
			if tagged {
				newer, err := RecordReviewPoint(SystemGit{}, destination, &ReviewPoint{
					Tree: tree, Head: archiveRef(later), Session: archiveRef("moving"),
				})
				if err != nil {
					t.Fatal(err)
				}
				git(destination, "update-ref", nonCommitTip, newer)
			}
			conflicting := snapshot
			conflicting.Metadata.SavedRefs = maps.Clone(snapshot.Metadata.SavedRefs)
			conflicting.Metadata.SavedRefs[otherStart] = head
			if _, err := restoreGitSnapshotForSession(SystemGit{}, destination, &conflicting, archiveRef("moving"), true); err != nil {
				t.Fatal(err)
			}
			for _, ref := range [][2]string{{reviewed, later}, {overridden, later}, {start, head}, {otherStart, later}} {
				if got := gitLine(destination, "rev-parse", ref[0]); got != ref[1] {
					t.Fatalf("%s = %q, want %q", ref[0], got, ref[1])
				}
			}
			// A tip the destination already holds is not replaced.
			tip := gitLine(destination, "rev-parse", nonCommitTip)
			if (tip == point) != !tagged {
				t.Fatalf("tip %s, point %s, tagged %v", tip, point, tagged)
			}
		}
	}
}

func TestSessionStartsAndExplicitReviewBasesFollowWorktreeNames(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "tracked", []byte("base\n"), "base")
	base := gitLine(origin, "rev-parse", "HEAD")
	linked := filepath.Join(directory, "linked")
	git(origin, "worktree", "add", "-q", "-b", "session", linked)
	scope, err := SessionStartScope(SystemGit{}, linked)
	if err != nil {
		t.Fatal(err)
	}
	if scope != "refs/mj/worktrees/linked/review-baseline" {
		t.Fatalf("scope %q", scope)
	}
	start := fmt.Sprintf("refs/mj/session-start/%s/%s", digestBytes([]byte("moving")), digestBytes([]byte(scope)))
	git(linked, "update-ref", start, base)
	commitFile(t, linked, "reviewed.txt", []byte("reviewed\n"), "reviewed")
	reviewed := gitLine(linked, "rev-parse", "HEAD")
	git(linked, "update-ref", PushReviewBaseRef(scope), reviewed)
	snapshot, err := CollectGitSnapshot(SystemGit{}, linked, &GitCollectionSpec{
		ID:                  "repo",
		RelativeDestination: "repo",
		History:             GitHistoryModeCloneFrom(base),
		OriginOverride:      archiveRef(origin),
	})
	if err != nil {
		t.Fatal(err)
	}
	if snapshot.Metadata.ReviewBaselineRef == nil || *snapshot.Metadata.ReviewBaselineRef != scope {
		t.Fatalf("review baseline ref %v", snapshot.Metadata.ReviewBaselineRef)
	}
	destination := cloneRepository(t, directory, origin, "destination")
	if _, err := restoreGitSnapshotForSession(SystemGit{}, destination, &snapshot, archiveRef("moving"), false); err != nil {
		t.Fatal(err)
	}
	relocatedStart := fmt.Sprintf("refs/mj/session-start/%s/%s", digestBytes([]byte("moving")), digestBytes([]byte(mainScope)))
	if got := gitLine(destination, "rev-parse", relocatedStart); got != base {
		t.Fatalf("relocated start %q", got)
	}
	relocatedBase := PushReviewBaseRef(mainScope)
	if got := gitLine(destination, "rev-parse", relocatedBase); got != reviewed {
		t.Fatalf("relocated base %q", got)
	}
	// A selection already made in the destination belongs to that worktree.
	git(destination, "update-ref", relocatedBase, base)
	if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
		t.Fatal(err)
	}
	if got := gitLine(destination, "rev-parse", relocatedBase); got != base {
		t.Fatalf("relocated base %q after restore", got)
	}
}

// Hard-won: 38f041c03: a metadata-only capture whose only refs/mj ref was a dropped pin restored as a full snapshot.
func TestAMetadataOnlyCaptureWithoutReviewRefsNeverRestoresContents(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "tracked", []byte("base\n"), "base")
	source := cloneRepository(t, directory, origin, "source")
	captured := gitLine(source, "rev-parse", "HEAD")
	tree := gitLine(source, "rev-parse", "HEAD^{tree}")
	git(source, "update-ref", mainScope, tree)
	snapshot, err := CollectGitMetadataSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeNoBundle{},
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.Metadata.SavedRefs) != 0 {
		t.Fatalf("saved refs %v", snapshot.Metadata.SavedRefs)
	}
	if !snapshot.Metadata.ReviewRefsMetadataOnly {
		t.Fatal("not metadata only")
	}

	// The user keeps working; a restore with contents must not move them back.
	commitFile(t, source, "tracked", []byte("later\n"), "later")
	later := gitLine(source, "rev-parse", "HEAD")
	if later == captured {
		t.Fatal("HEAD did not move")
	}
	if err := RestoreGitSnapshot(SystemGit{}, source, &snapshot); err != nil {
		t.Fatal(err)
	}
	if got := gitLine(source, "rev-parse", "HEAD"); got != later {
		t.Fatalf("HEAD %q", got)
	}
	if got := archiveReadFile(t, filepath.Join(source, "tracked")); string(got) != "later\n" {
		t.Fatalf("tracked = %q", got)
	}
}

// Hard-won: 0c78ec89f: capture and restore migrated, copied or wrote review baseline pins in the user's repository.
func TestCaptureAndRestoreLeaveARepositorysOldReviewPinsAlone(t *testing.T) {
	for _, metadataOnly := range []bool{false, true} {
		directory := t.TempDir()
		origin := filepath.Join(directory, "origin")
		initializeRepository(t, origin)
		commitFile(t, origin, "tracked", []byte("base\n"), "base")
		head := gitLine(origin, "rev-parse", "HEAD")
		source := cloneRepository(t, directory, origin, "source")
		linked := filepath.Join(directory, "linked")
		git(source, "worktree", "add", "-q", "-b", "session", linked)
		point := func(session string) string {
			id, err := RecordReviewPoint(SystemGit{}, source, &ReviewPoint{
				Tree:    gitLine(source, "rev-parse", "HEAD^{tree}"),
				Head:    archiveRef(head),
				Session: archiveRef(session),
			})
			if err != nil {
				t.Fatal(err)
			}
			return id
		}
		current := point("moving")
		legacy := point("legacy")
		start := fmt.Sprintf("refs/mj/session-start/%s/%s", digestBytes([]byte("moving")), digestBytes([]byte(mainScope)))
		// The pins an older release left, under both names, and a start that
		// is live state.
		for _, ref := range [][2]string{
			{mainScope, current},
			{"refs/mj/worktrees/linked/review-baseline", current},
			{"refs/hel/review-baseline", legacy},
			{"refs/hel/worktrees/linked/review-baseline", legacy},
			{start, head},
		} {
			git(source, "update-ref", ref[0], ref[1])
		}
		refs := func(repository string) string {
			return gitLine(repository, "for-each-ref", "--format=%(refname) %(objectname)")
		}
		beforeCapture := refs(source)
		spec := &GitCollectionSpec{ID: "repo", RelativeDestination: "repo", History: GitHistoryModeCloneFrom(head)}

		var snapshot RepositorySnapshot
		var err error
		if metadataOnly {
			snapshot, err = CollectGitMetadataSnapshot(SystemGit{}, source, spec)
		} else {
			snapshot, err = CollectGitSnapshot(SystemGit{}, source, spec)
		}
		if err != nil {
			t.Fatal(err)
		}

		if refs(source) != beforeCapture {
			t.Fatal("capture wrote to the repository")
		}
		if _, ok := snapshot.Metadata.SavedRefs[start]; !ok {
			t.Fatalf("start not saved: %v", snapshot.Metadata.SavedRefs)
		}
		for name := range snapshot.Metadata.SavedRefs {
			if strings.Contains(name, "review-baseline") {
				t.Fatalf("a pin was carried: %v", snapshot.Metadata.SavedRefs)
			}
		}
		destination := source
		if !metadataOnly {
			destination = cloneRepository(t, directory, origin, "restored")
		}
		beforeRestore := refs(destination)
		if _, err := restoreGitSnapshotForSession(SystemGit{}, destination, &snapshot, archiveRef("moving"), true); err != nil {
			t.Fatal(err)
		}
		afterRestore := refs(destination)
		if metadataOnly {
			if afterRestore != beforeRestore {
				t.Fatal("restore wrote to the repository")
			}
		} else {
			for _, line := range strings.Split(afterRestore, "\n") {
				if strings.Contains(line, "review-baseline") {
					t.Fatalf("restore wrote a pin into the destination: %s", line)
				}
			}
			if got := gitLine(destination, "rev-parse", start); got != head {
				t.Fatalf("start %q", got)
			}
		}
	}
}

// Hard-won: 0c78ec89f: an old checkpoint that carried a review baseline pin must still restore without writing it.
func TestAnOldCheckpointCarryingAReviewBaselinePinRestoresWithoutIt(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "tracked", []byte("base\n"), "base")
	head := gitLine(origin, "rev-parse", "HEAD")
	source := cloneRepository(t, directory, origin, "source")
	start := fmt.Sprintf("refs/mj/session-start/%s/%s", digestBytes([]byte("moving")), digestBytes([]byte(mainScope)))
	git(source, "update-ref", start, head)
	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeCloneFrom(head),
	})
	if err != nil {
		t.Fatal(err)
	}
	// What an older release wrote: the pin among the saved refs, whose object
	// may have been garbage-collected since.
	missing := strings.Repeat("0", 40)
	if snapshot.Metadata.SavedRefs == nil {
		snapshot.Metadata.SavedRefs = map[string]string{}
	}
	snapshot.Metadata.SavedRefs[mainScope] = missing
	snapshot.Metadata.SavedRefs["refs/mj/worktrees/linked/review-baseline"] = missing
	snapshot.Metadata.ReviewBaselineRef = archiveRef(mainScope)

	destination := cloneRepository(t, directory, origin, "restored")
	if _, err := restoreGitSnapshotForSession(SystemGit{}, destination, &snapshot, archiveRef("moving"), true); err != nil {
		t.Fatal(err)
	}

	if got := gitLine(destination, "rev-parse", start); got != head {
		t.Fatalf("start %q", got)
	}
	if got := gitLine(destination, "for-each-ref", "refs/mj/review-baseline", "refs/mj/worktrees"); got != "" {
		t.Fatalf("pins written: %q", got)
	}
}

// Hard-won: 7414cc74: updating the checked-out branch first staged added files as deletions.
func TestManagedCloneRestoreMatchesHeadIndexAndWorktree(t *testing.T) {
	for _, detached := range []bool{false, true} {
		for _, dirty := range []bool{false, true} {
			directory := t.TempDir()
			origin := filepath.Join(directory, "origin")
			initializeRepository(t, origin)
			commitFile(t, origin, "tracked", []byte("base\n"), "base")
			commitFile(t, origin, "deleted", []byte("remove me\n"), "old file")
			base := gitLine(origin, "rev-parse", "HEAD")
			source := cloneRepository(t, directory, origin, "source")
			commitFile(t, source, "added", []byte("new committed file\n"), "add file")
			git(source, "rm", "deleted")
			git(source, "commit", "-qm", "delete file")
			if detached {
				git(source, "checkout", "--detach")
			}
			if dirty {
				archiveWriteFile(t, filepath.Join(source, "tracked"), []byte("staged\n"))
				git(source, "add", "tracked")
				archiveWriteFile(t, filepath.Join(source, "tracked"), []byte("unstaged\n"))
				archiveWriteFile(t, filepath.Join(source, "untracked"), []byte("untracked\n"))
			}
			snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
				ID: "repo", RelativeDestination: "repo", History: GitHistoryModeCloneFrom(base),
			})
			if err != nil {
				t.Fatal(err)
			}
			destination := cloneRepository(t, directory, origin, "destination")
			if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
				t.Fatal(err)
			}
			for _, arguments := range [][]string{
				{"rev-parse", "HEAD"},
				{"status", "--porcelain=v1", "-z"},
				{"ls-files", "--stage", "-z"},
				{"diff", "--binary"},
				{"diff", "--cached", "--binary"},
				{"rev-parse", "--abbrev-ref", "HEAD"},
			} {
				if !bytes.Equal(git(destination, arguments...), git(source, arguments...)) {
					t.Fatalf("%q, detached=%v, dirty=%v", arguments, detached, dirty)
				}
			}
			for _, path := range []string{"tracked", "added"} {
				if !bytes.Equal(archiveReadFile(t, filepath.Join(destination, path)), archiveReadFile(t, filepath.Join(source, path))) {
					t.Fatalf("%s differs", path)
				}
			}
			archiveAssertNotExists(t, filepath.Join(destination, "deleted"))
			if dirty {
				if got := archiveReadFile(t, filepath.Join(destination, "untracked")); string(got) != "untracked\n" {
					t.Fatalf("untracked = %q", got)
				}
			}
		}
	}
}

// Provisioning must fetch a local repository's history before restoring into
// it: the delta bundle carries only the commits origin lacks.
func TestSessionDeltaRestoreRequiresTheFetchedOriginHistory(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "base.txt", []byte("base\n"), "base")
	source := cloneRepository(t, directory, origin, "source")
	commitFile(t, source, "session.txt", []byte("session\n"), "session work")
	head := gitLine(source, "rev-parse", "HEAD")
	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeSessionDelta{},
	})
	if err != nil {
		t.Fatal(err)
	}

	destination := filepath.Join(directory, "target")
	initializeRepository(t, destination)
	err = RestoreGitSnapshot(SystemGit{}, destination, &snapshot)
	archiveAssertErrorContains(t, err, "import committed delta bundle")

	git(destination, "remote", "add", "origin", origin)
	git(destination, "fetch", "-q", "origin")
	if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
		t.Fatal(err)
	}

	if got := gitLine(destination, "rev-parse", "HEAD"); got != head {
		t.Fatalf("HEAD %q", got)
	}
	if got := archiveReadFile(t, filepath.Join(destination, "session.txt")); string(got) != "session\n" {
		t.Fatalf("session.txt = %q", got)
	}
}

func TestSessionDeltaErrorsWithoutOriginRefs(t *testing.T) {
	source := t.TempDir()
	initializeRepository(t, source)
	commitFile(t, source, "base.txt", []byte("base\n"), "base")

	_, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeSessionDelta{},
	})

	archiveAssertErrorContains(t, err, "repository has no origin refs to delta against")
}

func TestDeltaFromErrorsWhenTheBaseIsUnresolvable(t *testing.T) {
	source := t.TempDir()
	initializeRepository(t, source)
	commitFile(t, source, "old.txt", []byte("old\n"), "old root")

	_, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeDeltaFrom("refs/hel/missing"),
	})
	archiveAssertErrorContains(t, err, "delta base refs/hel/missing is unresolvable")

	oldHead := gitLine(source, "rev-parse", "HEAD")
	git(source, "checkout", "-q", "--orphan", "rewritten")
	git(source, "rm", "-q", "-rf", ".")
	commitFile(t, source, "new.txt", []byte("new root\n"), "new root")
	_, err = CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeDeltaFrom(oldHead),
	})
	archiveAssertErrorContains(t, err, fmt.Sprintf("delta base %s is unresolvable", oldHead))
}

func TestNoBundleSnapshotCarriesDirtyStateWithoutCommittedHistory(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "tracked.txt", []byte("base\n"), "base")
	commitFile(t, origin, "dirty.txt", []byte("clean\n"), "clean")
	source := cloneRepository(t, directory, origin, "source")
	archiveWriteFile(t, filepath.Join(source, "staged.txt"), []byte("staged\n"))
	git(source, "add", "staged.txt")
	archiveWriteFile(t, filepath.Join(source, "dirty.txt"), []byte("dirty\n"))
	archiveWriteFile(t, filepath.Join(source, "new.txt"), []byte("untracked\n"))
	head := gitLine(source, "rev-parse", "HEAD")

	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID:                  "repo",
		RelativeDestination: "repo",
		History:             GitHistoryModeNoBundle{},
		OriginOverride:      archiveRef("mj-local:repo"),
	})
	if err != nil {
		t.Fatal(err)
	}

	if len(snapshot.CommittedBundle) != 0 {
		t.Fatal("a committed bundle was captured")
	}
	if snapshot.Metadata.Origin != "mj-local:repo" {
		t.Fatalf("origin %q", snapshot.Metadata.Origin)
	}
	if snapshot.Metadata.BaseCommit != head {
		t.Fatalf("base %q", snapshot.Metadata.BaseCommit)
	}
	if len(snapshot.StagedPatch) == 0 || len(snapshot.UnstagedPatch) == 0 || len(snapshot.UntrackedTar) == 0 {
		t.Fatal("dirty state is missing")
	}

	destination := cloneRepository(t, directory, origin, "restored")
	git(destination, "checkout", "-q", "--detach", "HEAD")
	if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
		t.Fatal(err)
	}

	if got := gitLine(destination, "rev-parse", "HEAD"); got != head {
		t.Fatalf("HEAD %q", got)
	}
	if got := gitLine(destination, "symbolic-ref", "--short", "HEAD"); got != "main" {
		t.Fatalf("branch %q", got)
	}
	if got := archiveReadFile(t, filepath.Join(destination, "dirty.txt")); string(got) != "dirty\n" {
		t.Fatalf("dirty.txt = %q", got)
	}
	if got := archiveReadFile(t, filepath.Join(destination, "new.txt")); string(got) != "untracked\n" {
		t.Fatalf("new.txt = %q", got)
	}
	status := string(git(destination, "status", "--short"))
	for _, line := range []string{"A  staged.txt", " M dirty.txt", "?? new.txt"} {
		if !strings.Contains(status, line) {
			t.Fatalf("%q missing from %s", line, status)
		}
	}
}

func TestRestoreWithoutABundleReportsAnUnreachableCommitActionably(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "base.txt", []byte("base\n"), "base")
	source := cloneRepository(t, directory, origin, "source")
	commitFile(t, source, "local.txt", []byte("local\n"), "local only")
	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeNoBundle{},
	})
	if err != nil {
		t.Fatal(err)
	}

	destination := cloneRepository(t, directory, origin, "restored")
	err = RestoreGitSnapshot(SystemGit{}, destination, &snapshot)

	archiveAssertErrorContains(t, err, "must be reachable from the repository's origin")
}

// Hard-won: #1384: a registered nested worktree failed with an unidentified untracked file type.
func TestNestedWorktreeCheckpointNamesTheDirectoryAndExplicitScratchExclusion(t *testing.T) {
	source := t.TempDir()
	initializeRepository(t, source)
	commitFile(t, source, "tracked.txt", []byte("base\n"), "base")
	base := gitLine(source, "rev-parse", "HEAD")
	// An independent clone is a different repository: its work cannot travel
	// in this one's snapshot. (A linked worktree of this repository can.)
	nested := ".mj/csharp-parity/bin-src"
	git(source, "clone", "--no-hardlinks", ".", nested)
	archiveWriteFile(t, filepath.Join(source, filepath.FromSlash(nested), "valuable.txt"), []byte("nested work\n"))
	spec := &GitCollectionSpec{ID: "repo", RelativeDestination: "repo", History: GitHistoryModeDeltaFrom(base)}
	_, err := CollectGitSnapshot(SystemGit{}, source, spec)
	if err == nil {
		t.Fatal("a nested repository was checkpointed")
	}
	message := err.Error()
	for _, text := range []string{
		"nested Git repository directory",
		filepath.Join(source, filepath.FromSlash(nested)),
		".git/info/exclude",
		"preserve its work separately",
	} {
		if !strings.Contains(message, text) {
			t.Fatalf("%q missing from %s", text, message)
		}
	}
	archiveWriteFile(t, filepath.Join(source, ".git", "info", "exclude"), []byte("/"+nested+"/\n"))
	snapshot, err := CollectGitSnapshot(SystemGit{}, source, spec)
	if err != nil {
		t.Fatal(err)
	}
	if paths := archiveTarEntryPaths(t, snapshot.UntrackedTar); len(paths) != 0 {
		t.Fatalf("untracked paths %q", paths)
	}
	if got := archiveReadFile(t, filepath.Join(source, filepath.FromSlash(nested), "valuable.txt")); string(got) != "nested work\n" {
		t.Fatalf("nested work = %q", got)
	}
}

// Hard-won: b63fedbf31: checkpoint filename filtering silently discarded untracked files.
func TestSystemGitRoundTripRestoresCommitsIndexWorktreeAndUntracked(t *testing.T) {
	source := t.TempDir()
	git(source, "init", "-q", "-b", "main")
	git(source, "config", "user.name", "Hel Test")
	git(source, "config", "user.email", "hel@example.test")
	// Exercise every fixture file regardless of the host's global ignore list.
	git(source, "config", "core.excludesFile", "")
	git(source, "remote", "add", "origin", "https://github.com/example/repo.git")
	archiveWriteFile(t, filepath.Join(source, "tracked.txt"), []byte("base\n"))
	archiveWriteFile(t, filepath.Join(source, "dirty.txt"), []byte("clean\n"))
	git(source, "add", ".")
	git(source, "commit", "-qm", "base")
	base := strings.TrimSpace(string(git(source, "rev-parse", "HEAD")))
	archiveWriteFile(t, filepath.Join(source, "tracked.txt"), []byte("committed\n"))
	git(source, "commit", "-qam", "delta")
	archiveWriteFile(t, filepath.Join(source, "staged.txt"), []byte("staged\n"))
	git(source, "add", "staged.txt")
	archiveWriteFile(t, filepath.Join(source, "dirty.txt"), []byte("dirty\n"))
	archiveWriteFile(t, filepath.Join(source, "new.txt"), []byte("untracked\n"))
	configurationFiles := []string{"config.json", ".env", "auth.toml", "credentials/notes.txt"}
	if err := os.MkdirAll(filepath.Join(source, "credentials"), 0o777); err != nil {
		t.Fatal(err)
	}
	for _, relative := range configurationFiles {
		archiveWriteFile(t, filepath.Join(source, filepath.FromSlash(relative)), []byte(relative))
	}

	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeDeltaFrom(base),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.CommittedBundle) == 0 {
		t.Fatal("no committed bundle")
	}
	if snapshot.Metadata.BaseCommit != base {
		t.Fatalf("base %q", snapshot.Metadata.BaseCommit)
	}

	destinationParent := t.TempDir()
	destination := filepath.Join(destinationParent, "restore")
	git(destinationParent, "clone", "-q", source, destination)
	git(destination, "reset", "--hard", base)
	if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
		t.Fatal(err)
	}

	for path, want := range map[string]string{
		"tracked.txt": "committed\n",
		"dirty.txt":   "dirty\n",
		"new.txt":     "untracked\n",
	} {
		if got := archiveReadFile(t, filepath.Join(destination, path)); string(got) != want {
			t.Fatalf("%s = %q", path, got)
		}
	}
	status := string(git(destination, "status", "--short"))
	for _, line := range []string{"A  staged.txt", " M dirty.txt", "?? new.txt"} {
		if !strings.Contains(status, line) {
			t.Fatalf("%q missing from %s", line, status)
		}
	}
	for _, relative := range configurationFiles {
		if got := archiveReadFile(t, filepath.Join(destination, filepath.FromSlash(relative))); string(got) != relative {
			t.Fatalf("untracked file %s was not preserved", relative)
		}
	}
}

// Hard-won: #1542: adding base retention must preserve pinless legacy archive restores.
func TestRemoteWorkspaceRestoreKeepsPublishedHistoryPushURLsAndDirtyState(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "base.txt", []byte("base\n"), "base")
	commitFile(t, origin, "dirty.txt", []byte("initial\n"), "track dirty fixture")
	base := gitLine(origin, "rev-parse", "HEAD")
	upstream := filepath.Join(directory, "upstream.git")
	git(directory, "init", "--bare", "-q", upstream)
	source := cloneRepository(t, directory, origin, "source")
	git(source, "remote", "set-url", "--add", "--push", "origin", upstream)
	git(source, "switch", "-q", "-c", "mj/remote-session")
	git(source, "config", "--local", "mj.remoteWorkspace", "true")
	git(source, "config", "--local", "mj.baseCommit", base)
	commitFile(t, source, "session.txt", []byte("published session\n"), "session work")
	head := gitLine(source, "rev-parse", "HEAD")
	git(source, "push", "-q", "origin", "HEAD:refs/heads/mj/remote-session")
	// A fetch or push may advance an origin-tracking ref after publication.
	// The managed checkpoint still measures from its immutable launch base.
	git(source, "update-ref", "refs/remotes/origin/main", head)

	// The transport fixtures above are local; archived managed provenance
	// uses the network destinations a real provisioned workspace carries.
	git(source, "remote", "set-url", "origin", "https://fetch.example.test/repo.git")
	git(source, "config", "--replace-all", "remote.origin.pushurl", "https://push.example.test/repo.git")
	large := bytes.Repeat([]byte("x"), 128*1024)
	archiveWriteFile(t, filepath.Join(source, "staged.txt"), large)
	git(source, "add", "staged.txt")
	archiveWriteFile(t, filepath.Join(source, "dirty.txt"), large)
	archiveWriteFile(t, filepath.Join(source, "untracked.txt"), large)

	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeDeltaFrom(base),
	})
	if err != nil {
		t.Fatal(err)
	}
	snapshot.Metadata.RemoteWorkspace = true
	if snapshot.Metadata.BaseCommit != base {
		t.Fatalf("base %q", snapshot.Metadata.BaseCommit)
	}
	if !slices.Equal(snapshot.Metadata.PushURLs, []string{"https://push.example.test/repo.git"}) {
		t.Fatalf("push URLs %q", snapshot.Metadata.PushURLs)
	}
	if len(snapshot.CommittedBundle) == 0 {
		t.Fatal("no committed bundle")
	}
	for name, payload := range map[string][]byte{
		"staged": snapshot.StagedPatch, "unstaged": snapshot.UnstagedPatch, "untracked": snapshot.UntrackedTar,
	} {
		if len(payload) <= 64*1024 {
			t.Fatalf("%s payload is only %d bytes", name, len(payload))
		}
	}

	destination := cloneRepository(t, directory, origin, "restored")
	if err := RestoreGitSnapshot(SystemGit{}, destination, &snapshot); err != nil {
		t.Fatal(err)
	}
	for _, check := range [][]string{
		{head, "rev-parse", "HEAD"},
		{"https://fetch.example.test/repo.git", "config", "remote.origin.url"},
		{"https://push.example.test/repo.git", "config", "--get-all", "remote.origin.pushurl"},
		{"true", "config", "mj.remoteWorkspace"},
		{base, "config", "mj.baseCommit"},
		{base, "rev-parse", core.HistoryCutoffBaseRef},
		{"current", "config", "push.default"},
		{"true", "config", "push.autoSetupRemote"},
	} {
		if got := gitLine(destination, check[1:]...); got != check[0] {
			t.Fatalf("git %q = %q, want %q", check[1:], got, check[0])
		}
	}
	for _, path := range []string{"staged.txt", "dirty.txt", "untracked.txt"} {
		if !bytes.Equal(archiveReadFile(t, filepath.Join(destination, path)), large) {
			t.Fatalf("%s differs", path)
		}
	}
	remoteBase, err := RemoteWorkspaceBase(SystemGit{}, destination)
	if err != nil {
		t.Fatal(err)
	}
	if remoteBase == nil || *remoteBase != base {
		t.Fatalf("remote workspace base %v", remoteBase)
	}
	repeated, err := CollectGitSnapshot(SystemGit{}, destination, &GitCollectionSpec{
		ID: "repo", RelativeDestination: "repo", History: GitHistoryModeDeltaFrom(base),
	})
	if err != nil {
		t.Fatal(err)
	}
	if repeated.Metadata.HeadCommit != head {
		t.Fatalf("repeated head %q", repeated.Metadata.HeadCommit)
	}
	if len(repeated.CommittedBundle) == 0 {
		t.Fatal("no repeated bundle")
	}
	if !bytes.Equal(repeated.StagedPatch, snapshot.StagedPatch) || !bytes.Equal(repeated.UnstagedPatch, snapshot.UnstagedPatch) {
		t.Fatal("repeated patches differ")
	}
	// A pre-retention archive can restore HEAD while its unrelated launch
	// commit is no longer available at origin or in the archived delta.
	abandoned := cloneRepository(t, directory, origin, "old-launch")
	commitFile(t, abandoned, "abandoned.txt", []byte("old launch\n"), "abandoned launch base")
	missingBase := gitLine(abandoned, "rev-parse", "HEAD")
	legacySnapshot := snapshot
	legacySnapshot.Metadata.BaseCommit = missingBase
	if len(legacySnapshot.Metadata.SavedRefs) != 0 {
		t.Fatalf("saved refs %v", legacySnapshot.Metadata.SavedRefs)
	}
	legacyInput := input()
	legacyInput.Bundle.PrimaryRepository = "repo"
	legacyInput.Repositories = []RepositorySnapshot{legacySnapshot}
	legacyArchive := filepath.Join(directory, "old-workspace.hel.zip")
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(legacyArchive, &legacyInput))
	legacyWorkspace := filepath.Join(directory, "legacy-restored")
	if err := os.MkdirAll(legacyWorkspace, 0o777); err != nil {
		t.Fatal(err)
	}
	legacyDestination := cloneRepository(t, legacyWorkspace, origin, "repo")
	missing, err := SystemGit{}.Run(legacyDestination, &GitCommand{
		Arguments: []string{"rev-parse", "--verify", "--quiet", missingBase + "^{commit}"},
	})
	if err != nil {
		t.Fatal(err)
	}
	if missing.Status != 1 {
		t.Fatalf("missing base status %d", missing.Status)
	}
	if err := RestoreRepositories(legacyArchive, legacyWorkspace, SystemGit{}); err != nil {
		t.Fatal(err)
	}
	for _, check := range [][]string{
		{head, "rev-parse", "HEAD"},
		{missingBase, "config", "mj.baseCommit"},
		{"true", "config", "mj.remoteWorkspace"},
	} {
		if got := gitLine(legacyDestination, check[1:]...); got != check[0] {
			t.Fatalf("git %q = %q, want %q", check[1:], got, check[0])
		}
	}
	if !bytes.Equal(archiveReadFile(t, filepath.Join(legacyDestination, "dirty.txt")), large) {
		t.Fatal("dirty.txt differs")
	}
	absentRef, err := SystemGit{}.Run(legacyDestination, &GitCommand{
		Arguments: []string{"show-ref", "--verify", "--quiet", core.HistoryCutoffBaseRef},
	})
	if err != nil {
		t.Fatal(err)
	}
	if absentRef.Status != 1 {
		t.Fatalf("base ref status %d", absentRef.Status)
	}
}

// largeText is enough text that the capture's patch crosses the 64 KiB pipe
// buffer a child's stdout is drained through, which is where truncation and
// deadlock bugs in the Git plumbing would show up.
func largeText(marker string, lines int) string {
	var text strings.Builder
	for line := range lines {
		fmt.Fprintf(&text, "%s line %d with enough text to make the patch large\n", marker, line)
	}
	return text.String()
}

func TestReviewCaptureSeesTrackedModifiedAndUntrackedChangesWithoutTouchingTheIndex(t *testing.T) {
	repository := t.TempDir()
	initializeRepository(t, repository)
	commitFile(t, repository, "tracked.txt", []byte(largeText("base", 800)), "base")

	baseline, err := CaptureWorktreeTree(SystemGit{}, repository)
	if err != nil {
		t.Fatal(err)
	}
	statusBefore := git(repository, "status", "--porcelain")

	archiveWriteFile(t, filepath.Join(repository, "tracked.txt"), []byte(largeText("changed", 800)))
	archiveWriteFile(t, filepath.Join(repository, "untracked.txt"), []byte(largeText("added", 800)))
	archiveWriteFile(t, filepath.Join(repository, "staged.txt"), []byte(largeText("staged", 800)))
	git(repository, "add", "staged.txt")

	current, err := CaptureWorktreeTree(SystemGit{}, repository)
	if err != nil {
		t.Fatal(err)
	}
	if baseline == current {
		t.Fatal("the capture must follow the working tree")
	}

	patch, err := DiffBetweenTrees(SystemGit{}, repository, &baseline, current)
	if err != nil {
		t.Fatal(err)
	}
	if len(patch) <= 64*1024 {
		t.Fatalf("the fixture must exceed one pipe buffer, got %d bytes", len(patch))
	}
	for _, text := range []string{"tracked.txt", "untracked.txt", "staged.txt", "+changed line 799"} {
		if !strings.Contains(patch, text) {
			t.Fatalf("%q missing from the patch", text)
		}
	}

	statusAfter := git(repository, "status", "--porcelain")
	if len(statusBefore) != 0 {
		t.Fatalf("the repository starts clean: %s", statusBefore)
	}
	if !strings.Contains(string(statusAfter), "A  staged.txt") {
		t.Fatalf("capture leaves the real index exactly as the user left it: %s", statusAfter)
	}
	// A capture is consumed by the review that took it, so it is not pinned:
	// Mjolnir leaves no ref of its own in a user's repository beyond the one
	// baseline a running session depends on.
	if refs := git(repository, "for-each-ref", "refs/hel"); len(refs) != 0 {
		t.Fatalf("a capture must not write a ref into the user's repository: %s", refs)
	}
	entries, err := os.ReadDir(filepath.Join(repository, ".git"))
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), "hel-review-index-") {
			t.Fatalf("the scratch index is removed after the capture: %s", entry.Name())
		}
	}
}

func TestReviewCaptureWithoutABaselineDiffsAgainstTheEmptyTree(t *testing.T) {
	repository := t.TempDir()
	initializeRepository(t, repository)
	commitFile(t, repository, "tracked.txt", []byte("base\n"), "base")

	current, err := CaptureWorktreeTree(SystemGit{}, repository)
	if err != nil {
		t.Fatal(err)
	}
	patch, err := DiffBetweenTrees(SystemGit{}, repository, nil, current)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(patch, "new file mode") || !strings.Contains(patch, "+base") {
		t.Fatalf("an absent baseline renders the whole capture as additions: %s", patch)
	}
}

func TestASessionDiffShowsTrackedAndUntrackedWorkAgainstTheRecordedBase(t *testing.T) {
	repository := t.TempDir()
	initializeRepository(t, repository)
	commitFile(t, repository, "tracked.txt", []byte("base\n"), "base")
	base := gitLine(repository, "rev-parse", "HEAD")
	git(repository, "config", "mj.remoteWorkspace", "true")
	git(repository, "config", "mj.baseCommit", base)

	// Work the agent committed, work it left staged, and a new file it never
	// told Git about all belong in what the session produced.
	commitFile(t, repository, "tracked.txt", []byte("changed\n"), "work")
	archiveWriteFile(t, filepath.Join(repository, "untracked.txt"), []byte("new\n"))

	details, err := SessionDiffDetails(SystemGit{}, repository, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(details.Diff, "+changed") || !strings.Contains(details.Diff, "untracked.txt") {
		t.Fatalf("the diff covers committed and untracked work: %s", details.Diff)
	}
	if details.HeadDescendsFromBase == nil || !*details.HeadDescendsFromBase {
		t.Fatalf("head descends from base: %v", details.HeadDescendsFromBase)
	}

	// Amending the base commit itself leaves HEAD on history the base is not
	// part of, so the diff carries changes the session did not write.
	git(repository, "reset", "--hard", base)
	git(repository, "commit", "--amend", "-m", "rewritten")
	if gitLine(repository, "rev-parse", "HEAD") == base {
		t.Fatal("HEAD did not move")
	}
	rewritten, err := SessionDiffDetails(SystemGit{}, repository, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if rewritten.HeadDescendsFromBase == nil || *rewritten.HeadDescendsFromBase {
		t.Fatalf("head descends from base: %v", rewritten.HeadDescendsFromBase)
	}
}

// A session working in a subdirectory of its checkout is asked for its diff
// from that subdirectory, and its work is the whole checkout's, as its
// checkpoint is.
//
// Hard-won: c4322938: subdirectory diffs omitted tracked and untracked work elsewhere in the checkout.
func TestASessionDiffFromASubdirectoryCoversTheWholeCheckout(t *testing.T) {
	root := t.TempDir()
	initializeRepository(t, root)
	commitFile(t, root, "tracked.txt", []byte("base\n"), "base")
	if err := os.Mkdir(filepath.Join(root, "docs"), 0o777); err != nil {
		t.Fatal(err)
	}
	commitFile(t, root, filepath.Join("docs", "guide.md"), []byte("guide\n"), "docs")
	base := gitLine(root, "rev-parse", "HEAD")
	archiveWriteFile(t, filepath.Join(root, "tracked.txt"), []byte("edited at the root\n"))
	archiveWriteFile(t, filepath.Join(root, "notes.txt"), []byte("untracked at the root\n"))
	archiveWriteFile(t, filepath.Join(root, "docs", "draft.md"), []byte("untracked in docs\n"))

	details, err := SessionDiffDetails(SystemGit{}, filepath.Join(root, "docs"), &base, nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, line := range []string{"+edited at the root", "+untracked at the root", "+untracked in docs"} {
		if !strings.Contains(details.Diff, line) {
			t.Fatalf("%s missing: %s", line, details.Diff)
		}
	}
}

func TestASessionDiffCanSelectAnOlderDetachedBaseWithoutChangingTheLaunchBase(t *testing.T) {
	root := t.TempDir()
	initializeRepository(t, root)
	commitFile(t, root, "task.txt", []byte("original\n"), "A")
	taskBase := gitLine(root, "rev-parse", "HEAD")
	commitFile(t, root, "unrelated.txt", []byte("unrelated history\n"), "B")
	launchBase := gitLine(root, "rev-parse", "HEAD")
	git(root, "config", "mj.remoteWorkspace", "true")
	git(root, "config", "mj.baseCommit", launchBase)
	git(root, "checkout", "--detach", taskBase)
	commitFile(t, root, "task.txt", []byte("committed task\n"), "C")
	head := gitLine(root, "rev-parse", "HEAD")
	archiveWriteFile(t, filepath.Join(root, "staged.txt"), []byte("staged work\n"))
	git(root, "add", "staged.txt")
	archiveWriteFile(t, filepath.Join(root, "task.txt"), []byte("committed task\nunstaged work\n"))
	archiveWriteFile(t, filepath.Join(root, "untracked.txt"), []byte("untracked work\n"))

	// A symbolic revision is resolved, not merely echoed into metadata.
	chosen, err := SessionDiffDetails(SystemGit{}, root, archiveRef("HEAD^"), nil)
	if err != nil {
		t.Fatal(err)
	}
	if chosen.Base != taskBase || chosen.Head != head {
		t.Fatalf("base %q head %q", chosen.Base, chosen.Head)
	}
	if chosen.HeadDescendsFromBase == nil || !*chosen.HeadDescendsFromBase {
		t.Fatalf("head descends from base: %v", chosen.HeadDescendsFromBase)
	}
	for _, line := range []string{"+committed task", "+unstaged work", "+staged work", "+untracked work"} {
		if !strings.Contains(chosen.Diff, line) {
			t.Fatalf("%s missing: %s", line, chosen.Diff)
		}
	}
	if strings.Contains(chosen.Diff, "unrelated.txt") {
		t.Fatalf("unrelated history in the diff: %s", chosen.Diff)
	}
	defaultDiff, err := SessionDiffDetails(SystemGit{}, root, nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if defaultDiff.Base != launchBase || defaultDiff.Head != head {
		t.Fatalf("base %q head %q", defaultDiff.Base, defaultDiff.Head)
	}
	// The session left the launch base behind, so its diff reverts the
	// unrelated commit the base carried.
	if defaultDiff.HeadDescendsFromBase == nil || *defaultDiff.HeadDescendsFromBase {
		t.Fatalf("head descends from base: %v", defaultDiff.HeadDescendsFromBase)
	}
	if !strings.Contains(defaultDiff.Diff, "-unrelated history") {
		t.Fatalf("diff %s", defaultDiff.Diff)
	}
	if got := gitLine(root, "config", "mj.baseCommit"); got != launchBase {
		t.Fatalf("launch base %q", got)
	}
	if got := gitLine(root, "diff", "--cached", "--name-only"); got != "staged.txt" {
		t.Fatalf("staged %q", got)
	}

	for _, invalid := range []string{"missing-base", "--all", "", "HEAD^{tree}"} {
		_, err := SessionDiffDetails(SystemGit{}, root, &invalid, nil)
		if _, ok := err.(SessionExportErrorRefused); !ok {
			t.Fatalf("%q: %#v", invalid, err)
		}
	}
}

func TestASessionDiffFallsBackToTheCommitItsBranchWasCreatedAt(t *testing.T) {
	repository := t.TempDir()
	initializeRepository(t, repository)
	commitFile(t, repository, "tracked.txt", []byte("base\n"), "base")
	base := gitLine(repository, "rev-parse", "HEAD")
	git(repository, "checkout", "-q", "-b", "mj/session-1")
	commitFile(t, repository, "tracked.txt", []byte("changed\n"), "work")

	patch, err := SessionDiffText(SystemGit{}, repository, nil, archiveRef("mj/session-1"))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(patch, "-base") || !strings.Contains(patch, "+changed") {
		t.Fatalf("with no recorded base the branch reflog names the session's start: %s", patch)
	}

	// The same repository with no base and no branch cannot answer at all.
	_, failure := SessionDiffText(SystemGit{}, repository, nil, nil)
	if failure == nil {
		t.Fatal("a diff without a base succeeded")
	}
	archiveAssertErrorContains(t, failure, "no session base recorded")
	if got := gitLine(repository, "rev-parse", "HEAD~1"); got != base {
		t.Fatalf("the diff reads the repository without moving it: %s", got)
	}
}

func TestPushingASessionBranchReachesOriginAndReportsAMissingRemote(t *testing.T) {
	parent := t.TempDir()
	repository := filepath.Join(parent, "work")
	initializeRepository(t, repository)
	commitFile(t, repository, "tracked.txt", []byte("base\n"), "base")

	_, err := PushBranch(SystemGit{}, repository, "review/one")
	if _, ok := err.(PushBranchErrorNoRemote); !ok {
		t.Fatalf("a repository with no remote refuses before running a push: %v", err)
	}

	remote := filepath.Join(parent, "origin.git")
	git(parent, "init", "-q", "--bare", remote)
	git(repository, "remote", "add", "origin", remote)

	pushed, failure := PushBranch(SystemGit{}, repository, "review/one")
	if failure != nil {
		t.Fatal(failure)
	}
	if pushed.Remote != "origin" || pushed.Branch != "review/one" {
		t.Fatalf("pushed %+v", pushed)
	}
	if gitLine(remote, "rev-parse", "refs/heads/review/one") != gitLine(repository, "rev-parse", "HEAD") {
		t.Fatal("the pushed branch names the session's current commit")
	}
}

func TestASessionFileReadStaysInsideTheWorkspace(t *testing.T) {
	root := t.TempDir()
	outside := t.TempDir()
	archiveWriteFile(t, filepath.Join(root, "inside.txt"), []byte("visible\n"))
	archiveWriteFile(t, filepath.Join(outside, "secret.txt"), []byte("private\n"))

	contents, err := ReadSessionFile(root, "inside.txt")
	if err != nil {
		t.Fatal(err)
	}
	if string(contents) != "visible\n" {
		t.Fatalf("contents %q", contents)
	}

	_, err = ReadSessionFile(root, filepath.Join("..", "secret.txt"))
	archiveAssertErrorContains(t, err, "..")

	if runtime.GOOS != "windows" {
		if err := os.Symlink(filepath.Join(outside, "secret.txt"), filepath.Join(root, "link")); err != nil {
			t.Fatal(err)
		}
		_, err := ReadSessionFile(root, "link")
		archiveAssertErrorContains(t, err, "leaves the session workspace")
	}
}

// A person who asked for the wrong path has to be able to see where the read
// looked, because the directory differs per target kind (#1079).
//
// Hard-won: f870729d: a reported file-export lookup bug lacked the searched directory in its refusal.
func TestAMissingSessionFileRefusalNamesTheDirectoryItSearched(t *testing.T) {
	root := t.TempDir()
	_, err := ReadSessionFile(root, "secret.txt")
	archiveAssertErrorContains(t, err, "secret.txt is not in the session workspace")
	archiveAssertErrorContains(t, err, root)
}

// Hard-won: c61f0d6fe: Git 2.25 echoed --path-format=absolute, so the capture started from HEAD instead of the real index.
func TestReviewCapturePreservesTrackedIgnoredFilesAndStagedDeletions(t *testing.T) {
	repository := t.TempDir()
	initializeRepository(t, repository)
	for _, name := range []string{"unchanged", "modified", "deleted", "staged-deleted", "cached-deleted"} {
		commitFile(t, repository, name, []byte("base\n"), name)
	}
	commitFile(t, repository, ".gitignore",
		[]byte("unchanged\nmodified\ndeleted\nstaged-deleted\ncached-deleted\nnew-ignored\nforced\n"),
		"ignore tracked files")
	indexPath := filepath.Join(repository, ".git", "index")
	indexBeforeIgnoredCapture := archiveReadFile(t, indexPath)
	baselineTree, err := CaptureWorktreeTree(git231{}, repository)
	if err != nil {
		t.Fatal(err)
	}
	archiveWriteFile(t, filepath.Join(repository, "new-ignored"), []byte("ignored\n"))
	ignoredOnlyTree, err := CaptureWorktreeTree(git231{}, repository)
	if err != nil {
		t.Fatal(err)
	}
	if baselineTree != ignoredOnlyTree {
		t.Fatal("a new ignored file does not change the captured review tree")
	}
	if !bytes.Equal(archiveReadFile(t, indexPath), indexBeforeIgnoredCapture) {
		t.Fatal("the ignored-file capture leaves the real index unchanged")
	}

	archiveWriteFile(t, filepath.Join(repository, "modified"), []byte("changed\n"))
	if err := os.Remove(filepath.Join(repository, "deleted")); err != nil {
		t.Fatal(err)
	}
	git(repository, "rm", "staged-deleted")
	git(repository, "rm", "--cached", "cached-deleted")
	archiveWriteFile(t, filepath.Join(repository, "forced"), []byte("forced\n"))
	git(repository, "add", "-f", "forced")
	index := archiveReadFile(t, indexPath)
	tree, err := CaptureWorktreeTree(git231{}, repository)
	if err != nil {
		t.Fatal(err)
	}
	for path, want := range map[string]string{"unchanged": "base\n", "modified": "changed\n", "forced": "forced\n"} {
		if got := git(repository, "show", tree+":"+path); string(got) != want {
			t.Fatalf("%s = %q", path, got)
		}
	}
	files := gitLine(repository, "ls-tree", "--name-only", tree)
	for _, absent := range []string{"deleted", "staged-deleted", "cached-deleted", "new-ignored"} {
		if slices.Contains(strings.Split(files, "\n"), absent) {
			t.Fatalf("%s: %s", absent, files)
		}
	}
	if !bytes.Equal(archiveReadFile(t, indexPath), index) {
		t.Fatal("the capture changed the real index")
	}
}

func TestReviewCaptureUsesLinkedWorktreeIndexAndBootstrapsMissingIndex(t *testing.T) {
	root := t.TempDir()
	repository := filepath.Join(root, "repo")
	if err := os.Mkdir(repository, 0o777); err != nil {
		t.Fatal(err)
	}
	initializeRepository(t, repository)
	commitFile(t, repository, "tracked", []byte("base\n"), "base")
	commitFile(t, repository, ".gitignore", []byte("tracked\n"), "ignore")
	linked := filepath.Join(root, "linked")
	git(repository, "worktree", "add", "--detach", linked)
	archiveWriteFile(t, filepath.Join(linked, "tracked"), []byte("linked\n"))
	index := gitLine(linked, "rev-parse", "--path-format=absolute", "--git-path", "index")
	before := archiveReadFile(t, index)
	tree, err := CaptureWorktreeTree(SystemGit{}, linked)
	if err != nil {
		t.Fatal(err)
	}
	if got := git(linked, "show", tree+":tracked"); string(got) != "linked\n" {
		t.Fatalf("tracked = %q", got)
	}
	if !bytes.Equal(archiveReadFile(t, index), before) {
		t.Fatal("the capture changed the linked worktree's index")
	}
	if err := os.Remove(index); err != nil {
		t.Fatal(err)
	}
	again, err := CaptureWorktreeTree(SystemGit{}, linked)
	if err != nil {
		t.Fatal(err)
	}
	if again != tree {
		t.Fatalf("tree %s, want %s", again, tree)
	}
	archiveAssertNotExists(t, index)
}

func TestReviewCaptureHandlesUnbornRepositoriesAndRejectsCorruptIndexes(t *testing.T) {
	repository := t.TempDir()
	initializeRepository(t, repository)
	archiveWriteFile(t, filepath.Join(repository, ".gitignore"), []byte("ignored\n"))
	archiveWriteFile(t, filepath.Join(repository, "ignored"), []byte("ignored\n"))
	archiveWriteFile(t, filepath.Join(repository, "new"), []byte("new\n"))
	tree, err := CaptureWorktreeTree(SystemGit{}, repository)
	if err != nil {
		t.Fatal(err)
	}
	files := strings.Split(gitLine(repository, "ls-tree", "--name-only", tree), "\n")
	if !slices.Contains(files, "new") || slices.Contains(files, "ignored") {
		t.Fatalf("files %q", files)
	}
	index := filepath.Join(repository, ".git", "index")
	archiveAssertNotExists(t, index)
	archiveWriteFile(t, index, []byte("corrupt index"))
	if _, err := CaptureWorktreeTree(SystemGit{}, repository); err == nil {
		t.Fatal("a corrupt index was captured")
	}
	if got := archiveReadFile(t, index); string(got) != "corrupt index" {
		t.Fatalf("index = %q", got)
	}
}

func TestArchiveRoundTripPreservesClearBoundaryAndRequiresContextSchema(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "cleared.hel.zip")
	archiveInput := input()
	archiveInput.CanonicalSession.Transcript[1].StableID = "context-cleared:reset"
	verified := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))
	if verified.Manifest.SchemaVersion != core.ArchiveSchemaVersionContext {
		t.Fatalf("schema = %d", verified.Manifest.SchemaVersion)
	}
	archiveAssertSameJSON(t, archiveInput.CanonicalSession, verified.CanonicalSession)
	if start := verified.CanonicalSession.CurrentContextStart(); start != 2 {
		t.Fatalf("context start %d", start)
	}
	archiveInput.CanonicalSession.Transcript = append(archiveInput.CanonicalSession.Transcript, core.CanonicalTranscriptItem{
		StableID:        "message:received-one",
		Position:        3,
		CreatedAtMs:     300,
		LastChangedAtMs: 300,
		Body: core.CanonicalTranscriptBodyMessage{
			Event: &core.MailboxEvent{
				Key:         "received-one",
				Source:      "session_message",
				Wake:        true,
				CreatedAtMs: 200,
				Body: core.MailboxEventBodySessionMessage{
					From: core.Sender{Type: core.SenderSession, ID: "sender-session", Title: "Reviewer"},
					Text: "All of the message.\nIncluding 支持 Unicode and the second line.",
				},
			},
		},
	})
	verified = archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))
	if verified.Manifest.SchemaVersion != core.ArchiveSchemaVersionMessages {
		t.Fatalf("schema = %d", verified.Manifest.SchemaVersion)
	}
	archiveAssertSameJSON(t, archiveInput.CanonicalSession, verified.CanonicalSession)
	if start := verified.CanonicalSession.CurrentContextStart(); start != 2 {
		t.Fatalf("context start %d", start)
	}
	manifest, payloads, err := prepareArchive(&archiveInput)
	if err != nil {
		t.Fatal(err)
	}
	manifest.SchemaVersion = core.ArchiveSchemaVersionContext
	downgraded := filepath.Join(directory, "message-downgraded.hel.zip")
	archiveWriteZip(t, downgraded, directory, &manifest, payloads)
	_, err = ReadArchiveVerified(downgraded)
	archiveAssertErrorContains(t, err, "message transcript entries require archive schema 9")
}

func TestNativeUseEvidenceUsesSchema15WithAndWithoutOriginDeltaAndRejectsDowngrade(t *testing.T) {
	directory := t.TempDir()
	evidence := core.NativeSessionUseEvidence{
		Version:         1,
		NativeSessionID: "native-1",
		OpenedOrdinal:   archiveRef(uint64(2)),
		Usage:           core.NativeSessionUseUnknown,
	}

	// Without origin_delta: writes schema 15, reads back accurately, rejects
	// downgrade to 13.
	archiveInput := input()
	archiveInput.CanonicalSession.NativeUseEvidence = archiveRef(evidence)
	path := filepath.Join(directory, "native-use.hel.zip")
	verified := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &archiveInput))
	if verified.Manifest.SchemaVersion != core.ArchiveSchemaVersionNativeUse {
		t.Fatalf("schema = %d", verified.Manifest.SchemaVersion)
	}
	if !reflect.DeepEqual(verified.CanonicalSession.NativeUseEvidence, &evidence) {
		t.Fatalf("evidence = %+v", verified.CanonicalSession.NativeUseEvidence)
	}

	manifest, payloads, err := prepareArchive(&archiveInput)
	if err != nil {
		t.Fatal(err)
	}
	manifest.SchemaVersion = core.ArchiveSchemaVersionMailbox
	downgraded := filepath.Join(directory, "native-use-downgraded.hel.zip")
	archiveWriteZip(t, downgraded, directory, &manifest, payloads)
	_, err = ReadArchiveVerified(downgraded)
	archiveAssertErrorContains(t, err, "native-session-use evidence requires archive schema 15")

	// With origin_delta: also writes schema 15 and verifies cleanly.
	deltaInput := input()
	deltaInput.CanonicalSession.NativeUseEvidence = archiveRef(evidence)
	repository := &deltaInput.Repositories[0]
	repository.CommittedBundle = nil
	repository.StagedPatch = nil
	repository.UnstagedPatch = nil
	repository.UntrackedTar = nil
	repository.Metadata.SavedRefs = nil
	repository.Metadata.StashStack = nil
	originURL := repository.Metadata.Origin
	headCommit := repository.Metadata.HeadCommit
	repository.Metadata.OriginDelta = &core.WorkspaceSnapshotMetadata{
		OriginURL:             &originURL,
		ObjectFormat:          "sha1",
		Notices:               []string{},
		Head:                  core.SnapshotHead{Kind: core.SnapshotHeadBranch, Name: "main", Commit: headCommit},
		Refs:                  map[string]string{"refs/heads/main": headCommit},
		BranchTracking:        map[string]core.BranchTracking{},
		Stash:                 []core.SnapshotStashEntry{},
		Worktrees:             []core.SnapshotWorktree{},
		InitializedSubmodules: []core.SnapshotSubmodule{},
		IndexGitlinks:         []core.SnapshotGitlink{},
		Prerequisites:         []string{},
	}
	deltaPath := filepath.Join(directory, "native-use-origin-delta.hel.zip")
	deltaVerified := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(deltaPath, &deltaInput))
	if deltaVerified.Manifest.SchemaVersion != core.ArchiveSchemaVersionNativeUse {
		t.Fatalf("schema = %d", deltaVerified.Manifest.SchemaVersion)
	}
	if !reflect.DeepEqual(deltaVerified.CanonicalSession.NativeUseEvidence, &evidence) {
		t.Fatalf("evidence = %+v", deltaVerified.CanonicalSession.NativeUseEvidence)
	}
}

func uncompressedUntrackedExcess(manifest *core.ArchiveManifest) uint64 {
	var total uint64
	for index := range manifest.Payloads {
		payload := &manifest.Payloads[index]
		if (payload.Role.Kind == core.PayloadRoleGitUntrackedTar || payload.Role.Kind == core.PayloadRoleGitLinkedWorktreeUntrackedTar) &&
			payload.Size > 1024 {
			total += payload.Size - 1024
		}
	}
	return total
}

func compressedUntrackedEntries(t testing.TB, path string) uint64 {
	t.Helper()
	archive, err := zip.OpenReader(path)
	if err != nil {
		t.Fatal(err)
	}
	defer archive.Close()
	var total uint64
	for _, entry := range archive.File {
		if strings.Contains(entry.Name, "untracked.tar") {
			total += entry.CompressedSize64
		}
	}
	return total
}

func originDeltaRepository(repository *RepositorySnapshot) {
	repository.CommittedBundle = nil
	repository.StagedPatch = nil
	repository.UnstagedPatch = nil
	repository.UntrackedTar = nil
	repository.OperationPayloads = nil
	repository.LinkedWorktrees = nil
	repository.Metadata.SavedRefs = nil
	repository.Metadata.StashStack = nil
	repository.Metadata.GitOperation = nil
	origin := repository.Metadata.Origin
	head := repository.Metadata.HeadCommit
	branch := "main"
	if repository.Metadata.Branch != nil {
		branch = *repository.Metadata.Branch
	}
	repository.Metadata.OriginDelta = &core.WorkspaceSnapshotMetadata{
		OriginURL:             &origin,
		ObjectFormat:          "sha1",
		Notices:               []string{},
		Head:                  core.SnapshotHead{Kind: core.SnapshotHeadBranch, Name: branch, Commit: head},
		Refs:                  map[string]string{"refs/heads/main": head},
		BranchTracking:        map[string]core.BranchTracking{},
		Stash:                 []core.SnapshotStashEntry{},
		Worktrees:             []core.SnapshotWorktree{},
		InitializedSubmodules: []core.SnapshotSubmodule{},
		IndexGitlinks:         []core.SnapshotGitlink{},
		Prerequisites:         []string{},
	}
}

func snapshotFromVerified(t testing.TB, archive *VerifiedArchive, repository *core.RepositoryManifest) RepositorySnapshot {
	t.Helper()
	id := repository.Metadata.ID
	payload := func(role core.PayloadRole) []byte {
		t.Helper()
		data, err := archive.PayloadByRole(&role)
		if err != nil {
			t.Fatal(err)
		}
		return slices.Clone(data)
	}
	repositoryRole := func(kind core.PayloadRoleKind) core.PayloadRole {
		return core.PayloadRole{Kind: kind, RepositoryID: id}
	}
	snapshot := RepositorySnapshot{
		Metadata:        repository.Metadata,
		CommittedBundle: payload(repositoryRole(core.PayloadRoleGitBundle)),
		StagedPatch:     payload(repositoryRole(core.PayloadRoleGitStagedPatch)),
		UnstagedPatch:   payload(repositoryRole(core.PayloadRoleGitUnstagedPatch)),
		UntrackedTar:    payload(repositoryRole(core.PayloadRoleGitUntrackedTar)),
	}
	if repository.Metadata.GitOperation != nil {
		snapshot.OperationPayloads = &GitOperationPayloads{
			ConflictedWorktreeTar: payload(repositoryRole(core.PayloadRoleGitConflictedWorktreeTar)),
			StateTar:              payload(repositoryRole(core.PayloadRoleGitOperationStateTar)),
		}
	}
	for index := range repository.LinkedWorktrees {
		worktree := &repository.LinkedWorktrees[index]
		worktreeRole := func(kind core.PayloadRoleKind) core.PayloadRole {
			return core.PayloadRole{Kind: kind, RepositoryID: id, Worktree: uint32(index)}
		}
		snapshot.LinkedWorktrees = append(snapshot.LinkedWorktrees, LinkedWorktreeSnapshot{
			Metadata:      worktree.Metadata,
			StagedPatch:   payload(worktreeRole(core.PayloadRoleGitLinkedWorktreeStagedPatch)),
			UnstagedPatch: payload(worktreeRole(core.PayloadRoleGitLinkedWorktreeUnstagedPatch)),
			UntrackedTar:  payload(worktreeRole(core.PayloadRoleGitLinkedWorktreeUntrackedTar)),
		})
	}
	return snapshot
}

func archiveFileSize(t testing.TB, path string) uint64 {
	t.Helper()
	info, err := os.Stat(path)
	if err != nil {
		t.Fatal(err)
	}
	return uint64(info.Size())
}

func archiveAssertSamePayload(t testing.TB, before, after *VerifiedArchive, role core.PayloadRole) {
	t.Helper()
	left, err := before.PayloadByRole(&role)
	if err != nil {
		t.Fatal(err)
	}
	right, err := after.PayloadByRole(&role)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(left, right) {
		t.Fatalf("%s changed", payloadRoleDebug(&role))
	}
}

func archiveAssertEmptyUntracked(t testing.TB, archive *VerifiedArchive, role core.PayloadRole) {
	t.Helper()
	body, err := archive.PayloadByRole(&role)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(body, emptyTar()) {
		t.Fatalf("%s was not emptied", payloadRoleDebug(&role))
	}
}

func TestPurgeReplacesPrimaryAndLinkedUntrackedFilesAndRestoresWithoutThem(t *testing.T) {
	directory := t.TempDir()
	origin := filepath.Join(directory, "origin")
	initializeRepository(t, origin)
	commitFile(t, origin, "base.txt", []byte("base\n"), "base")
	base := gitLine(origin, "rev-parse", "HEAD")
	source := cloneRepository(t, directory, origin, "source")
	commitFile(t, source, "first.txt", []byte("first\n"), "first")
	archiveWriteFile(t, filepath.Join(source, "base.txt"), []byte("stashed tracked\n"))
	archiveWriteFile(t, filepath.Join(source, "stash-only.txt"), []byte("stashed untracked\n"))
	git(source, "stash", "push", "-u", "-q", "-m", "holds untracked")
	archiveWriteFile(t, filepath.Join(source, "staged.txt"), []byte("staged\n"))
	git(source, "add", "staged.txt")
	archiveWriteFile(t, filepath.Join(source, "base.txt"), []byte("unstaged edit\n"))
	archiveWriteFile(t, filepath.Join(source, "scratch.txt"), []byte("scratch\n"))
	git(source, "worktree", "add", "-q", "-b", "lane", "lane")
	archiveWriteFile(t, filepath.Join(source, "lane", "lane-note.txt"), []byte("lane\n"))
	head := gitLine(source, "rev-parse", "HEAD")

	snapshot, err := CollectGitSnapshot(SystemGit{}, source, &GitCollectionSpec{
		ID:                  "repo",
		RelativeDestination: "repo",
		History:             GitHistoryModeCloneFrom(base),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(snapshot.UntrackedTar) == 0 {
		t.Fatal("no untracked tar")
	}
	if len(snapshot.LinkedWorktrees) != 1 || len(snapshot.LinkedWorktrees[0].UntrackedTar) == 0 {
		t.Fatalf("linked worktrees = %d", len(snapshot.LinkedWorktrees))
	}
	if !slices.ContainsFunc(snapshot.Metadata.StashStack, func(entry core.SavedStashEntry) bool {
		return strings.Contains(entry.Message, "holds untracked")
	}) {
		t.Fatalf("stash stack = %+v", snapshot.Metadata.StashStack)
	}

	archiveInput := input()
	archiveInput.Bundle.ID = "repo"
	archiveInput.Bundle.PrimaryRepository = "repo"
	archiveInput.Repositories = []RepositorySnapshot{snapshot}
	sourceArchive := filepath.Join(directory, "legacy.hel.zip")
	written := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(sourceArchive, &archiveInput))
	schema := written.Manifest.SchemaVersion
	if schema == core.ArchiveSchemaVersionOriginDelta {
		t.Fatalf("schema = %d", schema)
	}
	before := archiveMust[VerifiedArchive](t)(ReadArchiveVerified(sourceArchive))
	sourceLen := archiveFileSize(t, sourceArchive)

	destination := filepath.Join(directory, "purged.hel.zip")
	dryMissing := filepath.Join(directory, "no-such-dir", "dry.hel.zip")
	dry := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(sourceArchive, dryMissing, true))
	if _, ok := dry.Kind.(ArchiveUntrackedPurgeKindDryRun); !ok {
		t.Fatalf("kind = %#v", dry.Kind)
	}
	if dry.BytesReclaimed == 0 {
		t.Fatal("a dry run reclaimed nothing")
	}
	archiveAssertNotExists(t, filepath.Join(directory, "no-such-dir"))
	if !slices.ContainsFunc(dry.StashUntrackedNotes, func(note StashUntrackedNote) bool {
		return note.RepositoryID == "repo" && strings.Contains(note.Message, "holds untracked")
	}) {
		t.Fatalf("notes = %+v", dry.StashUntrackedNotes)
	}

	report := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(sourceArchive, destination, false))
	rewritten, ok := report.Kind.(ArchiveUntrackedPurgeKindRewritten)
	if !ok {
		t.Fatalf("expected a rewritten archive, got %#v", report.Kind)
	}
	if report.SchemaVersion != schema {
		t.Fatalf("schema = %d, want %d", report.SchemaVersion, schema)
	}
	uncompressed := uncompressedUntrackedExcess(&before.Manifest)
	if uncompressed <= dry.BytesReclaimed || uncompressed <= report.BytesReclaimed {
		t.Fatalf("uncompressed %d, dry %d, live %d", uncompressed, dry.BytesReclaimed, report.BytesReclaimed)
	}
	if want := saturatingSub(sourceLen, archiveFileSize(t, destination)); report.BytesReclaimed != want {
		t.Fatalf("reclaimed %d, want %d", report.BytesReclaimed, want)
	}
	if !reflect.DeepEqual(report.StashUntrackedNotes, dry.StashUntrackedNotes) {
		t.Fatalf("notes %+v, dry %+v", report.StashUntrackedNotes, dry.StashUntrackedNotes)
	}
	streamed := archiveMust[VerifiedArchiveMetadata](t)(VerifyArchiveStreaming(destination))
	if streamed.ArchiveSha256 != rewritten.ArchiveSha256 || streamed.Manifest.SchemaVersion != schema {
		t.Fatalf("streamed %s schema %d", streamed.ArchiveSha256, streamed.Manifest.SchemaVersion)
	}
	after := archiveMust[VerifiedArchive](t)(ReadArchiveVerified(destination))
	if after.ArchiveSha256 != rewritten.ArchiveSha256 {
		t.Fatalf("digest %s, want %s", after.ArchiveSha256, rewritten.ArchiveSha256)
	}
	if archiveFileSize(t, destination) >= sourceLen {
		t.Fatal("the purged archive is not smaller")
	}
	for _, role := range []core.PayloadRole{
		{Kind: core.PayloadRoleGitBundle, RepositoryID: "repo"},
		{Kind: core.PayloadRoleGitStagedPatch, RepositoryID: "repo"},
		{Kind: core.PayloadRoleGitUnstagedPatch, RepositoryID: "repo"},
		{Kind: core.PayloadRoleGitLinkedWorktreeStagedPatch, RepositoryID: "repo", Worktree: 0},
		{Kind: core.PayloadRoleGitLinkedWorktreeUnstagedPatch, RepositoryID: "repo", Worktree: 0},
		{Kind: core.PayloadRoleCanonicalSession},
	} {
		archiveAssertSamePayload(t, &before, &after, role)
	}
	for _, role := range []core.PayloadRole{
		{Kind: core.PayloadRoleGitUntrackedTar, RepositoryID: "repo"},
		{Kind: core.PayloadRoleGitLinkedWorktreeUntrackedTar, RepositoryID: "repo", Worktree: 0},
	} {
		archiveAssertEmptyUntracked(t, &after, role)
	}

	restored := snapshotFromVerified(t, &after, &after.Manifest.Repositories[0])
	destinationRepository := cloneRepository(t, directory, origin, "restored")
	if err := RestoreGitSnapshot(SystemGit{}, destinationRepository, &restored); err != nil {
		t.Fatal(err)
	}
	if got := gitLine(destinationRepository, "rev-parse", "HEAD"); got != head {
		t.Fatalf("HEAD %s, want %s", got, head)
	}
	for name, want := range map[string]string{"first.txt": "first\n", "staged.txt": "staged\n", "base.txt": "unstaged edit\n"} {
		if got := archiveReadFile(t, filepath.Join(destinationRepository, name)); string(got) != want {
			t.Fatalf("%s = %q, want %q", name, got, want)
		}
	}
	if !bytes.Equal(git(destinationRepository, "diff", "--cached"), git(source, "diff", "--cached")) {
		t.Fatal("the staged diff changed")
	}
	if !bytes.Equal(git(destinationRepository, "diff"), git(source, "diff")) {
		t.Fatal("the unstaged diff changed")
	}
	archiveAssertNotExists(t, filepath.Join(destinationRepository, "scratch.txt"))
	archiveAssertNotExists(t, filepath.Join(destinationRepository, "lane", "lane-note.txt"))
	if info, err := os.Stat(filepath.Join(destinationRepository, "lane", ".git")); err != nil || !info.Mode().IsRegular() {
		t.Fatalf("lane/.git is not a file: %v", err)
	}
}

func TestPurgeDryRunAndAlreadyCleanArchivesDoNotWrite(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "source.hel.zip")
	sourceInput := input()
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(source, &sourceInput))
	destination := filepath.Join(directory, "dest.hel.zip")
	dry := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(source, destination, true))
	if _, ok := dry.Kind.(ArchiveUntrackedPurgeKindDryRun); !ok {
		t.Fatalf("kind = %#v", dry.Kind)
	}
	if dry.BytesReclaimed == 0 {
		t.Fatal("a dry run reclaimed nothing")
	}
	archiveAssertNotExists(t, destination)

	for _, untracked := range [][]byte{nil, emptyTar()} {
		clean := input()
		for index := range clean.Repositories {
			clean.Repositories[index].UntrackedTar = slices.Clone(untracked)
		}
		cleanPath := filepath.Join(directory, fmt.Sprintf("clean-%d.hel.zip", len(untracked)))
		archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(cleanPath, &clean))
		cleanDestination := filepath.Join(directory, fmt.Sprintf("clean-%d-out.hel.zip", len(untracked)))
		report := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(cleanPath, cleanDestination, false))
		if _, ok := report.Kind.(ArchiveUntrackedPurgeKindAlreadyClean); !ok {
			t.Fatalf("kind = %#v", report.Kind)
		}
		if report.BytesReclaimed != 0 {
			t.Fatalf("reclaimed %d", report.BytesReclaimed)
		}
		archiveAssertNotExists(t, cleanDestination)
	}
}

func TestPurgeReportsOnDiskBytesForACompressibleUntrackedPayload(t *testing.T) {
	directory := t.TempDir()
	archiveInput := input()
	repetitive := bytes.Repeat([]byte{'a'}, 64*1024)
	archiveInput.Repositories[0].UntrackedTar = tarWithFile("pad.txt", repetitive, 0o644)
	archiveInput.Repositories[1].UntrackedTar = nil
	source := filepath.Join(directory, "compressible.hel.zip")
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(source, &archiveInput))
	before := archiveMust[VerifiedArchiveMetadata](t)(VerifyArchiveStreaming(source))
	uncompressed := uncompressedUntrackedExcess(&before.Manifest)
	removed := compressedUntrackedEntries(t, source)
	if uncompressed <= 60_000 || removed >= uncompressed {
		t.Fatalf("uncompressed %d, removed %d", uncompressed, removed)
	}

	dry := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(source, filepath.Join(directory, "dry.hel.zip"), true))
	if _, ok := dry.Kind.(ArchiveUntrackedPurgeKindDryRun); !ok {
		t.Fatalf("kind = %#v", dry.Kind)
	}
	if dry.BytesReclaimed == 0 || dry.BytesReclaimed > removed || removed-dry.BytesReclaimed >= 2048 ||
		dry.BytesReclaimed >= uncompressed {
		t.Fatalf("dry %d, removed %d, uncompressed %d", dry.BytesReclaimed, removed, uncompressed)
	}
	archiveAssertNotExists(t, filepath.Join(directory, "dry.hel.zip"))

	oldLen := archiveFileSize(t, source)
	destination := filepath.Join(directory, "purged.hel.zip")
	report := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(source, destination, false))
	newLen := archiveFileSize(t, destination)
	if _, ok := report.Kind.(ArchiveUntrackedPurgeKindRewritten); !ok {
		t.Fatalf("kind = %#v", report.Kind)
	}
	if report.BytesReclaimed != saturatingSub(oldLen, newLen) || report.BytesReclaimed == 0 ||
		report.BytesReclaimed >= uncompressed {
		t.Fatalf("reclaimed %d, old %d, new %d, uncompressed %d", report.BytesReclaimed, oldLen, newLen, uncompressed)
	}
}

func TestPurgeReportsOriginDeltaAndStillRewritesAMixedArchive(t *testing.T) {
	directory := t.TempDir()
	full := input()
	for index := range full.Repositories {
		originDeltaRepository(&full.Repositories[index])
	}
	fullPath := filepath.Join(directory, "origin.hel.zip")
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(fullPath, &full))
	fullDestination := filepath.Join(directory, "origin-out.hel.zip")
	report := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(fullPath, fullDestination, false))
	if _, ok := report.Kind.(ArchiveUntrackedPurgeKindAlreadyNewFormat); !ok {
		t.Fatalf("kind = %#v", report.Kind)
	}
	archiveAssertNotExists(t, fullDestination)

	mixed := input()
	originDeltaRepository(&mixed.Repositories[0])
	mixedPath := filepath.Join(directory, "mixed.hel.zip")
	written := archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(mixedPath, &mixed))
	if written.Manifest.SchemaVersion < core.ArchiveSchemaVersionOriginDelta {
		t.Fatalf("schema = %d", written.Manifest.SchemaVersion)
	}
	mixedDestination := filepath.Join(directory, "mixed-out.hel.zip")
	purged := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(mixedPath, mixedDestination, false))
	if _, ok := purged.Kind.(ArchiveUntrackedPurgeKindRewritten); !ok {
		t.Fatalf("kind = %#v", purged.Kind)
	}
	verified := archiveMust[VerifiedArchiveMetadata](t)(VerifyArchiveStreaming(mixedDestination))
	if verified.Manifest.SchemaVersion != written.Manifest.SchemaVersion {
		t.Fatalf("schema = %d, want %d", verified.Manifest.SchemaVersion, written.Manifest.SchemaVersion)
	}
	if verified.Manifest.Repositories[0].Metadata.OriginDelta == nil ||
		verified.Manifest.Repositories[1].Metadata.OriginDelta != nil {
		t.Fatal("origin delta metadata moved")
	}
	after := archiveMust[VerifiedArchive](t)(ReadArchiveVerified(mixedDestination))
	archiveAssertEmptyUntracked(t, &after, core.PayloadRole{Kind: core.PayloadRoleGitUntrackedTar, RepositoryID: "worker"})
}

func TestPurgeKeepsSchema3WhenOnlyTheUntrackedTarWasSharded(t *testing.T) {
	directory := t.TempDir()
	archiveInput := input()
	large := tarWithFile("big.bin", bytes.Repeat([]byte{'x'}, testPartBytes), 0o644)
	if len(large) <= testPartBytes {
		t.Fatalf("tar is %d bytes", len(large))
	}
	archiveInput.Repositories[0].UntrackedTar = large
	source := filepath.Join(directory, "sharded.hel.zip")
	if err := writeArchiveInstalledWithPartSize(source, &archiveInput, testPartBytes); err != nil {
		t.Fatal(err)
	}
	before := archiveMust[VerifiedArchiveMetadata](t)(VerifyArchiveStreaming(source))
	if before.Manifest.SchemaVersion != core.ArchiveSchemaVersionSharded {
		t.Fatalf("schema = %d", before.Manifest.SchemaVersion)
	}
	if !slices.ContainsFunc(before.Manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role.Kind == core.PayloadRoleGitUntrackedTar && len(payload.Parts) > 1
	}) {
		t.Fatal("the untracked tar was not sharded")
	}
	destination := filepath.Join(directory, "sharded-purged.hel.zip")
	oldLen := archiveFileSize(t, source)
	uncompressed := uncompressedUntrackedExcess(&before.Manifest)
	removed := compressedUntrackedEntries(t, source)
	dry := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(source, filepath.Join(directory, "sharded-dry.hel.zip"), true))
	if _, ok := dry.Kind.(ArchiveUntrackedPurgeKindDryRun); !ok {
		t.Fatalf("kind = %#v", dry.Kind)
	}
	if dry.BytesReclaimed == 0 || dry.BytesReclaimed > removed || removed-dry.BytesReclaimed >= 2048 ||
		dry.BytesReclaimed >= uncompressed {
		t.Fatalf("dry %d, removed %d, uncompressed %d", dry.BytesReclaimed, removed, uncompressed)
	}
	report := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(source, destination, false))
	if _, ok := report.Kind.(ArchiveUntrackedPurgeKindRewritten); !ok {
		t.Fatalf("kind = %#v", report.Kind)
	}
	if report.BytesReclaimed != saturatingSub(oldLen, archiveFileSize(t, destination)) || report.BytesReclaimed == 0 ||
		report.BytesReclaimed >= uncompressed {
		t.Fatalf("reclaimed %d, uncompressed %d", report.BytesReclaimed, uncompressed)
	}
	after := archiveMust[VerifiedArchiveMetadata](t)(VerifyArchiveStreaming(destination))
	if after.Manifest.SchemaVersion != core.ArchiveSchemaVersionSharded {
		t.Fatalf("schema = %d", after.Manifest.SchemaVersion)
	}
	untrackedRole := core.PayloadRole{Kind: core.PayloadRoleGitUntrackedTar, RepositoryID: "hel"}
	index := slices.IndexFunc(after.Manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role == untrackedRole
	})
	if index < 0 {
		t.Fatal("the untracked payload is missing")
	}
	untracked := after.Manifest.Payloads[index]
	if untracked.Size != 1024 || len(untracked.Parts) <= 1 {
		t.Fatalf("untracked size %d parts %d", untracked.Size, len(untracked.Parts))
	}
	read := archiveMust[VerifiedArchive](t)(ReadArchiveVerified(destination))
	archiveAssertEmptyUntracked(t, &read, untrackedRole)
}

func TestPurgePreservesOperationStateAndOtherPayloads(t *testing.T) {
	directory := t.TempDir()
	archiveInput := input()
	object := strings.Repeat("a", 40)
	archiveInput.Repositories[0].Metadata.GitOperation = &core.SavedGitOperation{
		Unmerged: []core.SavedUnmergedEntry{{
			Path:   "conflict.txt",
			Stage:  2,
			Mode:   "100644",
			Object: object,
		}},
		MissingWorktreePaths: []string{},
		Objects:              []string{object},
	}
	archiveInput.Repositories[0].OperationPayloads = &GitOperationPayloads{
		ConflictedWorktreeTar: tarWithFile("conflict.txt", []byte("<<<<<<<\n"), 0o644),
		StateTar:              tarWithFile("MERGE_HEAD", []byte("abc\n"), 0o644),
	}
	source := filepath.Join(directory, "operation.hel.zip")
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(source, &archiveInput))
	before := archiveMust[VerifiedArchive](t)(ReadArchiveVerified(source))
	destination := filepath.Join(directory, "operation-purged.hel.zip")
	report := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(source, destination, false))
	if report.SchemaVersion != before.Manifest.SchemaVersion {
		t.Fatalf("schema = %d, want %d", report.SchemaVersion, before.Manifest.SchemaVersion)
	}
	after := archiveMust[VerifiedArchive](t)(ReadArchiveVerified(destination))
	if after.Manifest.SchemaVersion != before.Manifest.SchemaVersion {
		t.Fatalf("schema = %d, want %d", after.Manifest.SchemaVersion, before.Manifest.SchemaVersion)
	}
	for _, role := range []core.PayloadRole{
		{Kind: core.PayloadRoleGitOperationStateTar, RepositoryID: "hel"},
		{Kind: core.PayloadRoleGitConflictedWorktreeTar, RepositoryID: "hel"},
		{Kind: core.PayloadRoleGitBundle, RepositoryID: "hel"},
		{Kind: core.PayloadRoleGitStagedPatch, RepositoryID: "hel"},
		{Kind: core.PayloadRoleNativeArtifact, RelativePath: "sessions/native-1/rollout.jsonl"},
	} {
		archiveAssertSamePayload(t, &before, &after, role)
	}
	archiveAssertEmptyUntracked(t, &after, core.PayloadRole{Kind: core.PayloadRoleGitUntrackedTar, RepositoryID: "hel"})
}

func TestPurgeLeavesAnExistingDestinationArchiveUntouched(t *testing.T) {
	directory := t.TempDir()
	source := filepath.Join(directory, "source.hel.zip")
	sourceInput := input()
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(source, &sourceInput))
	sourceBytes := archiveReadFile(t, source)
	destination := filepath.Join(directory, "dest.hel.zip")
	archiveWriteFile(t, destination, []byte("keep-me"))
	_, err := PurgeUntrackedFromArchive(source, destination, false)
	archiveAssertErrorContains(t, err, "existing")
	if got := archiveReadFile(t, destination); string(got) != "keep-me" {
		t.Fatalf("destination = %q", got)
	}
	if !bytes.Equal(archiveReadFile(t, source), sourceBytes) {
		t.Fatal("the source archive changed")
	}
}

func TestPurgeDetectsUntrackedStashEntriesInAShardedBundleWithoutBufferingBlobs(t *testing.T) {
	directory := t.TempDir()
	// Identical bytes zlib-compress to a single shard. A counter does not, so
	// the bundle crosses several part boundaries while its blob stays large.
	blob := make([]byte, 200*1024)
	for index := range blob {
		blob[index] = byte(index * 17)
	}
	bundle, stashCommit := bundleWithUntrackedStash(t, blob)
	archiveInput := input()
	archiveInput.Repositories[0].CommittedBundle = bundle
	archiveInput.Repositories[0].Metadata.StashStack = []core.SavedStashEntry{{
		Commit:  stashCommit,
		Message: "holds untracked",
	}}
	manifest, payloads, err := prepareArchiveWithPartSize(&archiveInput, testPartBytes)
	if err != nil {
		t.Fatal(err)
	}
	bundleRole := core.PayloadRole{Kind: core.PayloadRoleGitBundle, RepositoryID: "hel"}
	bundleIndex := slices.IndexFunc(payloads, func(payload pendingPayload) bool {
		return payload.descriptor.Role == bundleRole
	})
	if bundleIndex < 0 {
		t.Fatal("the bundle payload is missing")
	}
	data := payloads[bundleIndex].data
	// Stored bundles are not split by the writer. Cut this one explicitly so
	// the reader has to walk every shard in order.
	partBytes := max(len(data)/3, 1)
	if len(data) <= partBytes {
		t.Fatalf("bundle is %d bytes", len(data))
	}
	path := payloads[bundleIndex].descriptor.Path
	var parts []core.PayloadPartDescriptor
	for index, chunk := range slices.Collect(slices.Chunk(data, partBytes)) {
		parts = append(parts, core.PayloadPartDescriptor{
			Path:   payloadPartPath(path, index),
			Sha256: digestBytes(chunk),
			Size:   uint64(len(chunk)),
		})
	}
	if len(parts) <= 1 {
		t.Fatalf("parts = %d", len(parts))
	}
	payloads[bundleIndex].descriptor.Parts = slices.Clone(parts)
	for index := range manifest.Payloads {
		if manifest.Payloads[index].Path == path {
			manifest.Payloads[index].Parts = parts
		}
	}

	source := filepath.Join(directory, "sharded-bundle.hel.zip")
	temporary, err := os.CreateTemp(directory, ".tmp")
	if err != nil {
		t.Fatal(err)
	}
	if err := restrictArchivePermissions(temporary.Name()); err != nil {
		t.Fatal(err)
	}
	if err := writeZip(temporary, directory, &manifest, payloads); err != nil {
		t.Fatal(err)
	}
	if err := temporary.Sync(); err != nil {
		t.Fatal(err)
	}
	if err := temporary.Close(); err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(temporary.Name(), source); err != nil {
		t.Fatal(err)
	}
	if err := restrictArchivePermissions(source); err != nil {
		t.Fatal(err)
	}
	verified := archiveMust[VerifiedArchiveMetadata](t)(VerifyArchiveStreaming(source))
	if !slices.ContainsFunc(verified.Manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role == bundleRole && len(payload.Parts) > 1
	}) {
		t.Fatal("the bundle is not sharded")
	}

	destination := filepath.Join(directory, "sharded-out.hel.zip")
	resetBundleDiscardStats()
	report := archiveMust[ArchiveUntrackedPurgeReport](t)(PurgeUntrackedFromArchive(source, destination, true))
	if _, ok := report.Kind.(ArchiveUntrackedPurgeKindDryRun); !ok {
		t.Fatalf("kind = %#v", report.Kind)
	}
	archiveAssertNotExists(t, destination)
	if !slices.ContainsFunc(report.StashUntrackedNotes, func(note StashUntrackedNote) bool {
		return note.RepositoryID == "hel" && strings.Contains(note.Message, "holds untracked")
	}) {
		t.Fatalf("notes = %+v", report.StashUntrackedNotes)
	}
	peak, total := bundleDiscardStats()
	if peak == 0 || peak > 8*1024 {
		t.Fatalf("discard peak %d", peak)
	}
	if total < uint64(len(blob)) {
		t.Fatalf("discarded %d bytes of a %d byte blob", total, len(blob))
	}
}

func bundleWithUntrackedStash(t testing.TB, blob []byte) ([]byte, string) {
	t.Helper()
	commit := []byte("tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\n" +
		"parent 0000000000000000000000000000000000000001\n" +
		"parent 0000000000000000000000000000000000000002\n" +
		"parent 0000000000000000000000000000000000000003\n" +
		"author T <t@t> 1 +0000\n" +
		"committer T <t@t> 1 +0000\n" +
		"\n" +
		"holds untracked\n")
	oid := gitObjectID("commit", commit, false)
	pack := []byte("PACK")
	pack = binary.BigEndian.AppendUint32(pack, 2)
	pack = binary.BigEndian.AppendUint32(pack, 2)
	pack = pushStoredObject(t, pack, 1, commit)
	pack = pushStoredObject(t, pack, 3, blob)
	pack = append(pack, make([]byte, 20)...)
	return append([]byte("# v2 git bundle\n\n"), pack...), oid
}

func pushStoredObject(t testing.TB, pack []byte, kind byte, body []byte) []byte {
	t.Helper()
	size := len(body)
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
	var compressed bytes.Buffer
	encoder := zlib.NewWriter(&compressed)
	if _, err := encoder.Write(body); err != nil {
		t.Fatal(err)
	}
	if err := encoder.Close(); err != nil {
		t.Fatal(err)
	}
	return append(pack, compressed.Bytes()...)
}

func TestOlderArchivesWithoutTerminalDetailMetadataStillRestoreFullOutput(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, "legacy.hel.zip")
	legacy := input()
	output := strings.Repeat("terminal log line\n", 8*1024)
	legacy.CanonicalSession.Transcript = append(legacy.CanonicalSession.Transcript, core.CanonicalTranscriptItem{
		StableID:        "terminal:legacy",
		Position:        3,
		CreatedAtMs:     104,
		LastChangedAtMs: 104,
		Body: core.CanonicalTranscriptBodyTerminalOutput{
			Record: core.CanonicalTerminalOutput{
				TerminalID: "legacy",
				Output:     output,
				ExitCode:   archiveRef(uint32(1)),
			},
		},
	})
	archiveMust[VerifiedArchiveMetadata](t)(WriteArchiveAtomic(path, &legacy))
	reader, entry := archiveZipEntry(t, path, "canonical/session.json")
	defer reader.Close()
	opened, err := entry.Open()
	if err != nil {
		t.Fatal(err)
	}
	raw, err := io.ReadAll(opened)
	opened.Close()
	if err != nil {
		t.Fatal(err)
	}
	var session struct {
		Transcript []struct {
			Body map[string]json.RawMessage `json:"body"`
		} `json:"transcript"`
	}
	if err := json.Unmarshal(raw, &session); err != nil {
		t.Fatal(err)
	}
	oldBody := session.Transcript[2].Body
	var record struct {
		Output string `json:"output"`
	}
	if err := json.Unmarshal(oldBody["record"], &record); err != nil {
		t.Fatal(err)
	}
	if record.Output != output {
		t.Fatal("the archived output changed")
	}
	for _, field := range []string{"output_line_count", "first_error_line", "detail_trimmed"} {
		if _, present := oldBody[field]; present {
			t.Fatalf("%s was written", field)
		}
	}

	archive := archiveMust[VerifiedArchiveMetadata](t)(VerifyArchiveStreaming(path))
	archiveAssertSameJSON(t, legacy.CanonicalSession.Transcript[2], archive.CanonicalSession.Transcript[2])
}
