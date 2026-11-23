// Port of mj-checkpoint/src/checkpoint/tests.rs: image attachments, relay
// seed safety, archive schema refusal, sub-agent reports, large native
// files and missing-parent recovery.

package checkpoint

import (
	"bytes"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func imageCheckpointFixture(t testing.TB, temp string) (CheckpointExportSpec, core.AttachmentRef, []byte, string) {
	t.Helper()
	spec, _ := checkpointFixture(t, temp)
	image := checkpointMust(base64.StdEncoding.DecodeString(
		"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP4z8DwHwAFgAI/yZmaPdoAAAAASUVORK5CYII="))(t)
	reference := checkpointMust(core.NewAttachmentRef(image, "image/png", 1, 1))(t)
	store := core.AttachmentStoreWorker(spec.RelayRoot)
	checkpointNoError(t, store.Install(&reference, image))
	imageBlock := json.RawMessage(checkpointMust(json.Marshal(reference.ContentBlock()))(t))
	content := make([]json.RawMessage, 10)
	for i := range content {
		content[i] = imageBlock
	}
	spec.CanonicalSession.QueuedPrompts[0].Content = content
	checkpointMust(ExportCheckpoint(&spec))(t)
	if version := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t).Manifest.SchemaVersion; version != core.ArchiveSchemaVersionAttachments {
		t.Fatalf("schema version = %d", version)
	}

	restoredRelay := filepath.Join(temp, "restored-image-relay")
	checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           restoredRelay,
		HarnessHome:         filepath.Join(temp, "restored-image-harness"),
		RestoreRepositories: false,
		RestoreNative:       false,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{}))

	return spec, reference, image, restoredRelay
}

// decodeContentBlocks decodes queued ACP content as Rust's
// serde_json::from_value does for each block.
func decodeContentBlocks(t testing.TB, content []json.RawMessage) []acp.ContentBlock {
	t.Helper()
	blocks := make([]acp.ContentBlock, len(content))
	for i := range content {
		checkpointNoError(t, json.Unmarshal(content[i], &blocks[i]))
	}
	return blocks
}

// sameJSONValues compares JSON texts as decoded serde_json values.
func sameJSONValues(t testing.TB, left, right []json.RawMessage) bool {
	t.Helper()
	return slices.EqualFunc(left, right, func(a, b json.RawMessage) bool {
		return jsonvalue.Equal(checkpointMust(jsonvalue.Parse(a))(t), checkpointMust(jsonvalue.Parse(b))(t))
	})
}

func TestCheckpointRoundTripsTenImageReferencesAndRestoresTheBlob(t *testing.T) {
	temp := t.TempDir()
	spec, reference, image, restoredRelay := imageCheckpointFixture(t, temp)
	seed := restoredSeed(t, restoredRelay)

	if !sameJSONValues(t, seed.QueuedPrompts[0].Content, spec.CanonicalSession.QueuedPrompts[0].Content) {
		t.Fatal("the restored queue differs from the archived one")
	}
	blocks := decodeContentBlocks(t, seed.QueuedPrompts[0].Content)
	references := checkpointMust(core.References(blocks))(t)
	expected := make([]core.AttachmentRef, 10)
	for i := range expected {
		expected[i] = reference
	}
	if !reflect.DeepEqual(references, expected) {
		t.Fatalf("references = %+v", references)
	}

	store := core.AttachmentStoreWorker(restoredRelay)
	checkpointNoError(t, store.Resolve(blocks))
	for _, block := range blocks {
		if block.Image == nil {
			t.Fatal("checkpoint image queue contained a non-image block")
		}
		if block.Image.URI != nil {
			t.Fatalf("a resolved image kept its URI %s", *block.Image.URI)
		}
		if data := checkpointMust(base64.StdEncoding.DecodeString(block.Image.Data))(t); !bytes.Equal(data, image) {
			t.Fatal("the resolved image differs from the blob")
		}
	}
}

func TestRestoredImageResolutionRejectsMissingAndCorruptBlobs(t *testing.T) {
	temp := t.TempDir()
	_, reference, image, restoredRelay := imageCheckpointFixture(t, temp)
	store := core.AttachmentStoreWorker(restoredRelay)
	attachmentPath := filepath.Join(store.Root(), reference.Sha256)
	queued := restoredSeed(t, restoredRelay).QueuedPrompts[0].Content

	checkpointNoError(t, os.Remove(attachmentPath))
	if err := store.Resolve(decodeContentBlocks(t, queued)); err == nil {
		t.Fatal("a missing blob resolved")
	}

	corrupt := slices.Clone(image)
	corrupt[0] ^= 1
	checkpointWriteFile(t, attachmentPath, corrupt)
	if err := store.Resolve(decodeContentBlocks(t, queued)); err == nil {
		t.Fatal("a corrupt blob resolved")
	}
}

