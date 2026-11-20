// Port of mj-checkpoint/src/checkpoint/tests.rs: the export fixture and the
// export, staging and repository restore tests.

package checkpoint

import (
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// checkpointFixture is a Codex session with one native rollout and one
// repository "app" with a base commit and an origin, exported to
// worker/source.hel.zip; it returns the spec and that output path.
func checkpointFixture(t testing.TB, temp string) (CheckpointExportSpec, string) {
	t.Helper()
	workerRoot := filepath.Join(temp, "worker")
	checkpointMkdirAll(t, workerRoot)
	harnessHome := filepath.Join(temp, "codex")
	native := filepath.Join(harnessHome, "sessions/2026/08/09")
	checkpointMkdirAll(t, native)
	checkpointWriteFile(t, filepath.Join(native, "rollout-"+checkpointTestNative+".jsonl"), []byte("native"))
	workspace := filepath.Join(temp, "workspace")
	repository := filepath.Join(workspace, "app")
	checkpointMkdirAll(t, repository)
	gitLine(repository, "init")
	gitLine(repository, "config", "user.email", "hel@example.test")
	gitLine(repository, "config", "user.name", "Hel Test")
	checkpointWriteFile(t, filepath.Join(repository, "README.md"), []byte("hello"))
	gitLine(repository, "add", ".")
	gitLine(repository, "commit", "-m", "base")
	gitLine(repository, "remote", "add", "origin", "https://github.com/example/app.git")
	base := gitLine(repository, "rev-parse", "HEAD")
	output := filepath.Join(workerRoot, "source.hel.zip")
	lastActivity := int64(1)
	title := "test"
	return CheckpointExportSpec{
		ProtocolVersion: CheckpointExportProtocolVersion,
		Session: core.SessionManifest{
			ID:              checkpointTestSession,
			Title:           "test",
			HarnessKind:     core.HarnessKindCodex,
			ProfileID:       "codex-1",
			NativeSessionID: checkpointTestNative,
			CreatedAt:       "2026-08-09T00:00:00Z",
			CheckpointedAt:  "2026-08-09T00:01:00Z",
			HelVersion:      "0.1.0",
			RelayVersion:    "0.1.0",
			AdapterVersion:  "test",
		},
		Target: core.TargetManifest{
			TemplateID: "local",
			TargetKind: "podman",
			Details:    map[string]string{},
		},
		Bundle: core.BundleManifest{
			ID:                "bundle",
			PrimaryRepository: "app",
		},
		RelayRoot:     workerRoot,
		HarnessHome:   harnessHome,
		WorkspaceRoot: workspace,
		Repositories: []CheckpointRepositorySpec{{
			ID:                  "app",
			RelativeDestination: "app",
			Capture:             CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureDeltaFrom, BaseCommit: base},
		}},
		CanonicalSession: core.CanonicalSessionSnapshot{
			EventFrontier:       1,
			EventFrontierDigest: strings.Repeat("a", 64),
			Session: core.CanonicalSessionState{
				Execution:        core.CanonicalExecutionState{State: core.CanonicalExecutionStateIdle},
				LastActivityAtMs: &lastActivity,
				SessionTitle:     &title,
			},
			Transcript: []core.CanonicalTranscriptItem{{
				StableID:        "user-1",
				Position:        1,
				CreatedAtMs:     1,
				LastChangedAtMs: 1,
				Body: core.CanonicalTranscriptBodyUser{
					Content: []json.RawMessage{json.RawMessage(`{"type":"text","text":"hello"}`)},
				},
			}},
			QueuedPrompts: []core.CanonicalQueuedPrompt{{
				CommandID:  "queued-1",
				Kind:       core.CanonicalQueuedCommandKind{Kind: core.CanonicalQueuedCommandKindPrompt},
				Content:    []json.RawMessage{json.RawMessage(`{"type":"text","text":"next"}`)},
				QueuedAtMs: 2,
			}},
		},
		OutputPath: output,
	}, output
}

// cloneRepositorySpecs copies a spec's repository list so a test can change
// one entry without changing another spec that shares it.
func cloneRepositorySpecs(specs []CheckpointRepositorySpec) []CheckpointRepositorySpec {
	return slices.Clone(specs)
}

// noGit is a runner for a plain project folder, where any Git use is a bug,
// so it refuses to answer.
type noGit struct{ t testing.TB }

func (g noGit) Run(_ string, command *GitCommand) (GitOutput, error) {
	g.t.Fatalf("plain project directory ran git %q", command.Arguments)
	return GitOutput{}, nil
}

func TestIgnoreNestedRepositoriesExportsOuterWorkWithoutPersistingTheOmission(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	nested := ".scratch/ü landing/master-check"
	gitLine(repository, "worktree", "add", "--detach", nested, "HEAD")
	checkpointWriteFile(t, filepath.Join(repository, nested, "valuable.txt"), []byte("nested work\n"))
	gitLine(repository, "clone", "--no-hardlinks", ".", ".scratch/independent")
	checkpointWriteFile(t, filepath.Join(repository, ".scratch/independent/valuable.txt"), []byte("clone work\n"))
	checkpointWriteFile(t, filepath.Join(repository, "README.md"), []byte("staged\n"))
	gitLine(repository, "add", "README.md")
	checkpointWriteFile(t, filepath.Join(repository, "README.md"), []byte("unstaged\n"))
	checkpointWriteFile(t, filepath.Join(repository, "keep.txt"), []byte("untracked\n"))
	exclude := checkpointReadFile(t, filepath.Join(repository, ".git/info/exclude"))

	if _, err := ExportCheckpoint(&spec); err == nil || !strings.Contains(err.Error(), core.NestedRepositoryCheckpointError) {
		t.Fatalf("error = %v", err)
	}
	if checkpointExists(spec.OutputPath) {
		t.Fatal("a refused export left an archive")
	}
	spec.IgnoreNestedRepositories = true
	// Exercise the controller/target JSON contract as well as archive
	// restore.
	var request CheckpointExportSpec
	checkpointNoError(t, json.Unmarshal(checkpointMust(json.Marshal(spec))(t), &request))
	checkpointMust(ExportCheckpoint(&request))(t)
	checkpointMust(VerifyArchiveStreaming(spec.OutputPath))(t)
	restored := filepath.Join(temp, "restored")
	checkpointNoError(t, os.Mkdir(restored, 0o755))
	gitLine(restored, "clone", repository, "app")
	checkpointNoError(t, RestoreRepositories(spec.OutputPath, restored, SystemGit{}))
	checkout := filepath.Join(restored, "app")
	if body := checkpointReadFile(t, filepath.Join(checkout, "README.md")); string(body) != "unstaged\n" {
		t.Fatalf("README.md = %q", body)
	}
	if staged := gitLine(checkout, "show", ":README.md"); staged != "staged" {
		t.Fatalf("staged README.md = %q", staged)
	}
	if body := checkpointReadFile(t, filepath.Join(checkout, "keep.txt")); string(body) != "untracked\n" {
		t.Fatalf("keep.txt = %q", body)
	}
	// The registered worktree is the repository's own work and comes back;
	// only the independent clone, which the user chose to omit, does not.
	if body := checkpointReadFile(t, filepath.Join(checkout, nested, "valuable.txt")); string(body) != "nested work\n" {
		t.Fatalf("worktree file = %q", body)
	}
	if checkpointExists(filepath.Join(checkout, ".scratch/independent")) {
		t.Fatal("the omitted clone was restored")
	}
	if after := checkpointReadFile(t, filepath.Join(repository, ".git/info/exclude")); string(after) != string(exclude) {
		t.Fatal("the export persisted the omission in .git/info/exclude")
	}
	if body := checkpointReadFile(t, filepath.Join(repository, nested, "valuable.txt")); string(body) != "nested work\n" {
		t.Fatalf("source worktree file = %q", body)
	}
	spec.IgnoreNestedRepositories = false
	if _, err := ExportCheckpoint(&spec); err == nil || !strings.Contains(err.Error(), core.NestedRepositoryCheckpointError) {
		t.Fatalf("error = %v", err)
	}
}

