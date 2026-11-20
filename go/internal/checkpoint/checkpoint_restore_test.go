// Port of mj-checkpoint/src/checkpoint/tests.rs: linked worktrees, archive
// schema and wire compatibility, and relay seed tests; and of the
// publication tests in mj-checkpoint/src/checkpoint/restore.rs.

package checkpoint

import (
	"archive/tar"
	"archive/zip"
	"bytes"
	"crypto/sha256"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/klauspost/compress/zstd"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Agents make linked worktrees inside their checkout (`git worktree add
// .mj/lanes/x`); a recovery copy must carry each one's HEAD, index, files
// and lock, and bring them back at the same place.
func TestLinkedWorktreesInsideACloneSurviveARecoveryCopy(t *testing.T) {
	temp := t.TempDir()
	spec, archivePath := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	base := gitLine(repository, "rev-parse", "HEAD")
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureManagedClone, BaseCommit: base}
	configure := func(directory string) {
		gitLine(directory, "config", "user.email", "hel@example.test")
		gitLine(directory, "config", "user.name", "Hel Test")
	}
	// A lane on its own branch with a commit, a staged edit, an unstaged
	// edit and an untracked file with a non-ASCII name.
	gitLine(repository, "worktree", "add", ".mj/lanes/a", "-b", "lane-a")
	laneA := filepath.Join(repository, ".mj/lanes/a")
	configure(laneA)
	checkpointWriteFile(t, filepath.Join(laneA, "a.txt"), []byte("committed in a\n"))
	gitLine(laneA, "add", "a.txt")
	gitLine(laneA, "commit", "-m", "lane a")
	checkpointWriteFile(t, filepath.Join(laneA, "README.md"), []byte("staged in a\n"))
	gitLine(laneA, "add", "README.md")
	checkpointWriteFile(t, filepath.Join(laneA, "a.txt"), []byte("unstaged in a\n"))
	checkpointWriteFile(t, filepath.Join(laneA, "ü-notes.txt"), []byte("untracked in a\n"))
	// A lane nested inside the first one.
	gitLine(laneA, "worktree", "add", ".mj/lanes/c", "-b", "lane-c")
	checkpointWriteFile(t, filepath.Join(laneA, ".mj/lanes/c/c.txt"), []byte("untracked in c\n"))
	// A detached lane whose HEAD is a commit no ref holds.
	gitLine(repository, "worktree", "add", "--detach", ".mj/lanes/d")
	laneD := filepath.Join(repository, ".mj/lanes/d")
	configure(laneD)
	checkpointWriteFile(t, filepath.Join(laneD, "d.txt"), []byte("only d holds this\n"))
	gitLine(laneD, "add", "d.txt")
	gitLine(laneD, "commit", "-m", "detached lane")
	// A locked lane, and a registered lane whose directory is gone.
	gitLine(repository, "worktree", "add", ".mj/lanes/l", "-b", "lane-l")
	gitLine(repository, "worktree", "lock", "--reason", "busy", ".mj/lanes/l")
	gitLine(repository, "worktree", "add", ".mj/lanes/x", "-b", "lane-x")
	checkpointNoError(t, os.RemoveAll(filepath.Join(repository, ".mj/lanes/x")))
	checkpointWriteFile(t, filepath.Join(repository, "keep.txt"), []byte("untracked in the main checkout\n"))

	checkpointMust(ExportCheckpoint(&spec))(t)
	archive := checkpointMust(ReadArchiveVerified(archivePath))(t)
	if archive.Manifest.SchemaVersion != core.ArchiveSchemaVersionLinkedWorktrees {
		t.Fatalf("schema version = %d", archive.Manifest.SchemaVersion)
	}
	var recorded []string
	for _, worktree := range archive.Manifest.Repositories[0].LinkedWorktrees {
		recorded = append(recorded, worktree.Metadata.Path)
	}
	lanes := []string{".mj/lanes/a", ".mj/lanes/a/.mj/lanes/c", ".mj/lanes/d", ".mj/lanes/l"}
	if !checkpointSamePaths(recorded, lanes) {
		t.Fatalf("the missing lane is left out and parents come before children: %v", recorded)
	}
	mainTar := checkpointMust(archive.PayloadByRole(&core.PayloadRole{Kind: core.PayloadRoleGitUntrackedTar, RepositoryID: "app"}))(t)
	var mainFiles []string
	reader := tar.NewReader(bytes.NewReader(mainTar))
	for {
		header, err := reader.Next()
		if errors.Is(err, io.EOF) {
			break
		}
		checkpointNoError(t, err)
		mainFiles = append(mainFiles, header.Name)
	}
	if !checkpointSamePaths(mainFiles, []string{"keep.txt"}) {
		t.Fatalf("lane contents travel as the lanes' own entries: %v", mainFiles)
	}
	if refs := gitLine(repository, "for-each-ref", "refs/mj-checkpoint"); refs != "" {
		t.Fatalf("the capture removes its temporary refs: %s", refs)
	}

	restoredRoot := filepath.Join(temp, "restored")
	checkpointNoError(t, os.Mkdir(restoredRoot, 0o755))
	gitLine(restoredRoot, "clone", repository, "app")
	checkpointNoError(t, RestoreRepositories(archivePath, restoredRoot, SystemGit{}))
	restored := filepath.Join(restoredRoot, "app")
	branch := func(directory string) string {
		command := gitCommand([]string{"symbolic-ref", "--quiet", "HEAD"})
		output := checkpointMust(SystemGit{}.Run(directory, &command))(t)
		return strings.TrimSpace(string(output.Stdout))
	}
	for _, lane := range lanes {
		source, copy := filepath.Join(repository, lane), filepath.Join(restored, lane)
		for _, check := range []struct {
			name      string
			arguments []string
		}{
			{"HEAD", []string{"rev-parse", "HEAD"}},
			{"status", []string{"status", "--porcelain"}},
			{"index", []string{"diff", "--cached"}},
			{"worktree", []string{"diff"}},
		} {
			if got, want := gitLine(copy, check.arguments...), gitLine(source, check.arguments...); got != want {
				t.Fatalf("%s %s = %q, want %q", lane, check.name, got, want)
			}
		}
		if got, want := branch(copy), branch(source); got != want {
			t.Fatalf("%s branch = %q, want %q", lane, got, want)
		}
	}
	if body := checkpointReadFile(t, filepath.Join(restored, ".mj/lanes/a/ü-notes.txt")); string(body) != "untracked in a\n" {
		t.Fatalf("untracked file = %q", body)
	}
	lock := func(checkout string) *string {
		directory := gitLine(checkout, "rev-parse", "--path-format=absolute", "--git-dir")
		body, err := os.ReadFile(filepath.Join(directory, "locked"))
		if err != nil {
			return nil
		}
		reason := strings.TrimRight(string(body), " \t\r\n")
		return &reason
	}
	if reason := lock(filepath.Join(restored, ".mj/lanes/l")); reason == nil || *reason != "busy" {
		t.Fatalf("lock reason = %v", reason)
	}
	if reason := lock(filepath.Join(restored, ".mj/lanes/d")); reason != nil {
		t.Fatalf("an unlocked lane is locked: %s", *reason)
	}
	if checkpointExists(filepath.Join(restored, ".mj/lanes/x")) {
		t.Fatal("the missing lane was restored")
	}
	if refs := gitLine(restored, "for-each-ref", "refs/mj-checkpoint"); refs != "" {
		t.Fatalf("the restore left temporary refs: %s", refs)
	}

	// A retried restore finds the worktrees it already made and leaves them.
	snapshot := checkpointMust(archivedRepositorySnapshot(&archive, &archive.Manifest.Repositories[0]))(t)
	checkpointNoError(t, restoreLinkedWorktrees(SystemGit{}, restored, snapshot.LinkedWorktrees))
	if body := checkpointReadFile(t, filepath.Join(restored, ".mj/lanes/a/a.txt")); string(body) != "unstaged in a\n" {
		t.Fatalf("a.txt = %q", body)
	}
}