// The relay seed must stay proportional to the queue, never to the
// conversation: a long session used to write its whole transcript into the
// target's relay root for three fields nobody else read.
// Hard-won: a3aa9745: restore wrote a 47 MB canonical transcript although relay startup consumed only a few fields.
func TestTheRelaySeedDoesNotGrowWithTheTranscript(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	item := spec.CanonicalSession.Transcript[0]
	text := strings.Repeat("x", 256)
	transcript := make([]core.CanonicalTranscriptItem, 0, 20_000)
	for position := uint64(1); position <= 20_000; position++ {
		next := item
		next.StableID = "user-" + strconv.FormatUint(position, 10)
		next.Position = position
		next.Body = core.CanonicalTranscriptBodyUser{
			Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"` + text + `"}`)},
		}
		transcript = append(transcript, next)
	}
	spec.CanonicalSession.Transcript = transcript
	spec.CanonicalSession.EventFrontier = 20_000
	checkpointMust(ExportCheckpoint(&spec))(t)

	relayRoot := restoreInto(t, temp, &spec, false)
	seed := checkpointMust(os.Stat(core.RestoredRelaySeedPath(relayRoot)))(t).Size()

	if seed >= 4096 {
		t.Fatalf("the relay seed embedded the transcript: %d bytes", seed)
	}
	if queued := restoredSeed(t, relayRoot).QueuedPrompts; !checkpointSameJSON(t, queued, spec.CanonicalSession.QueuedPrompts) {
		t.Fatalf("queued prompts = %+v", queued)
	}
}

// A restore seeds a relay that has none of its own state yet. Existing state
// means the previous worker was never fully torn down, and the seed would
// silently lose to it.
// Hard-won: 2e7f5ec9: a leaked worker snapshot could overwrite the restored relay seed and make resume fail.
func TestRestoreRefusesARelayRootThatAlreadyHoldsRelayState(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)
	relayRoot := filepath.Join(temp, "occupied-relay")
	checkpointMkdirAll(t, relayRoot)
	checkpointWriteFile(t, filepath.Join(relayRoot, core.RelayStateFile), []byte(`{"format_version":1}`))

	err := RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           relayRoot,
		HarnessHome:         filepath.Join(temp, "restored-harness"),
		RestoreRepositories: false,
		RestoreNative:       false,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{})

	if err == nil || !strings.Contains(err.Error(), "relay state already present") {
		t.Fatalf("error = %v", err)
	}
	if checkpointExists(core.RestoredRelaySeedPath(relayRoot)) {
		t.Fatal("a refused restore wrote the relay seed")
	}
}

// Path::exists follows links, so a *dangling* symlink at the seed path reads
// as "no file here" and used to send the write to the link target.
// Hard-won: 37432b73: private restore followed a dangling symlink instead of refusing its target.
func TestRestoreRefusesToSeedTheRelayThroughADanglingSymlink(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix symlinks")
	}
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)
	relayRoot := filepath.Join(temp, "symlinked-relay")
	checkpointMkdirAll(t, relayRoot)
	outside := filepath.Join(temp, "outside/seed.json")
	checkpointMkdirAll(t, filepath.Dir(outside))
	checkpointNoError(t, os.Symlink(outside, filepath.Join(relayRoot, core.RestoredRelaySeedFile)))

	err := RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           relayRoot,
		HarnessHome:         filepath.Join(temp, "restored-harness"),
		RestoreRepositories: false,
		RestoreNative:       false,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{})

	if err == nil || !strings.Contains(err.Error(), "symlink") {
		t.Fatalf("error = %v", err)
	}
	if checkpointExists(outside) {
		t.Fatal("the relay seed was written through the symlink")
	}
}

// Hard-won: 37432b73: private restore wrote through a symlinked ancestor into an unintended path.
func TestRestoreRefusesANativeArtifactUnderASymlinkedDirectory(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix symlinks")
	}
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)
	harnessHome := filepath.Join(temp, "symlinked-harness")
	checkpointMkdirAll(t, harnessHome)
	outside := filepath.Join(temp, "outside-sessions")
	checkpointMkdirAll(t, outside)
	checkpointNoError(t, os.Symlink(outside, filepath.Join(harnessHome, "sessions")))

	err := RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           filepath.Join(temp, "symlinked-native-relay"),
		HarnessHome:         harnessHome,
		RestoreRepositories: false,
		RestoreNative:       true,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{})

	if err == nil || !strings.Contains(err.Error(), "traverses a symlink") {
		t.Fatalf("error = %v", err)
	}
	if entries := checkpointMust(os.ReadDir(outside))(t); len(entries) != 0 {
		t.Fatal("the restore wrote through the symlinked directory")
	}
}

func TestRestoreWritesNativeArtifactsPrivatelyUnderTheHarnessHome(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	childRelative := "sessions/2026/08/09/rollout-child.jsonl"
	writeModernRollout(t, filepath.Join(spec.HarnessHome, childRelative), checkpointTestOtherNative, checkpointTestNative)
	checkpointMust(ExportCheckpoint(&spec))(t)
	archive := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	if session := checkpointMust(archive.CanonicalSession())(t); !checkpointSameJSON(t, session, spec.CanonicalSession) {
		t.Fatalf("canonical session = %+v", session)
	}
	var native []string
	for _, payload := range archive.Manifest.Payloads {
		if payload.Role.Kind == core.PayloadRoleNativeArtifact {
			native = append(native, payload.Role.RelativePath)
		}
	}
	if !checkpointSamePaths(native, []string{"sessions/2026/08/09/rollout-" + checkpointTestNative + ".jsonl"}) {
		t.Fatalf("native artifacts = %v", native)
	}
	harnessHome := filepath.Join(temp, "restored-native-harness")

	checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           filepath.Join(temp, "restored-native-relay"),
		HarnessHome:         harnessHome,
		RestoreRepositories: false,
		RestoreNative:       true,
		QueuePolicy:         QueueRestorePolicyRestore,
	}, SystemGit{}))

	restored := filepath.Join(harnessHome, "sessions/2026/08/09/rollout-"+checkpointTestNative+".jsonl")
	if body := checkpointReadFile(t, restored); string(body) != "native" {
		t.Fatalf("restored = %q", body)
	}
	if checkpointExists(filepath.Join(harnessHome, childRelative)) {
		t.Fatal("a child thread's rollout was restored")
	}
	if runtime.GOOS != "windows" {
		if mode := checkpointMust(os.Stat(restored))(t).Mode().Perm(); mode&0o077 != 0 {
			t.Fatalf("restored native artifact is group- or world-accessible: %o", mode)
		}
	}
}

func TestIncompatibleSchemaIsRejectedBeforeRestoreMutatesTarget(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)
	readme := filepath.Join(spec.WorkspaceRoot, "app/README.md")
	before := checkpointReadFile(t, readme)

	for _, schemaVersion := range []uint32{
		1,
		core.ArchiveSchemaVersion + 1,
		core.ArchiveSchemaVersionNativeUse + 1,
	} {
		incompatible := filepath.Join(temp, fmt.Sprintf("incompatible-%d.hel.zip", schemaVersion))
		copyArchiveWithSchema(t, spec.OutputPath, incompatible, schemaVersion)
		relayRoot := filepath.Join(temp, fmt.Sprintf("relay-%d", schemaVersion))
		err := RestoreCheckpoint(&CheckpointRestoreSpec{
			ArchivePath:         incompatible,
			WorkspaceRoot:       spec.WorkspaceRoot,
			RelayRoot:           relayRoot,
			HarnessHome:         filepath.Join(temp, "restored-harness"),
			RestoreRepositories: true,
			RestoreNative:       true,
			QueuePolicy:         QueueRestorePolicyRestore,
		}, SystemGit{})

		if err == nil || !strings.Contains(err.Error(), "incompatible Mjolnir archive schema") {
			t.Fatalf("schema %d: error = %v", schemaVersion, err)
		}
		if after := checkpointReadFile(t, readme); !bytes.Equal(after, before) {
			t.Fatalf("schema %d: the workspace changed", schemaVersion)
		}
		if checkpointExists(relayRoot) {
			t.Fatalf("schema %d: the relay root was written", schemaVersion)
		}
	}
}

func TestNonIdleCanonicalSessionIsRejectedBeforeRestoreMutatesTarget(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)
	invalid := spec.CanonicalSession
	invalid.Session.Execution = core.CanonicalExecutionState{State: core.CanonicalExecutionStateRunning, StartedAtMs: 3}

	assertCanonicalRestoreRejectedBeforeMutation(t, &spec, filepath.Join(temp, "non-idle.hel.zip"), &invalid, "not idle at the checkpoint barrier")
}

func TestInvalidFrontierDigestIsRejectedBeforeRestoreMutatesTarget(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)
	invalid := spec.CanonicalSession
	invalid.EventFrontierDigest = strings.Repeat("A", 64)

	assertCanonicalRestoreRejectedBeforeMutation(t, &spec, filepath.Join(temp, "invalid-digest.hel.zip"), &invalid, "64 lowercase hexadecimal characters")
}

func TestUnrestorableQueueIsRejectedBeforeRestoreMutatesTarget(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)

	empty := spec.CanonicalSession
	empty.QueuedPrompts = slices.Clone(empty.QueuedPrompts)
	empty.QueuedPrompts[0].Content = []json.RawMessage{}
	assertCanonicalRestoreRejectedBeforeMutation(t, &spec, filepath.Join(temp, "empty-prompt.hel.zip"), &empty, "has no content")

	malformed := spec.CanonicalSession
	malformed.QueuedPrompts = slices.Clone(malformed.QueuedPrompts)
	malformed.QueuedPrompts[0].Content = []json.RawMessage{json.RawMessage(`{"type":"not_an_acp_block"}`)}
	assertCanonicalRestoreRejectedBeforeMutation(t, &spec, filepath.Join(temp, "malformed-prompt.hel.zip"), &malformed, "has invalid ACP content block 0")
}

func TestParallelRepositoryCollectionPreservesManifestOrder(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	gitLine(spec.WorkspaceRoot, "clone", "-q", "app", "worker")
	worker := filepath.Join(spec.WorkspaceRoot, "worker")
	base := gitLine(worker, "rev-parse", "HEAD")
	spec.Repositories = append(spec.Repositories, CheckpointRepositorySpec{
		ID:                  "worker",
		RelativeDestination: "worker",
		Capture:             CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureDeltaFrom, BaseCommit: base},
	})

	checkpointMust(ExportCheckpoint(&spec))(t)
	verified := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	var ids []string
	for _, repository := range verified.Manifest.Repositories {
		ids = append(ids, repository.Metadata.ID)
	}
	if !slices.Equal(ids, []string{"app", "worker"}) {
		t.Fatalf("repositories = %v", ids)
	}
}

// A child's report files sit beside the repositories, outside all of them,
// so no repository capture sees them. The checkpoint carries them itself and
// puts them back even when the harness history is not restored.
// Hard-won: b63fedbf31: a report named config.json blocked Move during checkpoint export.
func TestCheckpointCarriesSubagentReportsAndRestoresThemBesideTheRepositories(t *testing.T) {
	for _, explicit := range []bool{false, true} {
		temp := t.TempDir()
		spec, _ := checkpointFixture(t, temp)
		relativeRoot := core.ReportRootDir
		if explicit {
			relativeRoot = core.Join("app", core.ProjectReportRootDir)
		}
		reports := filepath.Join(spec.WorkspaceRoot, relativeRoot, "child-1")
		if explicit {
			// Bare projects exclude this internal root when preparing
			// reports.
			exclude := checkpointMust(os.OpenFile(filepath.Join(spec.WorkspaceRoot, "app/.git/info/exclude"), os.O_APPEND|os.O_WRONLY, 0))(t)
			checkpointMust(exclude.Write([]byte("\n/.mj/agents/\n")))(t)
			checkpointNoError(t, exclude.Close())
			spec.ReportDirectories = &[]string{reports}
			unrelated := filepath.Join(spec.WorkspaceRoot, relativeRoot, "another-parents-child")
			checkpointMkdirAll(t, unrelated)
			checkpointWriteFile(t, filepath.Join(unrelated, "private.md"), []byte("another parent's report"))
		}
		checkpointMkdirAll(t, filepath.Join(reports, "logs"))
		checkpointWriteFile(t, filepath.Join(reports, "findings.md"), []byte("full findings"))
		checkpointWriteFile(t, filepath.Join(reports, "logs/test.log"), []byte("test output"))
		checkpointMkdirAll(t, filepath.Join(reports, "L4-flow"))
		checkpointWriteFile(t, filepath.Join(reports, "L4-flow/config.json"), []byte(`{"flow":4}`))
		checkpointMust(ExportCheckpoint(&spec))(t)
		if version := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t).Manifest.SchemaVersion; version != core.ArchiveSchemaVersionAgentReports {
			t.Fatalf("explicit %v: schema version = %d", explicit, version)
		}

		restoredWorkspace := filepath.Join(temp, "restored-workspace")
		checkpointMkdirAll(t, restoredWorkspace)
		// A file already on the target is newer than the archive's copy.
		kept := filepath.Join(restoredWorkspace, relativeRoot, "child-1/findings.md")
		checkpointMkdirAll(t, filepath.Dir(kept))
		checkpointWriteFile(t, kept, []byte("newer findings"))
		var reportRoot *string
		if explicit {
			reportRoot = ptrTo(filepath.Join(restoredWorkspace, relativeRoot))
		}
		checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
			ReportRoot:          reportRoot,
			ArchivePath:         spec.OutputPath,
			WorkspaceRoot:       restoredWorkspace,
			RelayRoot:           filepath.Join(temp, "restored-report-relay"),
			HarnessHome:         filepath.Join(temp, "restored-report-harness"),
			RestoreRepositories: false,
			RestoreNative:       false,
			QueuePolicy:         QueueRestorePolicyRestore,
		}, SystemGit{}))
		if checkpointExists(filepath.Join(restoredWorkspace, relativeRoot, "another-parents-child")) {
			t.Fatalf("explicit %v: another parent's reports were restored", explicit)
		}
		restored := filepath.Join(restoredWorkspace, relativeRoot, "child-1")
		for path, expected := range map[string]string{
			"logs/test.log":       "test output",
			"findings.md":         "newer findings",
			"L4-flow/config.json": `{"flow":4}`,
		} {
			if body := checkpointReadFile(t, filepath.Join(restored, path)); string(body) != expected {
				t.Fatalf("explicit %v: %s = %q", explicit, path, body)
			}
		}

		if runtime.GOOS != "windows" && explicit {
			unsafeWorkspace := filepath.Join(temp, "unsafe-workspace")
			outside := filepath.Join(temp, "outside")
			checkpointMkdirAll(t, filepath.Join(unsafeWorkspace, "app"))
			checkpointMkdirAll(t, outside)
			checkpointNoError(t, os.Symlink(outside, filepath.Join(unsafeWorkspace, "app/.mj")))
			unsafeRoot := filepath.Join(unsafeWorkspace, relativeRoot)
			err := RestoreCheckpoint(&CheckpointRestoreSpec{
				ArchivePath:         spec.OutputPath,
				ReportRoot:          &unsafeRoot,
				WorkspaceRoot:       unsafeWorkspace,
				RelayRoot:           filepath.Join(temp, "unsafe-relay"),
				HarnessHome:         filepath.Join(temp, "unsafe-harness"),
				RestoreRepositories: false,
				RestoreNative:       false,
				QueuePolicy:         QueueRestorePolicyRestore,
			}, SystemGit{})
			if err == nil || !strings.Contains(err.Error(), "symlink") {
				t.Fatalf("error = %v", err)
			}
			if entries := checkpointMust(os.ReadDir(outside))(t); len(entries) != 0 {
				t.Fatal("the restore wrote through the symlinked directory")
			}
		}

		// A child sees exactly the same workspace, but its own launch
		// identity prevents it from archiving the parent's (and siblings')
		// reports.
		checkpointWriteFile(t, filepath.Join(spec.RelayRoot, "launch.json"), []byte(`{"handback_tool":true}`))
		child := spec
		child.OutputPath = filepath.Join(spec.RelayRoot, "child.zip")
		checkpointMust(ExportCheckpoint(&child))(t)
		childArchive := checkpointMust(ReadArchiveVerified(child.OutputPath))(t)
		for _, artifact := range childArchive.Manifest.Payloads {
			if artifact.Role.Kind == core.PayloadRoleNativeArtifact && checkpointPathStartsWith(artifact.Role.RelativePath, core.ArchiveReportDir) {
				t.Fatalf("explicit %v: a child archived %s", explicit, artifact.Role.RelativePath)
			}
		}
	}
}

func TestSubagentReportsPastTheLimitKeepTheNewestFiles(t *testing.T) {
	temp := t.TempDir()
	if artifacts, _, err := collectSubagentReports(temp, nil); err != nil || len(artifacts) != 0 {
		t.Fatalf("no report directory, nothing to carry: %v, %v", artifactPaths(artifacts), err)
	}
	reports := filepath.Join(temp, core.ReportRootDir, "c")
	checkpointMkdirAll(t, reports)
	half := int(core.MaxCheckpointReportBytes/2 + 1)
	old := filepath.Join(reports, "old.log")
	checkpointWriteFile(t, old, bytes.Repeat([]byte("o"), half))
	checkpointNoError(t, os.Chtimes(old, time.Time{}, time.Unix(1, 0)))
	checkpointWriteFile(t, filepath.Join(reports, "new.log"), bytes.Repeat([]byte("n"), half))
	if runtime.GOOS != "windows" {
		checkpointNoError(t, os.Symlink(old, filepath.Join(reports, "link.log")))
	}
	artifacts, omitted := func() ([]core.NativeArtifact, CheckpointReportOmissions) {
		artifacts, omitted, err := collectSubagentReports(temp, nil)
		checkpointNoError(t, err)
		return artifacts, omitted
	}()
	if omitted.Files != 1 || omitted.Bytes != uint64(half) {
		t.Fatalf("omitted = %+v", omitted)
	}
	if omitted.LargestDirectories[0].ChildID != "c" || omitted.LargestDirectories[0].Bytes != 2*uint64(half) {
		t.Fatalf("largest directories = %+v", omitted.LargestDirectories)
	}
	if !checkpointSamePaths(artifactPaths(artifacts), []string{core.Join(core.ArchiveReportDir, "c/new.log")}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

func TestAReportWrittenAfterThePrestageChangesTheSourceFingerprint(t *testing.T) {
	temp := t.TempDir()
	harness := filepath.Join(temp, "harness")
	workspace := filepath.Join(temp, "workspace")
	checkpointMkdirAll(t, harness)
	checkpointMkdirAll(t, workspace)
	before := checkpointMust(checkpointSourceFingerprint(harness, workspace, nil))(t)
	if own := checkpointMust(nativeSourceFingerprint(harness))(t); before != own {
		t.Fatal("without reports the fingerprint is the harness home's own")
	}
	reports := filepath.Join(workspace, core.ReportRootDir, "c")
	checkpointMkdirAll(t, reports)
	checkpointWriteFile(t, filepath.Join(reports, "r.md"), []byte("report"))
	if after := checkpointMust(checkpointSourceFingerprint(harness, workspace, nil))(t); after == before {
		t.Fatal("a new report did not change the fingerprint")
	}
	directories := []string{filepath.Join(workspace, "project/.mj/agents/owned")}
	checkpointMkdirAll(t, directories[0])
	selected := checkpointMust(checkpointSourceFingerprint(harness, workspace, &directories))(t)
	checkpointWriteFile(t, filepath.Join(reports, "unrelated.md"), []byte("another parent's report"))
	if after := checkpointMust(checkpointSourceFingerprint(harness, workspace, &directories))(t); after != selected {
		t.Fatal("another parent's report changed the fingerprint")
	}
	checkpointWriteFile(t, filepath.Join(directories[0], "report.md"), []byte("our report"))
	if after := checkpointMust(checkpointSourceFingerprint(harness, workspace, &directories))(t); after == selected {
		t.Fatal("our own report did not change the fingerprint")
	}
}

func TestCapturedCheckpointAcceptsAnOlderWorkersJSON(t *testing.T) {
	var captured CapturedCheckpoint
	checkpointNoError(t, json.Unmarshal([]byte(`{"stage_path":"stage","native_bytes":12,"repository_bytes":34,"reused_native":true}`), &captured))
	if !reflect.DeepEqual(captured.OmittedReports, CheckpointReportOmissions{}) {
		t.Fatalf("omitted reports = %+v", captured.OmittedReports)
	}
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	encoded := checkpointMust(json.Marshal(spec))(t)
	var members map[string]json.RawMessage
	checkpointNoError(t, json.Unmarshal(encoded, &members))
	if _, ok := members["report_directories"]; ok {
		t.Fatalf("encoded = %s", encoded)
	}
	var decoded CheckpointExportSpec
	checkpointNoError(t, json.Unmarshal(encoded, &decoded))
	if decoded.ReportDirectories != nil {
		t.Fatalf("report directories = %v", *decoded.ReportDirectories)
	}
}

// Hard-won: move-a618a54a: a 1 GiB native-file cap made a quota Move of a long Claude session fail
func TestQuotaMoveStreamsANativeTranscriptLargerThanOneGiB(t *testing.T) {
	started := time.Now()
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	spec.Session.HarnessKind = core.HarnessKindClaude
	relative := "projects/old/" + checkpointTestNative + ".jsonl"
	source := filepath.Join(spec.HarnessHome, relative)
	checkpointMkdirAll(t, filepath.Dir(source))
	file := checkpointMust(os.Create(source))(t)
	checkpointNoError(t, file.Truncate(1024*1024*1024+65537))
	checkpointMust(file.Write([]byte("transcript start\n")))(t)
	checkpointMust(file.Seek(-16, io.SeekEnd))(t)
	checkpointMust(file.Write([]byte("transcript end!\n")))(t)
	checkpointNoError(t, file.Close())
	checkpointMust(ExportCheckpoint(&spec))(t)
	checkpointMust(VerifyArchiveStreaming(spec.OutputPath))(t)
	stagePath := filepath.Join(spec.RelayRoot, "large-native-stage")
	captureSpec := captureSpecFromExport(&spec, stagePath)
	checkpointMust(CaptureCheckpoint(&captureSpec, SystemGit{}))(t)
	checkpointMust(PackCheckpoint(&CheckpointPackSpec{
		ProtocolVersion:  CheckpointStagingProtocolVersion,
		RelayRoot:        spec.RelayRoot,
		StagePath:        stagePath,
		CanonicalSession: spec.CanonicalSession,
		OutputPath:       spec.OutputPath,
	}))(t)
	repositoryArchive := checkpointMust(readArchiveRepositories(spec.OutputPath))(t)
	for _, payload := range repositoryArchive.Manifest.Payloads {
		if payload.Role.Kind == core.PayloadRoleNativeArtifact {
			if _, ok := repositoryArchive.Payloads[payload.Path]; ok {
				t.Fatalf("the repository read retained native payload %s", payload.Path)
			}
		}
	}
	home := filepath.Join(temp, "restored-home")
	checkpointNoError(t, RestoreCheckpoint(&CheckpointRestoreSpec{
		ArchivePath:         spec.OutputPath,
		WorkspaceRoot:       spec.WorkspaceRoot,
		RelayRoot:           filepath.Join(temp, "restored-relay"),
		HarnessHome:         home,
		RestoreRepositories: false,
		RestoreNative:       true,
		QueuePolicy:         QueueRestorePolicyDiscard,
	}, SystemGit{}))
	app := filepath.Join(spec.WorkspaceRoot, "app")
	restoredRelative := checkpointMust(restoredNativeRelativePath(core.HarnessKindClaude, relative, &app))(t)
	digest := func(path string) []byte {
		file := checkpointMust(os.Open(path))(t)
		defer file.Close()
		hash := sha256.New()
		checkpointMust(io.CopyBuffer(hash, file, make([]byte, 65536)))(t)
		return hash.Sum(nil)
	}
	restored := filepath.Join(home, restoredRelative)
	if checkpointMust(os.Stat(source))(t).Size() != checkpointMust(os.Stat(restored))(t).Size() {
		t.Fatal("the restored transcript has another size")
	}
	if !bytes.Equal(digest(source), digest(restored)) {
		t.Fatal("the restored transcript differs")
	}
	fmt.Fprintf(os.Stderr, "large native checkpoint round trip: %v\n", time.Since(started))
}

// Hard-won: 055d8a772 review: parent runtime loss must not rewind a retained checkout or child state.
func TestMissingParentRestorePreservesCurrentComponentsAndRetriesPartialFailure(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix symlinks")
	}
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	nativeRelative := "sessions/2026/08/09/rollout-" + checkpointTestNative + ".jsonl"
	archivedNative := bytes.Repeat([]byte("n"), 128*1024)
	checkpointWriteFile(t, filepath.Join(spec.HarnessHome, nativeRelative), archivedNative)
	checkpointMust(ExportCheckpoint(&spec))(t)
	checkout := filepath.Join(spec.WorkspaceRoot, "app")
	current := filepath.Join(checkout, "README.md")
	checkpointWriteFile(t, current, []byte("current work after checkpoint"))
	index := gitLine(checkout, "ls-files", "--stage")
	childReport := filepath.Join(spec.WorkspaceRoot, ".mj/agents/child/report.txt")
	checkpointMkdirAll(t, filepath.Dir(childReport))
	checkpointWriteFile(t, childReport, []byte("current child report"))
	restore := CheckpointRestoreSpec{
		ArchivePath:           spec.OutputPath,
		WorkspaceRoot:         spec.WorkspaceRoot,
		ReportRoot:            ptrTo(filepath.Join(spec.WorkspaceRoot, ".mj/agents")),
		RelayRoot:             filepath.Join(temp, "recovered-parent"),
		HarnessHome:           filepath.Join(temp, "recovered-native"),
		RestoreRepositories:   false,
		RestoreNative:         true,
		QueuePolicy:           QueueRestorePolicyRestore,
		PrimaryRepositoryRoot: &checkout,
	}
	checkpointMkdirAll(t, restore.RelayRoot)
	seed := filepath.Join(restore.RelayRoot, core.RestoredRelaySeedFile)
	currentProjection := spec.CanonicalSession
	currentProjection.EventFrontier = 27
	currentProjection.EventFrontierDigest = strings.Repeat("b", 64)
	currentProjection.QueuedPrompts = []core.CanonicalQueuedPrompt{}
	currentSeed := checkpointMust(RelaySeedForRestore(&currentProjection, true, QueueRestorePolicyRestore))(t)
	checkpointNoError(t, os.Symlink(current, seed))
	if _, err := RestoreMissingParentCheckpoint(&restore, noNativeCheckpointState{}, &currentSeed); err == nil {
		t.Fatal("the seed was written through a symlink")
	}
	if body := checkpointReadFile(t, current); string(body) != "current work after checkpoint" {
		t.Fatalf("current work = %q", body)
	}
	if body := checkpointReadFile(t, filepath.Join(restore.HarnessHome, nativeRelative)); !bytes.Equal(body, archivedNative) {
		t.Fatal("the native history was not recovered")
	}
	checkpointNoError(t, os.Remove(seed))
	// Hard-won: 98a60381e: a leftover seed must not replay completed work after journal loss.
	leftover := checkpointMust(RelaySeedForRestore(&spec.CanonicalSession, true, QueueRestorePolicyRestore))(t)
	checkpointWriteFile(t, seed, checkpointMust(json.Marshal(leftover))(t))
	retry := checkpointMust(RestoreMissingParentCheckpoint(&restore, noNativeCheckpointState{}, &currentSeed))(t)
	if retry.NativeRestored || !retry.RelayRestored {
		t.Fatalf("retry = %+v", retry)
	}
	var recoveredSeed core.RestoredRelaySeed
	checkpointNoError(t, json.Unmarshal(checkpointReadFile(t, seed), &recoveredSeed))
	if recoveredSeed.EventFrontier != 27 {
		t.Fatalf("event frontier = %d", recoveredSeed.EventFrontier)
	}
	if len(recoveredSeed.QueuedPrompts) != 0 {
		t.Fatal("the archive must not replay already completed queued work")
	}
	checkpointWriteFile(t, filepath.Join(restore.HarnessHome, nativeRelative), []byte("newer native history"))
	relay := filepath.Join(restore.RelayRoot, core.RelayStateFile)
	checkpointWriteFile(t, relay, []byte("current relay state"))
	repeated := checkpointMust(RestoreMissingParentCheckpoint(&restore, noNativeCheckpointState{}, &currentSeed))(t)
	if repeated.NativeRestored || repeated.RelayRestored {
		t.Fatalf("repeated = %+v", repeated)
	}
	if body := checkpointReadFile(t, relay); string(body) != "current relay state" {
		t.Fatalf("relay state = %q", body)
	}
	if body := checkpointReadFile(t, filepath.Join(restore.HarnessHome, nativeRelative)); string(body) != "newer native history" {
		t.Fatalf("native history = %q", body)
	}
	if body := checkpointReadFile(t, current); string(body) != "current work after checkpoint" {
		t.Fatalf("current work = %q", body)
	}
	if body := checkpointReadFile(t, childReport); string(body) != "current child report" {
		t.Fatalf("child report = %q", body)
	}
	if got := gitLine(checkout, "ls-files", "--stage"); got != index {
		t.Fatalf("index = %s, want %s", got, index)
	}
	fromStore := restore
	fromStore.ArchivePath = filepath.Join(temp, "no-recovery-archive")
	fromStore.RelayRoot = filepath.Join(temp, "runtime-from-store")
	fromStore.RestoreNative = false
	result := checkpointMust(RestoreMissingParentCheckpoint(&fromStore, noNativeCheckpointState{}, &currentSeed))(t)
	if !result.RelayRestored || result.NativeRestored {
		t.Fatalf("result = %+v", result)
	}
	if body := checkpointReadFile(t, filepath.Join(fromStore.HarnessHome, nativeRelative)); string(body) != "newer native history" {
		t.Fatalf("native history = %q", body)
	}
}

// Hard-won: bd6363001 review: publishing a SQLite database before its WAL made retry silently lose conversation rows.
func TestMissingSqliteRecoveryPublishesACompleteWalSnapshotBeforeOtherArtifactFailures(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix symlinks")
	}
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	checkpointMust(ExportCheckpoint(&spec))(t)
	original := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
	source := filepath.Join(temp, "legacy.db")
	live := openTestDatabase(t, source)
	execTestDatabase(t, live, "PRAGMA journal_mode=WAL")
	execTestDatabase(t, live, "PRAGMA wal_autocheckpoint=0")
	execTestDatabase(t, live, "CREATE TABLE conversation(id TEXT PRIMARY KEY, body TEXT)")
	execTestDatabase(t, live, "PRAGMA wal_checkpoint(TRUNCATE)")
	body := strings.Repeat("conversation only in WAL ", 8192)
	execTestDatabase(t, live, "INSERT INTO conversation VALUES ('wal-only', ?1)", body)
	wal := checkpointReadFile(t, filepath.Join(temp, "legacy.db-wal"))
	if len(wal) <= 64*1024 {
		t.Fatalf("the log holds only %d bytes", len(wal))
	}
	session := original.Manifest.Session
	session.HarnessKind = core.HarnessKindOpenCode
	relative := ".data/opencode/opencode.db"
	legacy := filepath.Join(temp, "legacy.hel.zip")
	repositories := make([]RepositorySnapshot, 0, len(original.Manifest.Repositories))
	for i := range original.Manifest.Repositories {
		repositories = append(repositories, checkpointMust(archivedRepositorySnapshot(&original, &original.Manifest.Repositories[i]))(t))
	}
	checkpointMust(WriteArchiveHashed(legacy, &ArchiveInput{
		Session:          session,
		Target:           original.Manifest.Target,
		Bundle:           original.Manifest.Bundle,
		CanonicalSession: checkpointMust(original.CanonicalSession())(t),
		Repositories:     repositories,
		NativeArtifacts: []core.NativeArtifact{
			{RelativePath: relative, Data: core.NativeArtifactDataBytes(checkpointReadFile(t, source)), Mode: 0o600},
			{RelativePath: ".data/opencode/opencode.db-wal", Data: core.NativeArtifactDataBytes(wal), Mode: 0o600},
			{RelativePath: "blocked/late-artifact", Data: core.NativeArtifactDataBytes([]byte("last artifact")), Mode: 0o600},
		},
	}))(t)
	checkpointNoError(t, live.Close())
	home := filepath.Join(temp, "recovered-home")
	checkpointMkdirAll(t, home)
	current := filepath.Join(temp, "current-files")
	checkpointNoError(t, os.Mkdir(current, 0o755))
	checkpointNoError(t, os.Symlink(current, filepath.Join(home, "blocked")))
	app := filepath.Join(spec.WorkspaceRoot, "app")
	restore := CheckpointRestoreSpec{
		ArchivePath:           legacy,
		WorkspaceRoot:         spec.WorkspaceRoot,
		RelayRoot:             filepath.Join(temp, "recovered-relay"),
		HarnessHome:           home,
		RestoreRepositories:   false,
		RestoreNative:         true,
		QueuePolicy:           QueueRestorePolicyDiscard,
		PrimaryRepositoryRoot: &app,
	}
	if _, err := RestoreMissingParentCheckpoint(&restore, noNativeCheckpointState{}, nil); err == nil {
		t.Fatal("an artifact under a symlinked directory restored")
	}
	if info, err := os.Stat(filepath.Join(home, relative)); err != nil || !info.Mode().IsRegular() {
		t.Fatalf("the database was not published: %v", err)
	}
	for _, sidecar := range []string{".data/opencode/opencode.db-wal", ".data/opencode/opencode.db-shm"} {
		if checkpointExists(filepath.Join(home, sidecar)) {
			t.Fatalf("%s was published beside the collapsed database", sidecar)
		}
	}
	readBody := func(query string) string {
		database := openTestDatabase(t, filepath.Join(home, relative))
		defer database.Close()
		var value string
		checkpointNoError(t, database.QueryRow(query).Scan(&value))
		return value
	}
	if got := readBody("SELECT body FROM conversation WHERE id='wal-only'"); got != body {
		t.Fatal("the recovered database lost the rows only in the log")
	}
	checkpointNoError(t, os.Remove(filepath.Join(home, "blocked")))
	checkpointMust(RestoreMissingParentCheckpoint(&restore, noNativeCheckpointState{}, nil))(t)
	database := openTestDatabase(t, filepath.Join(home, relative))
	execTestDatabase(t, database, "UPDATE conversation SET body='newer conversation'")
	checkpointNoError(t, database.Close())
	checkpointMust(RestoreMissingParentCheckpoint(&restore, noNativeCheckpointState{}, nil))(t)
	if got := readBody("SELECT body FROM conversation"); got != "newer conversation" {
		t.Fatalf("body = %q", got)
	}
}