// A plain project folder is the user's own directory, not a checkout: the
// archive says so, captures nothing about it, and a restore never touches
// it.
func TestAPlainProjectDirectoryIsRecordedButNeverCapturedOrRestored(t *testing.T) {
	temp := t.TempDir()
	spec, archivePath := checkpointFixture(t, temp)
	plain := filepath.Join(spec.WorkspaceRoot, "notes")
	checkpointMkdirAll(t, filepath.Join(plain, "drafts"))
	checkpointWriteFile(t, filepath.Join(plain, "todo.md"), []byte("# todo\n"))
	checkpointWriteFile(t, filepath.Join(plain, "drafts/one.md"), []byte("draft one\n"))
	spec.Repositories = []CheckpointRepositorySpec{{
		ID:                  "notes",
		RelativeDestination: "notes",
		Capture:             CheckpointRepositoryCapture{Mode: CheckpointRepositoryCapturePlainDirectory},
	}}
	spec.Bundle.PrimaryRepository = "notes"
	checkpointMust(ExportCheckpointWithGit(&spec, noGit{t}))(t)

	archive := checkpointMust(ReadArchiveVerified(archivePath))(t)
	if len(archive.Manifest.Repositories) != 1 {
		t.Fatalf("one repository was exported, not %d", len(archive.Manifest.Repositories))
	}
	repository := archive.Manifest.Repositories[0]
	if !repository.Metadata.PlainDirectory {
		t.Fatal("the repository is not recorded as a plain directory")
	}
	if !checkpointPathEqual(repository.Metadata.RelativeDestination, "notes") {
		t.Fatalf("destination = %s", repository.Metadata.RelativeDestination)
	}
	if repository.Metadata.HeadCommit != "" || repository.Metadata.Origin != "" || len(repository.Metadata.SavedRefs) != 0 {
		t.Fatalf("metadata = %+v", repository.Metadata)
	}
	for _, payload := range archive.Manifest.Payloads {
		switch payload.Role.Kind {
		case core.PayloadRoleGitBundle, core.PayloadRoleGitStagedPatch, core.PayloadRoleGitUnstagedPatch, core.PayloadRoleGitUntrackedTar:
			if payload.Size != 0 {
				t.Fatalf("a plain folder contributes no Git payload content: %+v", payload)
			}
		}
	}

	// The user keeps working in the folder, and a restore must leave it
	// alone: no checkout, no overwrite, no removal of anything added since.
	checkpointWriteFile(t, filepath.Join(plain, "todo.md"), []byte("# todo\n\nnew work\n"))
	checkpointWriteFile(t, filepath.Join(plain, "later.md"), []byte("written later\n"))
	restoreRoot := filepath.Join(temp, "restore-workspace")
	checkpointMkdirAll(t, filepath.Join(restoreRoot, "notes"))
	checkpointWriteFile(t, filepath.Join(restoreRoot, "notes/todo.md"), checkpointReadFile(t, filepath.Join(plain, "todo.md")))
	checkpointNoError(t, RestoreRepositories(archivePath, restoreRoot, noGit{t}))
	if body := checkpointReadFile(t, filepath.Join(restoreRoot, "notes/todo.md")); string(body) != "# todo\n\nnew work\n" {
		t.Fatalf("todo = %q", body)
	}
	if checkpointExists(filepath.Join(restoreRoot, "notes/drafts")) {
		t.Fatal("the restore wrote into the plain folder")
	}
}

// appendAtEOF is a reader that appends to the file it reads when it reaches
// the end, as a harness can append just after a capture's stream reached
// EOF.
type appendAtEOF struct {
	source   *os.File
	path     string
	appended bool
}

func (r *appendAtEOF) Read(data []byte) (int, error) {
	count, err := r.source.Read(data)
	if count == 0 && !r.appended {
		r.appended = true
		file, openErr := os.OpenFile(r.path, os.O_APPEND|os.O_WRONLY, 0)
		if openErr != nil {
			return count, openErr
		}
		_, writeErr := file.Write([]byte("later harness bytes"))
		file.Close()
		if writeErr != nil {
			return count, writeErr
		}
	}
	return count, err
}

// appendDuringGit appends to a native file at the first Git command, as an
// asynchronous native write during repository collection does.
type appendDuringGit struct {
	path     string
	appended atomic.Bool
}

func (g *appendDuringGit) Run(repository string, command *GitCommand) (GitOutput, error) {
	if !g.appended.Swap(true) {
		file, err := os.OpenFile(g.path, os.O_APPEND|os.O_WRONLY, 0)
		if err != nil {
			return GitOutput{}, err
		}
		_, err = file.Write([]byte("async native write during Git"))
		file.Close()
		if err != nil {
			return GitOutput{}, err
		}
	}
	return SystemGit{}.Run(repository, command)
}

// captureSpecFromExport is the capture spec for the same session, target and
// repositories as an export spec.
func captureSpecFromExport(spec *CheckpointExportSpec, stagePath string) CheckpointCaptureSpec {
	return CheckpointCaptureSpec{
		ProtocolVersion:  CheckpointStagingProtocolVersion,
		Session:          spec.Session,
		Target:           spec.Target,
		Bundle:           spec.Bundle,
		RelayRoot:        spec.RelayRoot,
		HarnessHome:      spec.HarnessHome,
		WorkspaceRoot:    spec.WorkspaceRoot,
		Repositories:     cloneRepositorySpecs(spec.Repositories),
		AllowEmptyNative: false,
		StagePath:        stagePath,
		RefreshExisting:  false,
	}
}