// A managed worktree's bundle is empty when its own HEAD has nothing beyond
// the base, yet a lane's commits, branch-held or detached, must still
// travel.
func TestLinkedWorktreeCommitsTravelWhenTheCheckoutHasNoneOfItsOwn(t *testing.T) {
	temp := t.TempDir()
	spec, archivePath := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	for _, lane := range []struct {
		name      string
		arguments []string
	}{
		{"branch", []string{"-b", "lane-branch"}},
		{"detached", []string{"--detach"}},
	} {
		path := ".mj/lanes/" + lane.name
		gitLine(repository, append([]string{"worktree", "add", path}, lane.arguments...)...)
		directory := filepath.Join(repository, path)
		gitLine(directory, "config", "user.email", "hel@example.test")
		gitLine(directory, "config", "user.name", "Hel Test")
		checkpointWriteFile(t, filepath.Join(directory, lane.name+".txt"), []byte(lane.name))
		gitLine(directory, "add", ".")
		gitLine(directory, "commit", "-m", lane.name)
	}
	checkpointMust(ExportCheckpoint(&spec))(t)

	restoredRoot := filepath.Join(temp, "restored")
	checkpointNoError(t, os.Mkdir(restoredRoot, 0o755))
	gitLine(restoredRoot, "clone", repository, "app")
	checkpointNoError(t, RestoreRepositories(archivePath, restoredRoot, SystemGit{}))
	restored := filepath.Join(restoredRoot, "app")
	for _, lane := range []string{"branch", "detached"} {
		source, copy := filepath.Join(repository, ".mj/lanes", lane), filepath.Join(restored, ".mj/lanes", lane)
		if got, want := gitLine(copy, "rev-parse", "HEAD"), gitLine(source, "rev-parse", "HEAD"); got != want {
			t.Fatalf("%s HEAD = %s, want %s", lane, got, want)
		}
	}
	if got := gitLine(filepath.Join(restored, ".mj/lanes/branch"), "branch", "--show-current"); got != "lane-branch" {
		t.Fatalf("branch lane = %q", got)
	}
	if got := gitLine(filepath.Join(restored, ".mj/lanes/detached"), "branch", "--show-current"); got != "" {
		t.Fatalf("detached lane = %q", got)
	}
}

// A retry skips a worktree that already sits at its recorded commit, so a
// restore that failed after `worktree add` must not leave one behind.
func TestALinkedWorktreeThatFailsToRestoreLeavesNothingForARetryToSkip(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	head := gitLine(repository, "rev-parse", "HEAD")
	snapshot := LinkedWorktreeSnapshot{
		Metadata: core.LinkedWorktreeMetadata{
			Path:       ".mj/lanes/a",
			HeadCommit: head,
		},
		StagedPatch:   []byte("this is not a patch\n"),
		UnstagedPatch: []byte{},
		UntrackedTar:  []byte{},
	}
	restore := func(snapshot LinkedWorktreeSnapshot) error {
		return restoreLinkedWorktrees(SystemGit{}, repository, []LinkedWorktreeSnapshot{snapshot})
	}
	if err := restore(snapshot); err == nil {
		t.Fatal("a broken patch restored")
	}
	if checkpointExists(filepath.Join(repository, ".mj/lanes/a")) {
		t.Fatal("a failed restore left its worktree directory")
	}
	if strings.Contains(gitLine(repository, "worktree", "list"), "lanes/a") {
		t.Fatal("a failed restore left its worktree registered")
	}

	snapshot.StagedPatch = []byte{}
	checkpointNoError(t, restore(snapshot))
	if !checkpointExists(filepath.Join(repository, ".mj/lanes/a/README.md")) {
		t.Fatal("the retried restore made no worktree")
	}
}

// zipTestEntry is one entry of a copied archive.
type zipTestEntry struct {
	name string
	mode uint32
	body []byte
}

// readZipTestEntries reads every entry of a checkpoint archive, zstd
// payloads included, as the zip crate reads them for the Rust copies.
func readZipTestEntries(t testing.TB, source string) []zipTestEntry {
	t.Helper()
	reader := checkpointMust(zip.OpenReader(source))(t)
	defer reader.Close()
	reader.RegisterDecompressor(zipMethodZstd, func(compressed io.Reader) io.ReadCloser {
		decoder, err := zstd.NewReader(compressed)
		if err != nil {
			return io.NopCloser(errorReader{err})
		}
		return decoder.IOReadCloser()
	})
	var entries []zipTestEntry
	for _, file := range reader.File {
		mode := uint32(0o600)
		if file.CreatorVersion>>8 == 3 {
			mode = uint32(file.Mode().Perm())
		}
		body := checkpointMust(file.Open())(t)
		data := checkpointMust(io.ReadAll(body))(t)
		body.Close()
		entries = append(entries, zipTestEntry{name: file.Name, mode: mode, body: data})
	}
	return entries
}

