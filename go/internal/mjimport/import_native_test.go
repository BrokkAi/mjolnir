// Port of mj-import/src/import/native_tests.rs.
//
// Every test here reads its fixture's conversation through the SessionWiki
// adapters, so each skips at that read while they are not ported.

package mjimport

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type nativeTestFixture struct {
	kind            core.HarnessKind
	home            string
	sourcePath      string
	nativeSessionID string
	cwd             string
	transcript      ClaudeTranscript
}

func nativeTestConfig() core.Config {
	github := "example/app"
	config := core.DefaultConfig()
	config.Bundles = map[string]core.ProjectBundle{
		"app": {
			PrimaryRepo:  "app",
			Repositories: []core.ProjectRepository{{ID: "app", Github: &github, Destination: "app"}},
		},
	}
	config.Targets = map[string]core.TargetTemplate{
		"localhost": core.TargetTemplateLocalBare{},
		"podman":    core.TargetTemplateLocalPodman{ContainerTemplate: core.ContainerTemplate{Image: "agent-dev:latest"}},
	}
	return config
}

func nativeTestNewFixture(t *testing.T, root string, kind core.HarnessKind) nativeTestFixture {
	t.Helper()
	cwd := filepath.Join(root, "app")
	importTestInitializeRepository(t, cwd, "app")
	if kind != core.HarnessKindMuse {
		t.Fatalf("unsupported native test harness %s", importHarnessDebug(kind))
	}
	id := MuseID
	sourcePath := filepath.Join(root, "muse", ".data/muse/sessions/2026/09/08", id, "session.jsonl")
	home := filepath.Join(root, "muse")
	importTestMkdir(t, filepath.Dir(sourcePath))
	WriteMuseSession(sourcePath, id, cwd)
	importNeedsSessionwiki(t)
	transcript, err := ReadNativeTranscript(kind, sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	return nativeTestFixture{
		kind:            kind,
		home:            home,
		sourcePath:      sourcePath,
		nativeSessionID: id,
		cwd:             cwd,
		transcript:      transcript,
	}
}

func nativeTestProgressControl(cancelled *atomic.Bool) ImportControl {
	return ImportControl{
		Cancelled:        cancelled,
		Progress:         func(ImportArchiveProgress) {},
		IncludeUntracked: true,
	}
}

func nativeTestImportFixture(t *testing.T, fixture *nativeTestFixture, root string) ImportedClaudeSession {
	t.Helper()
	config := nativeTestConfig()
	state := core.NewStateState()
	archiveDirectory := filepath.Join(root, "archives")
	importTestMkdir(t, archiveDirectory)
	imported, err := ImportNativeSession(&config, &state, NativeImportRequest{
		Harness:          fixture.kind,
		HarnessHome:      fixture.home,
		NativeSessionID:  fixture.nativeSessionID,
		SourcePath:       fixture.sourcePath,
		Transcript:       &fixture.transcript,
		BundleID:         "app",
		ArchiveDirectory: archiveDirectory,
	}, nil)
	if err != nil {
		t.Fatal(err)
	}
	return imported
}

func TestNativeDispatchAndImportPreserveIdentityAndSourceForMuse(t *testing.T) {
	kind := core.HarnessKindMuse
	directory := t.TempDir()
	fixture := nativeTestNewFixture(t, directory, kind)
	original, err := os.ReadFile(fixture.sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	located, err := LocateNativeSession(kind, fixture.home, ClaudeSessionSelectionNativeSessionID(fixture.nativeSessionID))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, fixture.nativeSessionID)
	importTestEqual(t, located.SourcePath, fixture.sourcePath)
	transcript, err := ReadNativeTranscript(kind, located.SourcePath)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, transcript.Cwd, fixture.cwd)
	importTestEqual(t, transcript.EditedPaths, []string{"native-marker.txt"})

	listings := []NativeSessionListing{}
	if err := ScanNativeSessions(kind, fixture.home, NewNativeScanCache(), func(progress SessionScanProgress[NativeSessionListing]) {
		if progress.Session != nil {
			listings = append(listings, *progress.Session)
		}
	}); err != nil {
		t.Fatal(err)
	}
	if len(listings) != 1 {
		t.Fatal(len(listings))
	}
	importTestEqual(t, listings[0].NativeSessionID, fixture.nativeSessionID)
	if listings[0].UnavailableReason != nil {
		t.Fatal(*listings[0].UnavailableReason)
	}

	config := nativeTestConfig()
	state := core.NewStateState()
	archiveDirectory := filepath.Join(directory, "archives")
	importTestMkdir(t, archiveDirectory)
	imported, err := ImportNativeSession(&config, &state, NativeImportRequest{
		Harness:          kind,
		HarnessHome:      fixture.home,
		NativeSessionID:  fixture.nativeSessionID,
		SourcePath:       fixture.sourcePath,
		Transcript:       &transcript,
		BundleID:         "app",
		ArchiveDirectory: archiveDirectory,
	}, nil)
	if err != nil {
		t.Fatal(err)
	}
	record, _ := state.Sessions.Get(imported.SessionID)
	importTestEqual(t, record.State, core.SessionStateStopped)
	if record.NativeSessionID == nil || *record.NativeSessionID != fixture.nativeSessionID {
		t.Fatalf("%v", record.NativeSessionID)
	}
	verified, err := checkpoint.VerifyArchiveStreaming(imported.ArchivePath)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, verified.Manifest.Session.NativeSessionID, fixture.nativeSessionID)
	importTestNativePayload(t, &verified.Manifest, func(*core.PayloadDescriptor) bool { return true })
	after, err := os.ReadFile(fixture.sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(after, original) {
		t.Fatal("the native source changed")
	}
}