// Hard-won: 7d68f8bde: the staged copy must own its size when the live source changes at EOF.
func TestStagedCheckpointPreservesExportContentsAndDefersCanonicalHistory(t *testing.T) {
	temp := t.TempDir()
	exportSpec, legacyPath := checkpointFixture(t, temp)
	largeNative := filepath.Join(exportSpec.HarnessHome, "sessions/2026/08/09/rollout-"+checkpointTestNative+".jsonl")
	checkpointWriteFile(t, largeNative, []byte(strings.Repeat("n", 128*1024)))
	// The same stream used by capture can reach EOF just before the harness
	// appends. Its result must describe the sealed destination, not the
	// source.
	source := checkpointMust(os.Open(largeNative))(t)
	copied := checkpointMust(writePrivateReader(temp, "owned-native-copy", &appendAtEOF{source: source, path: largeNative}, 0o600))(t)
	source.Close()
	if info := checkpointMust(os.Stat(filepath.Join(temp, "owned-native-copy")))(t); uint64(info.Size()) != copied {
		t.Fatalf("copied %d bytes, wrote %d", copied, info.Size())
	}
	if info := checkpointMust(os.Stat(largeNative))(t); uint64(info.Size()) <= copied {
		t.Fatal("the source did not grow")
	}
	captured := checkpointReadFile(t, largeNative)
	checkpointMust(ExportCheckpointWithGit(&exportSpec, &appendDuringGit{path: largeNative}))(t)
	legacy := checkpointMust(ReadArchiveVerified(exportSpec.OutputPath))(t)
	nativeIndex := slices.IndexFunc(legacy.Manifest.Payloads, func(payload core.PayloadDescriptor) bool {
		return payload.Role.Kind == core.PayloadRoleNativeArtifact
	})
	if nativeIndex < 0 {
		t.Fatal("the archive has no native artifact")
	}
	if payload := checkpointMust(legacy.Payload(&legacy.Manifest.Payloads[nativeIndex]))(t); string(payload) != string(captured) {
		t.Fatal("the archive holds native bytes written after the capture")
	}
	if info := checkpointMust(os.Stat(largeNative))(t); info.Size() <= int64(len(captured)) {
		t.Fatal("the source did not grow during Git")
	}
	checkpointWriteFile(t, largeNative, captured)

	reports := filepath.Join(exportSpec.WorkspaceRoot, core.ReportRootDir, "large-child")
	checkpointMkdirAll(t, reports)
	artifact := checkpointMust(os.Create(filepath.Join(reports, "build-artifact")))(t)
	checkpointNoError(t, artifact.Truncate(int64(core.MaxCheckpointReportBytes+1)))
	checkpointNoError(t, artifact.Close())

	stagePath := filepath.Join(exportSpec.RelayRoot, "checkpoint-stage-test")
	captureSpec := captureSpecFromExport(&exportSpec, stagePath)
	captureJSON := checkpointMust(json.Marshal(captureSpec))(t)
	if strings.Contains(string(captureJSON), "queued-1") {
		t.Fatal("canonical history crossed the barrier in the capture request")
	}
	capturedStage := checkpointMust(CaptureCheckpoint(&captureSpec, SystemGit{}))(t)
	if capturedStage.ReusedNative {
		t.Fatal("a fresh capture reused native history")
	}
	if capturedStage.OmittedReports.Files != 1 || capturedStage.OmittedReports.Bytes != core.MaxCheckpointReportBytes+1 {
		t.Fatalf("omitted reports = %+v", capturedStage.OmittedReports)
	}
	if capturedStage.NativeBytes < 128*1024 {
		t.Fatalf("native bytes = %d", capturedStage.NativeBytes)
	}
	if info, err := os.Stat(filepath.Join(stagePath, captureCheckpointStageManifest)); err != nil || !info.Mode().IsRegular() {
		t.Fatalf("stage manifest: %v", err)
	}
	nativeStage := filepath.Join(stagePath, "native/00000000")
	nativeStageModified := checkpointMust(os.Stat(nativeStage))(t).ModTime()
	refreshSpec := captureSpec
	refreshSpec.RefreshExisting = true
	refreshed := checkpointMust(CaptureCheckpoint(&refreshSpec, SystemGit{}))(t)
	if !refreshed.ReusedNative {
		t.Fatal("an unchanged prestage was recaptured")
	}
	if !reflect.DeepEqual(refreshed.OmittedReports, capturedStage.OmittedReports) {
		t.Fatalf("omitted reports = %+v, want %+v", refreshed.OmittedReports, capturedStage.OmittedReports)
	}
	if refreshed.NativeBytes != capturedStage.NativeBytes {
		t.Fatalf("native bytes = %d, want %d", refreshed.NativeBytes, capturedStage.NativeBytes)
	}
	if modified := checkpointMust(os.Stat(nativeStage))(t).ModTime(); !modified.Equal(nativeStageModified) {
		t.Fatal("barrier catch-up rewrote unchanged native history")
	}

	stagedPath := filepath.Join(exportSpec.RelayRoot, "staged.hel.zip")
	packSpec := CheckpointPackSpec{
		ProtocolVersion:  CheckpointStagingProtocolVersion,
		RelayRoot:        exportSpec.RelayRoot,
		StagePath:        stagePath,
		CanonicalSession: exportSpec.CanonicalSession,
		OutputPath:       stagedPath,
	}
	packed := checkpointMust(PackCheckpoint(&packSpec))(t)
	if packed.EventFrontier != exportSpec.CanonicalSession.EventFrontier {
		t.Fatalf("event frontier = %d", packed.EventFrontier)
	}
	if checkpointExists(stagePath) {
		t.Fatal("consumed stage was not removed")
	}

	legacy = checkpointMust(ReadArchiveVerified(legacyPath))(t)
	staged := checkpointMust(ReadArchiveVerified(stagedPath))(t)
	if !reflect.DeepEqual(staged.Manifest, legacy.Manifest) {
		t.Fatalf("staged manifest = %+v, want %+v", staged.Manifest, legacy.Manifest)
	}
	if !reflect.DeepEqual(staged.Payloads, legacy.Payloads) {
		t.Fatal("staged payloads differ from the export's")
	}
}