// errorReader fails every read with its error.
type errorReader struct{ err error }

func (r errorReader) Read([]byte) (int, error) { return 0, r.err }

// writeZipTestEntries writes entries as a new archive, each deflated with
// its Unix permissions, as the zip crate's default options write them.
func writeZipTestEntries(t testing.TB, destination string, entries []zipTestEntry) {
	t.Helper()
	output := checkpointMust(os.Create(destination))(t)
	writer := zip.NewWriter(output)
	for _, entry := range entries {
		header := &zip.FileHeader{Name: entry.name, Method: zip.Deflate}
		header.SetMode(os.FileMode(entry.mode & 0o777))
		file := checkpointMust(writer.CreateHeader(header))(t)
		checkpointMust(file.Write(entry.body))(t)
	}
	checkpointNoError(t, writer.Close())
	checkpointNoError(t, output.Close())
}

// checkpointPrettyJSON writes a decoded JSON value as serde_json's
// to_vec_pretty does.
func checkpointPrettyJSON(t testing.TB, value any) []byte {
	t.Helper()
	compact := checkpointMust(jsonvalue.Marshal(value))(t)
	var pretty bytes.Buffer
	checkpointNoError(t, json.Indent(&pretty, compact, "", "  "))
	return pretty.Bytes()
}

func copyArchiveWithSchema(t testing.TB, source string, destination string, schemaVersion uint32) {
	t.Helper()
	entries := readZipTestEntries(t, source)
	for i := range entries {
		if entries[i].name == "manifest.json" {
			manifest := checkpointMust(jsonvalue.Parse(entries[i].body))(t).(jsonvalue.Object)
			manifest.Set("schema_version", int64(schemaVersion))
			entries[i].body = checkpointPrettyJSON(t, manifest)
		}
	}
	writeZipTestEntries(t, destination, entries)
}

func copyArchiveWithCanonicalSession(t testing.TB, source string, destination string, canonicalSession *core.CanonicalSessionSnapshot) {
	t.Helper()
	entries := readZipTestEntries(t, source)
	canonicalBody := checkpointMust(checkpointMarshalPretty(canonicalSession))(t)
	digest := sha256.Sum256(canonicalBody)
	canonicalSha := core.LowerHex(digest[:])
	for i := range entries {
		switch entries[i].name {
		case "canonical/session.json":
			entries[i].body = slices.Clone(canonicalBody)
		case "manifest.json":
			manifest := checkpointMust(jsonvalue.Parse(entries[i].body))(t).(jsonvalue.Object)
			payloadsValue, _ := manifest.Get("payloads")
			payloads := payloadsValue.([]any)
			for j := range payloads {
				descriptor := payloads[j].(jsonvalue.Object)
				if path, _ := descriptor.Get("path"); path == any("canonical/session.json") {
					descriptor.Set("size", int64(len(canonicalBody)))
					descriptor.Set("sha256", canonicalSha)
					payloads[j] = descriptor
					break
				}
			}
			manifest.Set("payloads", payloads)
			entries[i].body = checkpointPrettyJSON(t, manifest)
		}
	}
	writeZipTestEntries(t, destination, entries)
}

// checkpointWithExtension is Path::with_extension.
func checkpointWithExtension(path string, extension string) string {
	return strings.TrimSuffix(path, filepath.Ext(path)) + "." + extension
}

func assertCanonicalRestoreRejectedBeforeMutation(t testing.TB, spec *CheckpointExportSpec, invalidArchive string, canonicalSession *core.CanonicalSessionSnapshot, expectedError string) {
	t.Helper()
	copyArchiveWithCanonicalSession(t, spec.OutputPath, invalidArchive, canonicalSession)
	readme := filepath.Join(spec.WorkspaceRoot, "app/README.md")
	before := checkpointReadFile(t, readme)
	relayRoot := checkpointWithExtension(invalidArchive, "relay")
	harnessHome := checkpointWithExtension(invalidArchive, "harness")

	err := RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         invalidArchive,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           relayRoot,
		HarnessHome:         harnessHome,
		RestoreRepositories: true,
		RestoreNative:       true,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{})

	if err == nil || !strings.Contains(err.Error(), expectedError) {
		t.Fatalf("error = %v", err)
	}
	if after := checkpointReadFile(t, readme); !bytes.Equal(after, before) {
		t.Fatal("a refused restore changed the workspace")
	}
	if checkpointExists(relayRoot) || checkpointExists(harnessHome) {
		t.Fatal("a refused restore wrote relay or native state")
	}
}

// A turn review keeps its whole world - profile, native session and relay -
// inside the primary worker root. A v1 checkpoint is single session, so none
// of it may end up in the archive.
func TestACheckpointExcludesEverythingTheReviewerOwns(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	reviewer := filepath.Join(spec.RelayRoot, "reviewer")
	reviewerHome := filepath.Join(reviewer, "profile")
	// A reviewer that ran: a staged profile with its own native rollout, its
	// own relay journal, and its own supervisor spec.
	reviewerNative := filepath.Join(reviewerHome, "sessions/2026/08/09")
	checkpointMkdirAll(t, reviewerNative)
	checkpointWriteFile(t, filepath.Join(reviewerNative, "rollout-reviewer-native.jsonl"), []byte("reviewer native session"))
	checkpointMkdirAll(t, filepath.Join(reviewer, "relay-journal"))
	checkpointWriteFile(t, filepath.Join(reviewer, "relay-journal/active.jsonl"), []byte("reviewer relay events"))
	checkpointWriteFile(t, filepath.Join(reviewer, "relay-state.json"), []byte("reviewer relay state"))
	checkpointWriteFile(t, filepath.Join(reviewer, "acp-supervisor.json"), []byte("reviewer bridge"))

	checkpointMust(ExportCheckpoint(&spec))(t)

	archive := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	var native []string
	for _, payload := range archive.Manifest.Payloads {
		if payload.Role.Kind == core.PayloadRoleNativeArtifact {
			native = append(native, checkpointLossy([]byte(payload.Role.RelativePath)))
		}
	}
	for _, path := range native {
		if strings.Contains(path, "reviewer") {
			t.Fatalf("a reviewer's files must stay out of the checkpoint: %v", native)
		}
	}
	archived := checkpointReadFile(t, spec.OutputPath)
	for _, secret := range []string{"reviewer native session", "reviewer relay events", "reviewer relay state"} {
		if bytes.Contains(archived, []byte(secret)) {
			t.Fatal("the archive must not carry the reviewer's content")
		}
	}
	// The primary's own native session is still captured, so this proves
	// exclusion rather than an export that captured nothing.
	if len(native) == 0 {
		t.Fatal("the primary's native session must still be exported")
	}
}