func TestNativeCheckpointRestoreRelocatesMuseWithoutChangingIdentity(t *testing.T) {
	kind := core.HarnessKindMuse
	directory := t.TempDir()
	fixture := nativeTestNewFixture(t, directory, kind)
	imported := nativeTestImportFixture(t, &fixture, directory)
	restoredWorkspace := filepath.Join(directory, "restored-workspace")
	targetCwd := filepath.Join(restoredWorkspace, "app")
	importTestMkdir(t, targetCwd)
	restoredHome := filepath.Join(directory, "restored-home")
	if err := checkpoint.RestoreCheckpoint(&checkpoint.CheckpointRestoreSpec{
		ReportRoot:            nil,
		ArchivePath:           imported.ArchivePath,
		WorkspaceRoot:         restoredWorkspace,
		RelayRoot:             filepath.Join(directory, "restored-relay"),
		HarnessHome:           restoredHome,
		RestoreRepositories:   false,
		RestoreNative:         true,
		QueuePolicy:           checkpoint.QueueRestorePolicyRestore,
		PrimaryRepositoryRoot: nil,
	}, checkpoint.SystemGit{}); err != nil {
		t.Fatal(err)
	}
	located, err := LocateNativeSession(kind, restoredHome, ClaudeSessionSelectionNativeSessionID(fixture.nativeSessionID))
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, located.NativeSessionID, fixture.nativeSessionID)
	transcript, err := ReadNativeTranscript(kind, located.SourcePath)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, transcript.Cwd, targetCwd)
	importTestEqual(t, transcript.EditedPaths, []string{"native-marker.txt"})
	found := false
	for _, item := range transcript.items {
		user, ok := item.Body.(core.TranscriptBodyUser)
		if !ok {
			continue
		}
		value, err := jsonvalue.Parse(user.Content[0])
		if err != nil {
			t.Fatal(err)
		}
		if text, ok := importJSONStr(importJSONGet(value, "text")); ok && strings.Contains(text, "import") {
			found = true
		}
	}
	if !found {
		t.Fatal("the imported prompt is missing")
	}
}