func TestPrestageCatchUpRecapturesNativeHistoryThatChangedBeforeTheBarrier(t *testing.T) {
	temp := t.TempDir()
	exportSpec, _ := checkpointFixture(t, temp)
	captureSpec := captureSpecFromExport(&exportSpec, filepath.Join(exportSpec.RelayRoot, "checkpoint-stage-test"))
	first := checkpointMust(CaptureCheckpoint(&captureSpec, SystemGit{}))(t)
	if first.ReusedNative {
		t.Fatal("a fresh capture reused native history")
	}
	rollout := filepath.Join(captureSpec.HarnessHome, "sessions/2026/08/09/rollout-"+checkpointTestNative+".jsonl")
	checkpointWriteFile(t, rollout, []byte("native changed before barrier"))

	captureSpec.RefreshExisting = true
	refreshed := checkpointMust(CaptureCheckpoint(&captureSpec, SystemGit{}))(t)
	if refreshed.ReusedNative {
		t.Fatal("a changed prestage was reused")
	}
	if refreshed.NativeBytes <= first.NativeBytes {
		t.Fatalf("native bytes = %d, first = %d", refreshed.NativeBytes, first.NativeBytes)
	}
}

func TestCheckpointPackagingThroughput(t *testing.T) {
	source := os.Getenv("MJ_CHECKPOINT_BENCH_ARCHIVE")
	if source == "" {
		t.Skip("timing measurement against MJ_CHECKPOINT_BENCH_ARCHIVE")
	}
	readStarted := time.Now()
	archive := checkpointMust(ReadArchiveVerified(source))(t)
	canonicalSession := checkpointMust(archive.CanonicalSession())(t)
	var nativeArtifacts []core.NativeArtifact
	for i := range archive.Manifest.Payloads {
		descriptor := &archive.Manifest.Payloads[i]
		if descriptor.Role.Kind != core.PayloadRoleNativeArtifact {
			continue
		}
		nativeArtifacts = append(nativeArtifacts, core.NativeArtifact{
			RelativePath: descriptor.Role.RelativePath,
			Data:         core.NativeArtifactDataBytes(slices.Clone(checkpointMust(archive.Payload(descriptor))(t))),
			Mode:         descriptor.Mode,
		})
	}
	repositories := make([]RepositorySnapshot, 0, len(archive.Manifest.Repositories))
	for i := range archive.Manifest.Repositories {
		repositories = append(repositories, checkpointMust(archivedRepositorySnapshot(&archive, &archive.Manifest.Repositories[i]))(t))
	}
	var payloadBytes uint64
	for i := range nativeArtifacts {
		payloadBytes += checkpointMust(nativeArtifacts[i].Size())(t)
	}
	for _, repository := range repositories {
		payloadBytes += uint64(len(repository.CommittedBundle) + len(repository.StagedPatch) +
			len(repository.UnstagedPatch) + len(repository.UntrackedTar))
	}
	payloadBytes += uint64(len(checkpointMust(json.Marshal(canonicalSession))(t)))
	readElapsed := time.Since(readStarted)
	stageFixture := t.TempDir()
	relayRoot := filepath.Join(stageFixture, "worker")
	harnessHome := filepath.Join(stageFixture, "harness")
	workspaceRoot := filepath.Join(stageFixture, "workspace")
	repositoryRoot := filepath.Join(workspaceRoot, "app")
	checkpointMkdirAll(t, relayRoot)
	checkpointMkdirAll(t, harnessHome)
	checkpointMkdirAll(t, repositoryRoot)
	for i := range nativeArtifacts {
		artifact := &nativeArtifacts[i]
		checkpointNoError(t, writePrivateFile(harnessHome, artifact.RelativePath,
			checkpointMust(core.NativeArtifactDataBytesOf(artifact.Data))(t), artifact.Mode))
	}
	gitLine(repositoryRoot, "init")
	gitLine(repositoryRoot, "config", "user.email", "hel@example.test")
	gitLine(repositoryRoot, "config", "user.name", "Hel Test")
	checkpointWriteFile(t, filepath.Join(repositoryRoot, "README.md"), []byte("benchmark"))
	gitLine(repositoryRoot, "add", ".")
	gitLine(repositoryRoot, "commit", "-m", "benchmark")
	captureSpec := CheckpointCaptureSpec{
		ProtocolVersion: CheckpointStagingProtocolVersion,
		Session:         archive.Manifest.Session,
		Target:          archive.Manifest.Target,
		Bundle:          archive.Manifest.Bundle,
		RelayRoot:       relayRoot,
		HarnessHome:     harnessHome,
		WorkspaceRoot:   workspaceRoot,
		Repositories: []CheckpointRepositorySpec{{
			ID:                  "app",
			RelativeDestination: "app",
			Capture:             CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureMetadataOnly},
		}},
		AllowEmptyNative: false,
		StagePath:        filepath.Join(stageFixture, "worker/checkpoint-stage"),
		RefreshExisting:  false,
	}
	prestageStarted := time.Now()
	checkpointMust(CaptureCheckpoint(&captureSpec, SystemGit{}))(t)
	prestageElapsed := time.Since(prestageStarted)
	catchUpStarted := time.Now()
	catchUp := captureSpec
	catchUp.RefreshExisting = true
	checkpointMust(CaptureCheckpoint(&catchUp, SystemGit{}))(t)
	catchUpElapsed := time.Since(catchUpStarted)
	output := filepath.Join(t.TempDir(), "benchmark.hel.zip")
	packStarted := time.Now()
	checkpointMust(WriteArchiveHashed(output, &ArchiveInput{
		Session:          archive.Manifest.Session,
		Target:           archive.Manifest.Target,
		Bundle:           archive.Manifest.Bundle,
		CanonicalSession: canonicalSession,
		NativeArtifacts:  nativeArtifacts,
		Repositories:     repositories,
	}))(t)
	fmt.Fprintf(os.Stderr, "checkpoint benchmark: payload_bytes=%d read_ms=%d prestage_ms=%d catch_up_ms=%d pack_ms=%d\n",
		payloadBytes, readElapsed.Milliseconds(), prestageElapsed.Milliseconds(), catchUpElapsed.Milliseconds(), time.Since(packStarted).Milliseconds())
}