// R18: a checkout that records a nested repository as a bare gitlink, with
// no `.gitmodules` entry, could not be suspended: the submodule check failed
// on a gitlink Git has no URL for. The gitlink is now recorded as the commit
// it points to.
// Hard-won: 4d80f0eb: an unregistered gitlink made checkpoint fail with Git’s missing .gitmodules URL error.
func TestAGitlinkWithoutAGitmodulesEntryDoesNotBlockTheCheckpoint(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	nested := filepath.Join(repository, "vendor/tool")
	checkpointMkdirAll(t, nested)
	gitLine(nested, "init", "-q")
	gitLine(nested, "-c", "user.email=hel@example.test", "-c", "user.name=Hel Test", "commit", "-q", "--allow-empty", "-m", "tool")
	head := gitLine(nested, "rev-parse", "HEAD")
	gitLine(repository, "update-index", "--add", "--cacheinfo", "160000,"+head+",vendor/tool")
	gitLine(repository, "commit", "-q", "-m", "record the tool")

	if _, err := ExportCheckpoint(&spec); err != nil {
		t.Fatalf("an unregistered gitlink is recorded, not refused: %v", err)
	}
}

func TestRawProjectExportKeepsGitMetadataWithoutGitContents(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureMetadataOnly}
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	checkpointWriteFile(t, filepath.Join(repository, "README.md"), []byte("dirty"))
	checkpointWriteFile(t, filepath.Join(repository, "untracked.txt"), []byte("untracked"))

	checkpointMust(ExportCheckpoint(&spec))(t)
	archive := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	metadata := archive.Manifest.Repositories[0].Metadata
	if metadata.BaseCommit != metadata.HeadCommit {
		t.Fatalf("base %s, head %s", metadata.BaseCommit, metadata.HeadCommit)
	}
	for _, kind := range []core.PayloadRoleKind{
		core.PayloadRoleGitBundle,
		core.PayloadRoleGitStagedPatch,
		core.PayloadRoleGitUnstagedPatch,
		core.PayloadRoleGitUntrackedTar,
	} {
		if payload := checkpointMust(archive.PayloadByRole(&core.PayloadRole{Kind: kind, RepositoryID: "app"}))(t); len(payload) != 0 {
			t.Fatalf("%s carries %d bytes", kind, len(payload))
		}
	}
}

// checkpointSha256Hex is the lower-hex SHA-256 of text.
func checkpointSha256Hex(text string) string {
	digest := sha256.Sum256([]byte(text))
	return core.LowerHex(digest[:])
}

// Hard-won: 32a2ba2bf: raw checkpoints bundled full history and blocked shared-checkout resumes.
func TestRawCheckpointRestoresReviewRefsAndDeclaresTheNewSchema(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureMetadataOnly}
	source := filepath.Join(spec.WorkspaceRoot, "app")
	head := gitLine(source, "rev-parse", "HEAD")
	session := spec.Session.ID
	baseline := "refs/mj/review-baseline"
	start := "refs/mj/session-start/" + checkpointSha256Hex(session) + "/" + checkpointSha256Hex(baseline)
	checkpointWriteFile(t, filepath.Join(source, "README.md"), []byte("review baseline\n"))
	tree := checkpointMust(CaptureWorktreeTree(SystemGit{}, source))(t)
	point := checkpointMust(RecordReviewPoint(SystemGit{}, source, &ReviewPoint{
		Tree:    tree,
		Head:    &head,
		Pending: []string{},
		Session: &session,
	}))(t)
	reviewed := "refs/mj/reviewed/" + tree
	overridden := "refs/mj/overridden/" + tree
	for _, ref := range [][2]string{{baseline, point}, {start, head}, {reviewed, head}, {overridden, head}} {
		gitLine(source, "update-ref", ref[0], ref[1])
	}
	checkpointMust(ExportCheckpoint(&spec))(t)
	archive := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	if archive.Manifest.SchemaVersion != core.ArchiveSchemaVersionReviewRefs {
		t.Fatalf("schema version = %d", archive.Manifest.SchemaVersion)
	}
	if bundle := checkpointMust(archive.PayloadByRole(&core.PayloadRole{Kind: core.PayloadRoleGitBundle, RepositoryID: "app"}))(t); len(bundle) != 0 {
		t.Fatalf("a raw checkpoint bundled %d bytes", len(bundle))
	}
	destination := filepath.Join(temp, "destination")
	gitLine(temp, "clone", "--no-local", "--quiet", source, destination)
	missing := checkpointMust(SystemGit{}.Run(destination, &GitCommand{Arguments: []string{"cat-file", "-e", point}}))(t)
	if missing.Status == 0 {
		t.Fatal("the clone already holds the review point")
	}
	// The raw archive imports no objects. Missing baseline objects are
	// skipped, while commit refs whose objects remain locally available are
	// recreated.
	checkpointNoError(t, RestoreSingleRepositoryIntoCheckout(spec.OutputPath, destination, SystemGit{}))
	if refs := gitLine(destination, "for-each-ref", baseline); refs != "" {
		t.Fatalf("baseline = %q", refs)
	}
	for _, name := range []string{start, reviewed, overridden} {
		if got := gitLine(destination, "rev-parse", name); got != head {
			t.Fatalf("%s = %s, want %s", name, got, head)
		}
	}
	// Move transports workspace files separately and disables repository
	// restore. Its session handoff must still install durable review refs.
	checkpointWriteFile(t, filepath.Join(source, "README.md"), []byte("staged live work\n"))
	gitLine(source, "add", "README.md")
	checkpointWriteFile(t, filepath.Join(source, "README.md"), []byte("unstaged live work\n"))
	index := gitLine(source, "ls-files", "--stage")
	for _, name := range []string{baseline, start, reviewed} {
		gitLine(source, "update-ref", "-d", name)
	}
	restoreInto(t, temp, &spec, false)
	for _, name := range []string{start, reviewed} {
		if got := gitLine(source, "rev-parse", name); got != head {
			t.Fatalf("%s = %s, want %s", name, got, head)
		}
	}
	// The old pin was not archived, so no restore brings it back.
	if refs := gitLine(source, "for-each-ref", baseline); refs != "" {
		t.Fatalf("baseline = %q", refs)
	}
	otherSession := "other-session"
	foreign := checkpointMust(RecordReviewPoint(SystemGit{}, source, &ReviewPoint{
		Tree:    gitLine(source, "rev-parse", "HEAD^{tree}"),
		Head:    &head,
		Pending: []string{},
		Session: &otherSession,
	}))(t)
	gitLine(source, "update-ref", baseline, foreign)
	// Ordinary raw resumes and Move handoffs can share the active checkout.
	// Neither may refuse or replace another session's pin.
	restoreInto(t, filepath.Join(temp, "shared-resume"), &spec, false)
	if got := gitLine(source, "rev-parse", baseline); got != foreign {
		t.Fatalf("baseline = %s, want %s", got, foreign)
	}
	if got := gitLine(source, "ls-files", "--stage"); got != index {
		t.Fatalf("index = %s, want %s", got, index)
	}
	if body := checkpointReadFile(t, filepath.Join(source, "README.md")); string(body) != "unstaged live work\n" {
		t.Fatalf("README.md = %q", body)
	}
}