// Hard-won: 70c1a004a: file-backed Muse artifacts lost their captured content identity.
func TestNativeImportRejectsChangedPreviewAndCancellationWithoutPublishing(t *testing.T) {
	kind := core.HarnessKindMuse
	directory := t.TempDir()
	sourceFixture := nativeTestNewFixture(t, directory, kind)
	originalBytes, err := os.ReadFile(sourceFixture.sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	original := string(originalBytes)
	changed := strings.ReplaceAll(original, "hello back", "changed native reply")
	if changed == original {
		t.Fatal("the fixture did not change")
	}
	importTestWrite(t, sourceFixture.sourcePath, changed)
	config := nativeTestConfig()
	archiveDirectory := filepath.Join(directory, "stale-archives")
	state := core.NewStateState()
	var cancelled atomic.Bool
	control := nativeTestProgressControl(&cancelled)
	_, err = ImportNativeSession(&config, &state, NativeImportRequest{
		Harness:          kind,
		HarnessHome:      sourceFixture.home,
		NativeSessionID:  sourceFixture.nativeSessionID,
		SourcePath:       sourceFixture.sourcePath,
		Transcript:       &sourceFixture.transcript,
		BundleID:         "app",
		ArchiveDirectory: archiveDirectory,
	}, &control)
	importTestErrorContains(t, err, "native session changed", "")
	if state.Sessions.Len() != 0 {
		t.Fatal("a session was recorded")
	}
	nativeTestNothingPublished(t, archiveDirectory)

	directory = t.TempDir()
	cancelledFixture := nativeTestNewFixture(t, directory, kind)
	config = nativeTestConfig()
	archiveDirectory = filepath.Join(directory, "cancelled-archives")
	state = core.NewStateState()
	var cancelledNow atomic.Bool
	cancelledNow.Store(true)
	control = nativeTestProgressControl(&cancelledNow)
	_, err = ImportNativeSession(&config, &state, NativeImportRequest{
		Harness:          kind,
		HarnessHome:      cancelledFixture.home,
		NativeSessionID:  cancelledFixture.nativeSessionID,
		SourcePath:       cancelledFixture.sourcePath,
		Transcript:       &cancelledFixture.transcript,
		BundleID:         "app",
		ArchiveDirectory: archiveDirectory,
	}, &control)
	importTestErrorContains(t, err, "import cancelled", "")
	if state.Sessions.Len() != 0 {
		t.Fatal("a session was recorded")
	}
	nativeTestNothingPublished(t, archiveDirectory)

	// A write after validation must not replace the captured native bytes
	// that were paired with the preview transcript.
	directory = t.TempDir()
	sourceFixture = nativeTestNewFixture(t, directory, kind)
	reply := strings.Repeat("h", 128*1024)
	largeBytes, err := os.ReadFile(sourceFixture.sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	importTestWrite(t, sourceFixture.sourcePath, strings.ReplaceAll(string(largeBytes), "hello back", reply))
	sourceFixture.transcript, err = ReadNativeTranscript(kind, sourceFixture.sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	originalBytes, err = os.ReadFile(sourceFixture.sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	if len(originalBytes) <= 64*1024 {
		t.Fatal(len(originalBytes))
	}
	changed = strings.ReplaceAll(string(originalBytes), reply, "later native reply")
	config = nativeTestConfig()
	archiveDirectory = filepath.Join(directory, "snapshot-archives")
	state = core.NewStateState()
	var notCancelled atomic.Bool
	control = ImportControl{
		Cancelled: &notCancelled,
		Progress: func(progress ImportArchiveProgress) {
			if _, writing := progress.(ImportArchiveProgressWritingArchive); writing {
				if err := os.WriteFile(sourceFixture.sourcePath, []byte(changed), 0o666); err != nil {
					panic(err)
				}
			}
		},
		IncludeUntracked: true,
	}
	imported, err := ImportNativeSession(&config, &state, NativeImportRequest{
		Harness:          kind,
		HarnessHome:      sourceFixture.home,
		NativeSessionID:  sourceFixture.nativeSessionID,
		SourcePath:       sourceFixture.sourcePath,
		Transcript:       &sourceFixture.transcript,
		BundleID:         "app",
		ArchiveDirectory: archiveDirectory,
	}, &control)
	if err != nil {
		t.Fatal(err)
	}
	record, _ := state.Sessions.Get(imported.SessionID)
	checkpointMetadata := record.Checkpoint
	verified, err := checkpoint.VerifyArchiveStreaming(checkpointMetadata.ArchivePath)
	if err != nil {
		t.Fatal(err)
	}
	native := importTestNativePayload(t, &verified.Manifest, func(payload *core.PayloadDescriptor) bool {
		name, ok := importPathFileName(payload.Role.RelativePath)
		return ok && name == "session.jsonl"
	})
	payload, err := checkpoint.ReadVerifiedCheckpointPayload(checkpointMetadata.ArchivePath, native, native.Size)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(payload, originalBytes) {
		t.Fatal("the archive holds bytes written after capture")
	}
	current, err := os.ReadFile(sourceFixture.sourcePath)
	if err != nil {
		t.Fatal(err)
	}
	importTestEqual(t, string(current), changed)
}

// nativeTestNothingPublished checks that an archive directory is missing or
// empty.
func nativeTestNothingPublished(t *testing.T, archiveDirectory string) {
	t.Helper()
	entries, err := os.ReadDir(archiveDirectory)
	if err != nil && !os.IsNotExist(err) {
		t.Fatal(err)
	}
	if len(entries) != 0 {
		t.Fatalf("published %v", entries)
	}
}