func TestCodexRootArchiveThroughput(t *testing.T) {
	const baselineMs = 46_124
	source := os.Getenv("MJ_CHECKPOINT_BENCH_ARCHIVE")
	harnessHome := os.Getenv("MJ_CHECKPOINT_BENCH_HARNESS_HOME")
	if source == "" || harnessHome == "" {
		t.Skip("timing measurement against a real Codex archive and harness home")
	}
	metadata := checkpointMust(VerifyArchiveStreaming(source))(t)
	if metadata.Manifest.Session.HarnessKind != core.HarnessKindCodex {
		t.Fatalf("harness = %s", metadata.Manifest.Session.HarnessKind)
	}
	for _, payload := range metadata.Manifest.Payloads {
		switch payload.Role.Kind {
		case core.PayloadRoleGitBundle, core.PayloadRoleGitStagedPatch, core.PayloadRoleGitUnstagedPatch, core.PayloadRoleGitUntrackedTar:
			if payload.Size != 0 {
				t.Fatal("benchmark source must use metadata-only repository capture")
			}
		}
	}

	// Prime the persistent negative verdicts exactly as a prior checkpoint
	// does. The timed run models the repeat-checkpoint path.
	nativeSessionID := metadata.Manifest.Session.NativeSessionID
	scanCache := codexScanCacheEmpty(nativeSessionID)
	warm := checkpointMust(collectNativeArtifactsCached(core.HarnessKindCodex, harnessHome, nativeSessionID, false, &scanCache))(t)
	if len(warm) != 1 {
		t.Fatalf("benchmark must capture only the root rollout, captured %d", len(warm))
	}

	outputDirectory := checkpointMust(os.MkdirTemp(filepath.Dir(source), ".tmp*"))(t)
	defer os.RemoveAll(outputDirectory)
	output := filepath.Join(outputDirectory, "export.hel.zip")
	installed := filepath.Join(outputDirectory, "installed.hel.zip")
	started := time.Now()
	collectStarted := time.Now()
	nativeArtifacts := checkpointMust(collectNativeArtifactsCached(core.HarnessKindCodex, harnessHome, nativeSessionID, false, &scanCache))(t)
	var nativeBytes uint64
	for i := range nativeArtifacts {
		nativeBytes += checkpointMust(nativeArtifacts[i].Size())(t)
	}
	collectMs := time.Since(collectStarted).Milliseconds()
	repositories := make([]RepositorySnapshot, 0, len(metadata.Manifest.Repositories))
	for _, repository := range metadata.Manifest.Repositories {
		repositories = append(repositories, RepositorySnapshot{
			Metadata:        repository.Metadata,
			CommittedBundle: []byte{},
			StagedPatch:     []byte{},
			UnstagedPatch:   []byte{},
			UntrackedTar:    []byte{},
			LinkedWorktrees: []LinkedWorktreeSnapshot{},
		})
	}
	archiveStarted := time.Now()
	sha := checkpointMust(WriteArchiveHashedBorrowed(output, &metadata.Manifest.Session, &metadata.Manifest.Target,
		&metadata.Manifest.Bundle, &metadata.CanonicalSession, nativeArtifacts, repositories))(t)
	archiveMs := time.Since(archiveStarted).Milliseconds()
	copyStarted := time.Now()
	checkpointWriteFile(t, installed, checkpointReadFile(t, output))
	copyMs := time.Since(copyStarted).Milliseconds()
	checksumStarted := time.Now()
	installedSha := checkpointMust(CheckpointSha256(installed))(t)
	checksumMs := time.Since(checksumStarted).Milliseconds()
	totalMs := time.Since(started).Milliseconds()
	if installedSha != sha {
		t.Fatalf("installed digest %s, exported %s", installedSha, sha)
	}
	fmt.Fprintf(os.Stderr, "Codex root archive benchmark: native_bytes=%d archive_bytes=%d collect_ms=%d archive_ms=%d copy_ms=%d checksum_ms=%d total_ms=%d target_ms=%d\n",
		nativeBytes, checkpointMust(os.Stat(installed))(t).Size(), collectMs, archiveMs, copyMs, checksumMs, totalMs, baselineMs/10)
	if totalMs > baselineMs/10 {
		t.Fatalf("root-only repeat export took %dms; 10x target is %dms", totalMs, baselineMs/10)
	}
}