func TestSessionDeltaWithoutOriginRefsRepairsOnceThenFails(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureSessionDelta}
	git := recordingGitWithFetchFailure()

	_, err := ExportCheckpointWithGit(&spec, git)

	if git.fetches() != 1 {
		t.Fatalf("fetches = %d", git.fetches())
	}
	if err == nil {
		t.Fatal("a repository without origin refs exported")
	}
	for _, expected := range []string{"repository 'app' has no origin refs", "refusing to bundle full history", "repair fetch failed"} {
		if !strings.Contains(err.Error(), expected) {
			t.Fatalf("error = %v", err)
		}
	}
}

func TestRemoteWorkspaceCaptureUsesImmutableMarkerBaseAfterOriginMoves(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	base := gitLine(repository, "rev-parse", "HEAD")
	gitLine(repository, "switch", "-q", "-c", "mj/remote-session")
	gitLine(repository, "config", "--local", "mj.remoteWorkspace", "true")
	gitLine(repository, "config", "--local", "mj.baseCommit", base)
	pushURL := "https://push.example.test/repo.git"
	gitLine(repository, "config", "--local", "--add", "remote.origin.pushurl", pushURL)
	checkpointWriteFile(t, filepath.Join(repository, "session.txt"), []byte("published session\n"))
	gitLine(repository, "add", "session.txt")
	gitLine(repository, "commit", "-qm", "session work")
	head := gitLine(repository, "rev-parse", "HEAD")
	// Make origin-tracking history appear current. A managed checkpoint must
	// still carry the commit because it uses mj.baseCommit.
	gitLine(repository, "update-ref", "refs/remotes/origin/main", head)

	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureRemoteWorkspace}
	repositories := checkpointMust(collectCheckpointRepositories(spec.WorkspaceRoot, spec.Repositories, SystemGit{}))(t)
	snapshot := repositories[0]
	if !snapshot.Metadata.RemoteWorkspace || snapshot.Metadata.BaseCommit != base || snapshot.Metadata.HeadCommit != head ||
		!slices.Equal(snapshot.Metadata.PushURLs, []string{pushURL}) {
		t.Fatalf("metadata = %+v", snapshot.Metadata)
	}
	if len(snapshot.CommittedBundle) == 0 {
		t.Fatal("the session commit was not bundled")
	}
}

func TestInvalidCanonicalSessionIsRejectedBeforeRepositoryRepair(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureSessionDelta}
	spec.CanonicalSession.Session.Execution = core.CanonicalExecutionState{State: core.CanonicalExecutionStateRunning, StartedAtMs: 3}
	git := recordingGitWithFetchFailure()

	_, err := ExportCheckpointWithGit(&spec, git)

	if err == nil || !strings.Contains(err.Error(), "not idle at the checkpoint barrier") {
		t.Fatalf("error = %v", err)
	}
	if git.fetches() != 0 {
		t.Fatalf("fetches = %d", git.fetches())
	}
	if checkpointExists(spec.OutputPath) {
		t.Fatal("a refused export left an archive")
	}
}

func TestSessionDeltaExportSucceedsWhenTheRepairFetchRestoresOriginRefs(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureSessionDelta}
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	origin := filepath.Join(temp, "origin.git")
	gitLine(spec.WorkspaceRoot, "clone", "-q", "--bare", "app", origin)
	gitLine(repository, "remote", "set-url", "origin", origin)
	checkpointWriteFile(t, filepath.Join(repository, "later.txt"), []byte("later"))
	gitLine(repository, "add", ".")
	gitLine(repository, "commit", "-qm", "later")
	gitRunner := recordingGitForwarding()

	checkpointMust(ExportCheckpointWithGit(&spec, gitRunner))(t)

	if gitRunner.fetches() != 1 {
		t.Fatalf("fetches = %d", gitRunner.fetches())
	}
	archive := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	if base := archive.Manifest.Repositories[0].Metadata.BaseCommit; base != "" {
		t.Fatalf("base = %q", base)
	}
	if bundle := checkpointMust(archive.PayloadByRole(&core.PayloadRole{Kind: core.PayloadRoleGitBundle, RepositoryID: "app"}))(t); len(bundle) == 0 {
		t.Fatal("the session commit was not bundled")
	}
}