func TestCheckpointCollectsTheConfiguredMemoryReplicaForNonClaudeHarnesses(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	memoryRoot := filepath.Join(spec.HarnessHome, "projects/replica/memory")
	checkpointMkdirAll(t, memoryRoot)
	checkpointWriteFile(t, filepath.Join(memoryRoot, "MEMORY.md"), []byte("remember this"))
	native := checkpointTestNative
	launch := core.WorkerLaunchConfig{
		CPUAccounting:         core.CpuAccountingModeProcessTree,
		SessionID:             checkpointTestSession,
		Harness:               core.HarnessKindCodex,
		HarnessHome:           spec.HarnessHome,
		BridgeCommand:         "codex-acp",
		BridgeArgs:            []string{},
		HarnessRuntime:        core.HarnessRuntimePolicyAmbient,
		Environment:           map[string]string{},
		Cwd:                   filepath.Join(spec.WorkspaceRoot, "app"),
		AdditionalDirectories: []string{},
		NativeSessionID:       &native,
		Subagents:             core.SubagentPolicy{Mode: core.SubagentPolicyNative},
		AgentMailboxesEnabled: true,
		SubtleSubagents:       true,
		ReviewTrigger:         core.ReviewTriggerOff,
		ProjectMemory: &core.ProjectMemoryLaunchConfig{
			ProjectKey:      "project",
			Root:            memoryRoot,
			BaselineRoot:    filepath.Join(spec.HarnessHome, "projects/replica/.hel-memory-baseline"),
			RepositoryRoots: map[string]string{},
			MCPDelivery:     core.ProjectMemoryMcpDeliveryAcp,
		},
		ExecutionPolicy:      core.ExecutionPolicyUnconstrained,
		ClaudePermissionMode: core.ClaudePermissionModeAuto,
	}
	checkpointNoError(t, launch.Write(filepath.Join(spec.RelayRoot, "launch.json")))

	artifacts := checkpointMust(collectCheckpointNativeArtifacts(&spec.Session, spec.RelayRoot, spec.HarnessHome,
		spec.WorkspaceRoot, nil, false, noNativeCheckpointState{}))(t)
	if !slices.ContainsFunc(artifacts, func(artifact core.NativeArtifact) bool {
		return checkpointPathEqual(artifact.RelativePath, "projects/replica/memory/MEMORY.md") &&
			string(artifactBytes(t, &artifact)) == "remember this"
	}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

func TestCheckpointCollectsMemoryFromALegacyWorkerLaunchConfig(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	memoryRoot := filepath.Join(spec.HarnessHome, "projects/replica/memory")
	checkpointMkdirAll(t, memoryRoot)
	checkpointWriteFile(t, filepath.Join(memoryRoot, "MEMORY.md"), []byte("legacy memory"))
	legacyLaunch := map[string]any{
		"session_id":        checkpointTestSession,
		"harness":           "codex",
		"bridge_command":    "codex-acp",
		"bridge_args":       []any{},
		"environment":       map[string]any{},
		"cwd":               filepath.Join(spec.WorkspaceRoot, "app"),
		"native_session_id": checkpointTestNative,
		"project_memory": map[string]any{
			"project_key":      "project",
			"root":             memoryRoot,
			"baseline_root":    filepath.Join(spec.HarnessHome, "projects/replica/.hel-memory-baseline"),
			"repository_roots": map[string]any{},
		},
		"force_unrestricted_mode": true,
	}
	checkpointWriteFile(t, filepath.Join(spec.RelayRoot, "launch.json"), checkpointMust(json.MarshalIndent(legacyLaunch, "", "  "))(t))

	artifacts := checkpointMust(collectCheckpointNativeArtifacts(&spec.Session, spec.RelayRoot, spec.HarnessHome,
		spec.WorkspaceRoot, nil, false, noNativeCheckpointState{}))(t)

	if !slices.ContainsFunc(artifacts, func(artifact core.NativeArtifact) bool {
		return checkpointPathEqual(artifact.RelativePath, "projects/replica/memory/MEMORY.md") &&
			string(artifactBytes(t, &artifact)) == "legacy memory"
	}) {
		t.Fatalf("artifacts = %v", artifactPaths(artifacts))
	}
}

func TestProjectMemoryReplicaAcceptsAnSSHHomeRelativePath(t *testing.T) {
	relative := ".local/share/hel/profiles/session/projects/replica/memory"
	home, ok := os.LookupEnv("HOME")
	if !ok {
		t.Fatal("test HOME is missing")
	}

	if resolved := checkpointMust(resolveHomeRelativeTargetPath(relative))(t); !checkpointPathEqual(resolved, core.Join(home, relative)) {
		t.Fatalf("resolved = %s", resolved)
	}
	if _, err := resolveHomeRelativeTargetPath("../memory"); err == nil {
		t.Fatal("a parent traversal resolved")
	}
}

func TestCheckpointsCollectMemoryThroughALegacyWorkerProfileLink(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix symlinks")
	}
	for _, statesHome := range []bool{true, false} {
		temp := t.TempDir()
		spec, _ := checkpointFixture(t, temp)
		installedHome := spec.HarnessHome
		memory := filepath.Join(installedHome, "projects/legacy/memory")
		checkpointMkdirAll(t, memory)
		body := strings.Repeat("legacy memory survives upgrade\n", 4096)
		if len(body) <= 64*1024 {
			t.Fatal("the memory must be larger than a pipe buffer")
		}
		checkpointWriteFile(t, filepath.Join(memory, "MEMORY.md"), []byte(body))
		launch := map[string]any{
			"harness":                 "codex",
			"environment":             map[string]any{"CODEX_HOME": installedHome},
			"project_memory":          map[string]any{"root": memory},
			"force_unrestricted_mode": true,
		}
		if statesHome {
			launch["harness_home"] = installedHome
		}
		checkpointWriteFile(t, filepath.Join(spec.RelayRoot, "launch.json"), checkpointMust(json.Marshal(launch))(t))
		spec.HarnessHome = filepath.Join(spec.RelayRoot, "profile")
		checkpointNoError(t, os.Symlink(installedHome, spec.HarnessHome))

		checkpointMust(ExportCheckpoint(&spec))(t)
		legacy := checkpointMust(ReadArchiveVerified(spec.OutputPath))(t)
		if !slices.ContainsFunc(legacy.Manifest.Payloads, func(descriptor core.PayloadDescriptor) bool {
			return descriptor.Role.Kind == core.PayloadRoleNativeArtifact &&
				checkpointPathEqual(descriptor.Role.RelativePath, "projects/legacy/memory/MEMORY.md") &&
				string(checkpointMust(legacy.Payload(&descriptor))(t)) == body
		}) {
			t.Fatalf("states home %v: the memory is missing from the archive", statesHome)
		}

		stagePath := filepath.Join(spec.RelayRoot, "checkpoint-stage")
		captureSpec := captureSpecFromExport(&spec, stagePath)
		checkpointMust(CaptureCheckpoint(&captureSpec, SystemGit{}))(t)
		packedPath := filepath.Join(spec.RelayRoot, "packed.zip")
		checkpointMust(PackCheckpoint(&CheckpointPackSpec{
			ProtocolVersion:  CheckpointStagingProtocolVersion,
			RelayRoot:        spec.RelayRoot,
			StagePath:        stagePath,
			CanonicalSession: spec.CanonicalSession,
			OutputPath:       packedPath,
		}))(t)
		packed := checkpointMust(ReadArchiveVerified(packedPath))(t)
		if !reflect.DeepEqual(packed.Manifest, legacy.Manifest) || !reflect.DeepEqual(packed.Payloads, legacy.Payloads) {
			t.Fatalf("states home %v: the packed archive differs from the export", statesHome)
		}

		// Accepting the home alias must not relax the memory containment
		// rule.
		outside := filepath.Join(temp, "outside-memory")
		checkpointMkdirAll(t, outside)
		checkpointWriteFile(t, filepath.Join(outside, "MEMORY.md"), []byte("must not be archived"))
		launch["project_memory"] = map[string]any{"root": outside}
		checkpointWriteFile(t, filepath.Join(spec.RelayRoot, "launch.json"), checkpointMust(json.Marshal(launch))(t))
		if _, err := ExportCheckpoint(&spec); err == nil || !strings.Contains(err.Error(), "outside the harness home") {
			t.Fatalf("states home %v: error = %v", statesHome, err)
		}
	}
}

func TestAManagedWorkspaceWithoutAnOriginCheckpointsWithTheOriginItIsGiven(t *testing.T) {
	// A checkout started without later history has no `origin` remote; the
	// controller supplies the URL the session was created from.
	temp := t.TempDir()
	spec, archivePath := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	launchBase := gitLine(repository, "rev-parse", "HEAD")
	gitLine(repository, "remote", "remove", "origin")
	gitLine(repository, "config", "--local", "mj.remoteWorkspace", "true")
	gitLine(repository, "config", "--local", "mj.baseCommit", launchBase)
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureRemoteWorkspace}
	checkpointWriteFile(t, filepath.Join(repository, "review.md"), []byte("findings"))
	gitLine(repository, "add", ".")
	gitLine(repository, "commit", "-m", "review notes")

	if _, err := ExportCheckpoint(&spec); err == nil || !strings.Contains(err.Error(), "managed workspace has no Git origin") {
		t.Fatalf("error = %v", err)
	}
	if checkpointExists(archivePath) {
		t.Fatal("a refused export left an archive")
	}

	origin := "https://github.com/example/app.git"
	spec.Repositories[0].OriginOverride = &origin
	checkpointMust(ExportCheckpoint(&spec))(t)
	archive := checkpointMust(VerifyArchiveStreaming(archivePath))(t)
	metadata := archive.Manifest.Repositories[0].Metadata
	if !metadata.RemoteWorkspace || metadata.Origin != origin || len(metadata.PushURLs) != 0 || metadata.BaseCommit != launchBase {
		t.Fatalf("metadata = %+v", metadata)
	}
	if head := gitLine(repository, "rev-parse", "HEAD"); metadata.HeadCommit != head {
		t.Fatalf("head = %s, want %s", metadata.HeadCommit, head)
	}
	// Capture reads the override; it does not give the checkout a remote.
	if remotes := gitLine(repository, "remote"); remotes != "" {
		t.Fatalf("remotes = %q", remotes)
	}
}

func TestCheckpointRefusesAHomeDifferentFromTheInstalledWorker(t *testing.T) {
	temp := t.TempDir()
	spec, _ := checkpointFixture(t, temp)
	other := filepath.Join(temp, "another-home")
	checkpointMkdirAll(t, other)
	checkpointWriteFile(t, filepath.Join(spec.RelayRoot, "launch.json"), checkpointMust(json.Marshal(map[string]any{"harness_home": other}))(t))
	if _, err := ExportCheckpoint(&spec); err == nil || !strings.Contains(err.Error(), "does not match the installed worker") {
		t.Fatalf("error = %v", err)
	}
	if checkpointExists(spec.OutputPath) {
		t.Fatal("a refused export left an archive")
	}
}

// gitSucceeds runs Git in repository and reports whether it exited 0.
func gitSucceeds(t testing.TB, repository string, arguments ...string) bool {
	t.Helper()
	command := exec.Command("git", arguments...)
	command.Dir = repository
	output := checkpointMust(core.RunWithInput(command, nil))(t)
	return output.Status.Success()
}

func TestACheckoutRestoreLandsOnTheBranchTheCallerNames(t *testing.T) {
	temp := t.TempDir()
	spec, archivePath := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	launchBase := gitLine(repository, "rev-parse", "HEAD")
	archivedOrigin := "https://archive-fetch.example.test/app.git"
	archivedPushURL := "https://archive-push.example.test/app.git"
	gitLine(repository, "config", "--local", "remote.origin.url", archivedOrigin)
	gitLine(repository, "config", "--local", "--replace-all", "remote.origin.pushurl", archivedPushURL)
	gitLine(repository, "config", "--local", "push.default", "current")
	gitLine(repository, "config", "--local", "push.autoSetupRemote", "true")
	gitLine(repository, "config", "--local", "mj.remoteWorkspace", "true")
	gitLine(repository, "config", "--local", "mj.baseCommit", launchBase)
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureRemoteWorkspace}
	checkpointWriteFile(t, filepath.Join(repository, "feature.txt"), []byte("session work"))
	gitLine(repository, "add", ".")
	gitLine(repository, "commit", "-m", "session work")
	checkpointWriteFile(t, filepath.Join(repository, "README.md"), []byte("edited"))
	head := gitLine(repository, "rev-parse", "HEAD")
	archivedBranch := gitLine(repository, "rev-parse", "--abbrev-ref", "HEAD")
	checkpointMust(ExportCheckpoint(&spec))(t)
	archive := checkpointMust(VerifyArchiveStreaming(archivePath))(t)
	metadata := archive.Manifest.Repositories[0].Metadata
	if !metadata.RemoteWorkspace || metadata.Origin != archivedOrigin || !slices.Equal(metadata.PushURLs, []string{archivedPushURL}) {
		t.Fatalf("metadata = %+v", metadata)
	}

	hostOrigin := "https://host-fetch.example.test/app.git"
	hostPushURL := "https://host-push.example.test/app.git"
	gitLine(repository, "config", "--local", "remote.origin.url", hostOrigin)
	gitLine(repository, "config", "--local", "--replace-all", "remote.origin.pushurl", hostPushURL)
	gitLine(repository, "config", "--local", "push.default", "simple")
	gitLine(repository, "config", "--local", "push.autoSetupRemote", "false")
	gitLine(repository, "config", "--local", "--unset-all", "mj.remoteWorkspace")
	gitLine(repository, "config", "--local", "--unset-all", "mj.baseCommit")

	checkout := filepath.Join(temp, "worktrees/session")
	gitLine(repository, "worktree", "add", "-b", "mj/session", checkout, "HEAD~1")

	restored := checkpointMust(RestoreSingleRepositoryOntoBranch(archivePath, checkout, "mj/session", SystemGit{}))(t)

	if restored == nil || *restored != archivedBranch {
		t.Fatalf("restored branch = %v, want %s", restored, archivedBranch)
	}
	if got := gitLine(checkout, "rev-parse", "HEAD"); got != head {
		t.Fatalf("HEAD = %s, want %s", got, head)
	}
	if got := gitLine(checkout, "rev-parse", "--abbrev-ref", "HEAD"); got != "mj/session" {
		t.Fatalf("branch = %s", got)
	}
	if body := checkpointReadFile(t, filepath.Join(checkout, "README.md")); string(body) != "edited" {
		t.Fatalf("the session's uncommitted work comes with it: %q", body)
	}
	if got := gitLine(repository, "config", "--local", "--get", "remote.origin.url"); got != hostOrigin {
		t.Fatalf("host origin is unchanged by the raw restore: %s", got)
	}
	if got := gitLine(repository, "config", "--local", "--get-all", "remote.origin.pushurl"); got != hostPushURL {
		t.Fatalf("host push URL is unchanged by the raw restore: %s", got)
	}
	if got := gitLine(repository, "config", "--local", "--get", "push.default"); got != "simple" {
		t.Fatalf("host push default is unchanged by the raw restore: %s", got)
	}
	if got := gitLine(repository, "config", "--local", "--get", "push.autoSetupRemote"); got != "false" {
		t.Fatalf("host automatic push setup is unchanged by the raw restore: %s", got)
	}
	for _, key := range []string{"mj.remoteWorkspace", "mj.baseCommit"} {
		if gitSucceeds(t, repository, "config", "--local", "--get", key) {
			t.Fatalf("host config unexpectedly has %s", key)
		}
	}

	// Restoring onto the archived branch is exactly what the override
	// avoids: that branch is checked out in the user's own working tree.
	_, err := RestoreSingleRepositoryOntoBranch(archivePath, checkout, archivedBranch, SystemGit{})
	if err == nil || !strings.Contains(err.Error(), "restore committed branch") {
		t.Fatalf("error = %v", err)
	}
	if got := gitLine(checkout, "rev-parse", "--abbrev-ref", "HEAD"); got != "mj/session" {
		t.Fatalf("a rejected restore leaves the checkout on its session branch: %s", got)
	}
	if body := checkpointReadFile(t, filepath.Join(checkout, "README.md")); string(body) != "edited" {
		t.Fatalf("a rejected restore leaves the worktree unchanged: %q", body)
	}
}