func TestRawProjectWithoutGitMetadataFailsCheckpointExport(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureMetadataOnly}
	checkpointNoError(t, os.RemoveAll(filepath.Join(spec.WorkspaceRoot, "app/.git")))

	_, err := ExportCheckpoint(&spec)

	if err == nil || !strings.Contains(err.Error(), "repository has no valid Git HEAD") {
		t.Fatalf("error = %v", err)
	}
}

// Hard-won: bd5c08f6: a slow export gave no phase timing in daemon logs.
func TestExportReportsPhaseTimingsToTheController(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	gitRunner := recordingGitForwarding()

	exported := checkpointMust(ExportCheckpointWithGit(&spec, gitRunner))(t)

	timings := exported.Timings
	if timings == nil {
		t.Fatal("export reports its phase timings")
	}
	if timings.TotalMs < max(timings.NativeMs, timings.RepositoriesMs, timings.ArchiveMs) {
		t.Fatalf("timings = %+v", *timings)
	}
}

func TestTargetCheckpointFromAWorkerWithoutTimingsStillDecodes(t *testing.T) {
	var target TargetCheckpoint
	checkpointNoError(t, json.Unmarshal([]byte(`{
		"path": "/worker/checkpoint.hel.zip",
		"sha256": "abc",
		"event_frontier": 7,
		"event_frontier_digest": "def"
	}`), &target))

	if target.Timings != nil {
		t.Fatalf("timings = %+v", *target.Timings)
	}
	// The field is also absent from the wire form when nothing measured it.
	encoded := checkpointMust(checkpointMarshal(target))(t)
	var members map[string]json.RawMessage
	checkpointNoError(t, json.Unmarshal(encoded, &members))
	if _, ok := members["timings"]; ok {
		t.Fatalf("encoded = %s", encoded)
	}
}

func TestCheckpointWireRequiresTheNewCaptureModeAndRejectsLegacyFields(t *testing.T) {
	var legacy CheckpointRepositorySpec
	if err := json.Unmarshal([]byte(`{"id":"app","relative_destination":"app","base_commit":"HEAD"}`), &legacy); err == nil {
		t.Fatal("a legacy repository spec decoded")
	}

	var repository CheckpointRepositorySpec
	checkpointNoError(t, json.Unmarshal([]byte(`{
		"id": "app",
		"relative_destination": "app",
		"capture": { "mode": "delta_from", "base_commit": "HEAD" },
		"origin_override": null
	}`), &repository))
	if repository.Capture != (CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureDeltaFrom, BaseCommit: "HEAD"}) {
		t.Fatalf("capture = %+v", repository.Capture)
	}

	var mixed CheckpointRepositorySpec
	if err := json.Unmarshal([]byte(`{
		"id": "app",
		"relative_destination": "app",
		"capture": { "mode": "session_delta" },
		"origin_override": null,
		"session_delta": true
	}`), &mixed); err == nil {
		t.Fatal("a spec with a legacy field decoded")
	}

	// The compatibility reset does not reinterpret the old event frontier.
	var target TargetCheckpoint
	if err := json.Unmarshal([]byte(`{
		"path": "/worker/checkpoint.hel.zip",
		"sha256": "abc",
		"event_sequence": 7,
		"full_history_fallbacks": ["app"]
	}`), &target); err == nil {
		t.Fatal("a legacy target checkpoint decoded")
	}

	restore := map[string]any{
		"archive_path":           "/relay/checkpoint.hel.zip",
		"workspace_root":         "/workspace",
		"relay_root":             "/relay",
		"harness_home":           "/harness",
		"restore_repositories":   true,
		"restore_native":         true,
		"discard_queued_prompts": false,
	}
	decodeRestore := func(value map[string]any) (CheckpointRestoreSpec, error) {
		var spec CheckpointRestoreSpec
		err := json.Unmarshal(checkpointMust(json.Marshal(value))(t), &spec)
		return spec, err
	}
	decoded := checkpointMust(decodeRestore(restore))(t)
	if decoded.QueuePolicy != QueueRestorePolicyRestore {
		t.Fatalf("queue policy = %s", decoded.QueuePolicy)
	}
	legacyDiscard := cloneJSONObject(restore)
	legacyDiscard["discard_queued_prompts"] = true
	if spec := checkpointMust(decodeRestore(legacyDiscard))(t); spec.QueuePolicy != QueueRestorePolicyDiscard {
		t.Fatalf("queue policy = %s", spec.QueuePolicy)
	}
	deferred := decoded
	deferred.ReportRoot = nil
	deferred.QueuePolicy = QueueRestorePolicyDefer
	var roundTripped CheckpointRestoreSpec
	checkpointNoError(t, json.Unmarshal(checkpointMust(json.Marshal(deferred))(t), &roundTripped))
	if !reflect.DeepEqual(roundTripped, deferred) {
		t.Fatalf("round trip = %+v, want %+v", roundTripped, deferred)
	}
	missingFlag := cloneJSONObject(restore)
	delete(missingFlag, "discard_queued_prompts")
	if _, err := decodeRestore(missingFlag); err == nil {
		t.Fatal("a restore spec without a queue policy decoded")
	}
	retiredRoot := cloneJSONObject(restore)
	retiredRoot["worker_root"] = "/legacy"
	if _, err := decodeRestore(retiredRoot); err == nil {
		t.Fatal("a restore spec with the retired worker root decoded")
	}
}

// cloneJSONObject copies a JSON object built in a test.
func cloneJSONObject(value map[string]any) map[string]any {
	clone := make(map[string]any, len(value))
	for key, member := range value {
		clone[key] = member
	}
	return clone
}