// Hard-won: 5dd34548: workspace restore could check out a branch already held by another worktree or return a raw Git error.
// Hard-won: d1e6bd62c: Git 2.25 has no `worktree list -z`, so the occupied-branch check failed.
func TestAWorkspaceRestoreRefusesABranchCheckedOutInAnotherWorktree(t *testing.T) {
	temp := t.TempDir()
	spec, archivePath := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	checkpointWriteFile(t, filepath.Join(repository, "feature.txt"), []byte("session work"))
	gitLine(repository, "add", ".")
	gitLine(repository, "commit", "-m", "session work")
	archivedBranch := gitLine(repository, "rev-parse", "--abbrev-ref", "HEAD")
	checkpointMust(ExportCheckpoint(&spec))(t)

	// The restore destination is a sibling worktree of the same repository,
	// so the archived branch is already owned by the main checkout.
	restoreRoot := filepath.Join(temp, "restore-workspace")
	destination := filepath.Join(restoreRoot, "app")
	gitLine(repository, "worktree", "add", "-b", "mj/session", destination, "HEAD~1")
	before := gitLine(destination, "rev-parse", "HEAD")

	err := RestoreRepositories(archivePath, restoreRoot, git231{})

	if err == nil || !strings.Contains(err.Error(), "is checked out in another worktree") {
		t.Fatalf("error = %v", err)
	}
	if got := gitLine(destination, "rev-parse", "--abbrev-ref", "HEAD"); got != "mj/session" {
		t.Fatalf("a rejected restore leaves the destination on its own branch: %s", got)
	}
	if got := gitLine(destination, "rev-parse", "HEAD"); got != before {
		t.Fatalf("a rejected restore leaves the destination checkout unchanged: %s", got)
	}
	if got := gitLine(repository, "rev-parse", "--abbrev-ref", "HEAD"); got != archivedBranch {
		t.Fatalf("branch = %s, want %s", got, archivedBranch)
	}
}

// A session whose project is a subdirectory of its checkout still owns the
// whole checkout: Stop deletes a managed worktree, so work anywhere in it
// has to travel in the archive and come back at the same place in the
// checkout.
// Hard-won: c4322938: stop and resume lost tracked and untracked work outside the project subdirectory.
func TestASubdirectorySessionRestoresWorkAcrossItsWholeCheckout(t *testing.T) {
	temp := t.TempDir()
	spec, archivePath := checkpointFixture(t, temp)
	repository := filepath.Join(spec.WorkspaceRoot, "app")
	checkpointMkdirAll(t, filepath.Join(repository, "docs"))
	checkpointWriteFile(t, filepath.Join(repository, "docs/guide.md"), []byte("guide"))
	gitLine(repository, "add", ".")
	gitLine(repository, "commit", "-m", "docs")
	base := gitLine(repository, "rev-parse", "HEAD")
	checkout := filepath.Join(temp, "clones/session")
	gitLine(repository, "worktree", "add", "-b", "mj/session", checkout, "HEAD")
	checkpointWriteFile(t, filepath.Join(checkout, "README.md"), []byte("edited at the root"))
	checkpointWriteFile(t, filepath.Join(checkout, "docs/guide.md"), []byte("staged in docs"))
	gitLine(checkout, "add", "docs/guide.md")
	checkpointWriteFile(t, filepath.Join(checkout, "notes.txt"), []byte("untracked at the root"))
	checkpointWriteFile(t, filepath.Join(checkout, "docs/draft.md"), []byte("untracked in docs"))
	// The controller describes a subdirectory project by its parent and its
	// own name, so the repository the worker captures is `<checkout>/docs`.
	spec.WorkspaceRoot = checkout
	spec.Repositories[0].RelativeDestination = "docs"
	spec.Repositories[0].Capture = CheckpointRepositoryCapture{Mode: CheckpointRepositoryCaptureDeltaFrom, BaseCommit: base}
	checkpointMust(ExportCheckpoint(&spec))(t)
	// An older build would unpack the untracked files inside `docs`; the
	// schema makes it refuse the archive instead.
	if version := checkpointMust(ReadArchiveVerified(archivePath))(t).Manifest.SchemaVersion; version != core.ArchiveSchemaVersionCheckoutSubdirectory {
		t.Fatalf("schema version = %d", version)
	}

	assertRestored := func(restored string) {
		t.Helper()
		read := func(path string) *string {
			body, err := os.ReadFile(filepath.Join(restored, path))
			if err != nil {
				return nil
			}
			text := string(body)
			return &text
		}
		expected := map[string]*string{
			"notes.txt":     ptrTo("untracked at the root"),
			"docs/draft.md": ptrTo("untracked in docs"),
			"draft.md":      nil,
			"README.md":     ptrTo("edited at the root"),
		}
		for _, path := range []string{"notes.txt", "docs/draft.md", "draft.md", "README.md"} {
			if got, want := read(path), expected[path]; !reflect.DeepEqual(got, want) {
				t.Fatalf("work in %s: %s = %v, want %v", restored, path, got, want)
			}
		}
		if staged := gitLine(restored, "diff", "--cached", "--name-only"); staged != "docs/guide.md" {
			t.Fatalf("the staged change is staged again: %q", staged)
		}
	}

	// Stop removes the managed worktree. An in-place resume recreates it
	// from the retained branch and restores the archive through the project
	// directory, in the same parent-and-name layout the export used.
	gitLine(repository, "worktree", "remove", "--force", checkout)
	gitLine(repository, "worktree", "add", checkout, "mj/session")
	checkpointNoError(t, RestoreRepositories(archivePath, checkout, SystemGit{}))
	assertRestored(checkout)

	// A restore onto a named branch is handed the checkout's top level.
	other := filepath.Join(temp, "clones/other")
	gitLine(repository, "worktree", "add", "-b", "mj/other", other, "mj/session")
	checkpointMust(RestoreSingleRepositoryOntoBranch(archivePath, other, "mj/other", SystemGit{}))(t)
	assertRestored(other)
}

// ptrTo is a pointer to a copy of value.
func ptrTo[T any](value T) *T { return &value }