func TestCheckpointExportWireUsesRelayRootAndRejectsRetiredWorkerRoot(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	var value map[string]json.RawMessage
	checkpointNoError(t, json.Unmarshal(checkpointMust(json.Marshal(spec))(t), &value))
	if string(value["protocol_version"]) != strconv.Itoa(int(CheckpointExportProtocolVersion)) {
		t.Fatalf("protocol version = %s", value["protocol_version"])
	}
	if _, ok := value["relay_root"]; !ok {
		t.Fatal("the spec has no relay_root")
	}
	if _, ok := value["worker_root"]; ok {
		t.Fatal("the spec has a worker_root")
	}

	unversioned := make(map[string]json.RawMessage, len(value))
	for key, member := range value {
		unversioned[key] = member
	}
	delete(unversioned, "protocol_version")
	var decoded CheckpointExportSpec
	if err := json.Unmarshal(checkpointMust(json.Marshal(unversioned))(t), &decoded); err == nil {
		t.Fatal("an unversioned spec decoded")
	}

	value["worker_root"] = json.RawMessage(`"/legacy"`)
	if err := json.Unmarshal(checkpointMust(json.Marshal(value))(t), &decoded); err == nil {
		t.Fatal("a spec with the retired worker root decoded")
	}
}

func TestCheckpointExportRejectsAnUnsupportedProtocolBeforeInterpretingPaths(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.ProtocolVersion = CheckpointExportProtocolVersion + 1
	spec.RelayRoot = "relative/worker"

	_, err := ExportCheckpoint(&spec)

	if err == nil || err.Error() != "unsupported checkpoint export protocol version 3; worker supports 2" {
		t.Fatalf("error = %v", err)
	}
}

// checkpointSameJSON compares two values as their JSON, which is how Rust's
// derived equality of these serde types reads after a round trip.
func checkpointSameJSON(t testing.TB, left, right any) bool {
	t.Helper()
	return bytes.Equal(checkpointMust(checkpointMarshal(left))(t), checkpointMust(checkpointMarshal(right))(t))
}

func TestCheckpointRoundTripsTheLatchedMaterializedSession(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.CanonicalSession.EventFrontier = 9
	spec.CanonicalSession.Transcript[0].Position = 7

	target := checkpointMust(ExportCheckpoint(&spec))(t)
	if target.EventFrontier != 9 {
		t.Fatalf("event frontier = %d", target.EventFrontier)
	}
	session := checkpointMust(ReadCheckpointSession(spec.OutputPath))(t)
	if !checkpointSameJSON(t, session, spec.CanonicalSession) {
		t.Fatalf("session = %+v", session)
	}
}

func restoreInto(t testing.TB, temp string, spec *CheckpointExportSpec, discardQueuedPrompts bool) string {
	t.Helper()
	relayRoot := filepath.Join(temp, "restored-relay-"+strconv.FormatBool(discardQueuedPrompts))
	policy := QueueRestorePolicyRestore
	if discardQueuedPrompts {
		policy = QueueRestorePolicyDiscard
	}
	checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           relayRoot,
		HarnessHome:         filepath.Join(temp, "restored-harness"),
		RestoreRepositories: false,
		RestoreNative:       false,
		QueuePolicy:         policy,
	}, SystemGit{}))
	return relayRoot
}

func restoredSeed(t testing.TB, relayRoot string) core.RestoredRelaySeed {
	t.Helper()
	var seed core.RestoredRelaySeed
	checkpointNoError(t, json.Unmarshal(checkpointReadFile(t, core.RestoredRelaySeedPath(relayRoot)), &seed))
	return seed
}

// I1-6: a same-harness resume kept the conversation but not the model the
// session had selected, because the relay seed carried no configuration.
// Hard-won: a78a57a9: native resume silently replaced the accepted Claude model and effort with harness defaults.
func TestANativeRestoreSeedsTheAcceptedModelAndEffortAndATextHandoffDoesNot(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.CanonicalSession.Session.Configuration.Values = map[string]any{
		"model":  "opus[1m]",
		"effort": "high",
		"mode":   "plan",
	}
	checkpointMust(ExportCheckpoint(&spec))(t)

	seedFor := func(restoreNative bool) map[string]string {
		relayRoot := filepath.Join(temp, "seeded-relay-"+strconv.FormatBool(restoreNative))
		checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
			ArchivePath:         spec.OutputPath,
			WorkspaceRoot:       spec.WorkspaceRoot,
			RelayRoot:           relayRoot,
			HarnessHome:         filepath.Join(temp, "seeded-harness-"+strconv.FormatBool(restoreNative)),
			RestoreRepositories: false,
			RestoreNative:       restoreNative,
			QueuePolicy:         QueueRestorePolicyRestore,
		}, SystemGit{}))
		return restoredSeed(t, relayRoot).AcceptedConfig
	}

	if accepted := seedFor(true); !reflect.DeepEqual(accepted, map[string]string{"effort": "high", "model": "opus[1m]"}) {
		t.Fatalf("accepted = %v", accepted)
	}
	if accepted := seedFor(false); len(accepted) != 0 {
		t.Fatalf("a text handoff seeded %v", accepted)
	}
}

// X-6: the projection stores each selector under the harness's own option
// id. Codex's effort option is `reasoning_effort`, so a seed that read only
// the key `effort` lost the effort across suspend and resume while the model
// (id `model`) survived.
// Hard-won: 0b5375dc: Codex resume read only the generic effort key and lost its reasoning_effort selection.
func TestANativeRestoreSeedsEffortStoredUnderTheHarnessOptionID(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.CanonicalSession.Session.Configuration.Values = map[string]any{
		"model":            "gpt-5.6-luna",
		"reasoning_effort": "low",
	}
	checkpointMust(ExportCheckpoint(&spec))(t)
	relayRoot := filepath.Join(temp, "option-id-relay")
	checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           relayRoot,
		HarnessHome:         filepath.Join(temp, "option-id-harness"),
		RestoreRepositories: false,
		RestoreNative:       true,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{}))
	if accepted := restoredSeed(t, relayRoot).AcceptedConfig; !reflect.DeepEqual(accepted, map[string]string{"effort": "low", "model": "gpt-5.6-luna"}) {
		t.Fatalf("accepted = %v", accepted)
	}
}

// I2-7: an archive carries no relay journal, so the worker restored from a
// session suspended before any prompt could not tell its native session was
// never used, and refused to replace it when the harness had no record of
// it. The seed now says whether the native session the restore continues
// ever received a prompt. `/clear` starts a new native session, so only the
// conversation after the newest context boundary counts.
// Hard-won: 0bb9439b: a never-prompted Codex thread was marked used and refused on resume.
func TestANativeRestoreSaysWhetherTheContinuedSessionEverReceivedAPrompt(t *testing.T) {
	temp := t.TempDir()
	base, _ := checkpointFixture(t, temp)
	boundary := core.CanonicalTranscriptItem{
		StableID:        core.ContextBoundaryPrefix + "clear-1",
		Position:        2,
		CreatedAtMs:     2,
		LastChangedAtMs: 2,
		Body:            core.CanonicalTranscriptBodySystem{Text: "Context cleared — a new conversation starts here."},
	}
	unusedAfterRestore := func(name string, transcript []core.CanonicalTranscriptItem, restoreNative bool) bool {
		spec := base
		spec.CanonicalSession.Transcript = transcript
		spec.CanonicalSession.EventFrontier = 2
		spec.OutputPath = filepath.Join(spec.RelayRoot, name+".hel.zip")
		checkpointMust(ExportCheckpoint(&spec))(t)
		relayRoot := filepath.Join(temp, "relay-"+name)
		checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
			ArchivePath:         spec.OutputPath,
			WorkspaceRoot:       spec.WorkspaceRoot,
			RelayRoot:           relayRoot,
			HarnessHome:         filepath.Join(temp, "harness-"+name),
			RestoreRepositories: false,
			RestoreNative:       restoreNative,
			QueuePolicy:         QueueRestorePolicyRestore,
		}, SystemGit{}))
		return restoredSeed(t, relayRoot).NativeSessionUnused
	}
	// Hard-won: 806b123: an adopted used conversation looked unused without its old transcript.
	adoption := core.CanonicalTranscriptItem{
		StableID:        core.AdoptionHistoryUnavailableItemID,
		Position:        1,
		CreatedAtMs:     1,
		LastChangedAtMs: 1,
		Body:            core.CanonicalTranscriptBodySystem{Text: "Earlier transcript unavailable"},
	}
	if unusedAfterRestore("adopted", []core.CanonicalTranscriptItem{adoption}, true) {
		t.Fatal("an adopted conversation looked unused")
	}
	if !unusedAfterRestore("adopted-cleared", []core.CanonicalTranscriptItem{adoption, boundary}, true) {
		t.Fatal("a cleared adopted conversation looked used")
	}
	prompt := base.CanonicalSession.Transcript[0]

	if !unusedAfterRestore("never-prompted", []core.CanonicalTranscriptItem{}, true) {
		t.Fatal("a never-prompted session looked used")
	}
	if unusedAfterRestore("prompted", []core.CanonicalTranscriptItem{prompt}, true) {
		t.Fatal("a prompted session keeps its imported identity protected")
	}
	if !unusedAfterRestore("cleared", []core.CanonicalTranscriptItem{prompt, boundary}, true) {
		t.Fatal("a cleared session looked used")
	}
	if unusedAfterRestore("text-handoff", []core.CanonicalTranscriptItem{}, false) {
		t.Fatal("a restore that does not continue the native session says nothing about it")
	}
	missingNative := base
	missingNative.CanonicalSession.Transcript = []core.CanonicalTranscriptItem{adoption}
	checkpointNoError(t, os.Remove(filepath.Join(missingNative.HarnessHome, "sessions/2026/08/09/rollout-"+checkpointTestNative+".jsonl")))
	if _, err := ExportCheckpoint(&missingNative); err == nil || !strings.Contains(err.Error(), "no session artifacts found") {
		t.Fatalf("error = %v", err)
	}
}

func TestRestoreSeedsTheRelayFrontierAndCanDiscardQueuedPrompts(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)

	kept := restoredSeed(t, restoreInto(t, temp, &spec, false))
	if kept.EventFrontier != spec.CanonicalSession.EventFrontier || kept.EventFrontierDigest != spec.CanonicalSession.EventFrontierDigest {
		t.Fatalf("seed = %+v", kept)
	}
	if !checkpointSameJSON(t, kept.QueuedPrompts, spec.CanonicalSession.QueuedPrompts) {
		t.Fatalf("queued prompts = %+v", kept.QueuedPrompts)
	}

	relayRoot := restoreInto(t, temp, &spec, true)
	discarded := restoredSeed(t, relayRoot)
	if discarded.EventFrontier != spec.CanonicalSession.EventFrontier {
		t.Fatalf("event frontier = %d", discarded.EventFrontier)
	}
	if len(discarded.QueuedPrompts) != 0 {
		t.Fatalf("queued prompts = %+v", discarded.QueuedPrompts)
	}
	if checkpointExists(filepath.Join(relayRoot, "events.jsonl")) {
		t.Fatal("the restore wrote a relay event log")
	}
}

// Hard-won: 337bc5e review: an interrupted missing-only write became permanent truncated history.
func TestFailedRecoveryWriteNeverPublishesAPartialFileAndRetryKeepsConcurrentWork(t *testing.T) {
	directory := t.TempDir()
	destination := filepath.Join(directory, "native-history")
	data := bytes.Repeat([]byte("n"), 128*1024)
	if _, err := publishMissingPrivateFile(destination, 0o600, func(file *os.File) error {
		if _, err := file.Write(data[:70*1024]); err != nil {
			return err
		}
		return errors.New("injected write failure")
	}); err == nil {
		t.Fatal("a failed write published")
	}
	if checkpointExists(destination) {
		t.Fatal("a failed write left a partial file")
	}
	published := checkpointMust(publishMissingPrivateFile(destination, 0o600, func(file *os.File) error {
		_, err := file.Write(data)
		return err
	}))(t)
	if !published {
		t.Fatal("a complete write was not published")
	}
	if body := checkpointReadFile(t, destination); !bytes.Equal(body, data) {
		t.Fatal("the published file differs from what was written")
	}
	checkpointNoError(t, os.Remove(destination))
	published = checkpointMust(publishMissingPrivateFile(destination, 0o600, func(file *os.File) error {
		if _, err := file.Write(data); err != nil {
			return err
		}
		return os.WriteFile(destination, []byte("concurrent native work"), 0o644)
	}))(t)
	if published {
		t.Fatal("the recovery replaced concurrent work")
	}
	if body := checkpointReadFile(t, destination); string(body) != "concurrent native work" {
		t.Fatalf("destination = %q", body)
	}
}
