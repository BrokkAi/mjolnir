// Port of mj-import/src/import/native_import.rs.

package mjimport

import (
	"bufio"
	"bytes"
	"errors"
	"fmt"
	"io"
	"maps"
	"os"
	"os/exec"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// NativeImportRequest is one already-located, already-parsed native session
// to import. The optional fields are nil when absent.
type NativeImportRequest struct {
	Harness           core.HarnessKind
	HarnessHome       string
	NativeSessionID   string
	SourcePath        string
	Transcript        *ClaudeTranscript
	BundleID          string
	ProfileID         *string
	Title             *string
	DaemonProjectRoot *string
	ArchiveDirectory  string
}

// ImportNativeSession builds, verifies, and installs a local archive for one
// already-located, already-parsed native session, for any harness, then
// updates the in-memory state. The caller saves state only after this
// returns successfully. control is nil when the caller neither cancels nor
// follows progress.
func ImportNativeSession(
	config *core.Config,
	state *core.StateState,
	request NativeImportRequest,
	control *ImportControl,
) (ImportedClaudeSession, error) {
	harness := request.Harness
	nativeSessionID := request.NativeSessionID
	sourcePath := request.SourcePath
	bundleID := request.BundleID
	bundle, ok := config.Bundles[bundleID]
	if !ok {
		return ImportedClaudeSession{}, fmt.Errorf("unknown bundle %s", core.RustDebugString(bundleID))
	}
	var sessionTitleOverride *string
	var title string
	switch {
	case request.Title != nil && strings.TrimSpace(*request.Title) != "":
		override := *request.Title
		sessionTitleOverride = &override
		title = override
	case request.Title != nil:
		return ImportedClaudeSession{}, errors.New("import title must not be empty")
	default:
		title = fmt.Sprintf("Imported %s session %s", harness.DisplayName(), nativeSessionID)
	}
	nativeArtifacts, err := checkpoint.CollectImportNativeArtifacts(harness, request.HarnessHome, nativeSessionID, sourcePath)
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	// Own the selected native bytes through archive publication. A harness
	// may keep writing its source files after this import's canonical
	// capture.
	if err := os.MkdirAll(request.ArchiveDirectory, 0o777); err != nil {
		return ImportedClaudeSession{}, importIOError(err)
	}
	nativeSnapshot, err := os.MkdirTemp(request.ArchiveDirectory, "native-import-")
	if err != nil {
		return ImportedClaudeSession{}, importIOError(err)
	}
	defer os.RemoveAll(nativeSnapshot)
	var museCapture *string
	for index := range nativeArtifacts {
		artifact := &nativeArtifacts[index]
		if control != nil {
			if err := control.checkCancelled(); err != nil {
				return ImportedClaudeSession{}, err
			}
		}
		path := filepath.Join(nativeSnapshot, strconv.Itoa(index))
		original, isFile := artifact.Data.(core.NativeArtifactDataFile)
		selectedMuseStream := harness == core.HarnessKindMuse && isFile &&
			importComparePaths(string(original), sourcePath) == 0
		if err := copyNativeArtifact(artifact, path, control); err != nil {
			return ImportedClaudeSession{}, err
		}
		if selectedMuseStream {
			metadata, err := os.Stat(sourcePath)
			if err != nil {
				return ImportedClaudeSession{}, importIOError(err)
			}
			if err := os.Chtimes(path, time.Time{}, metadata.ModTime()); err != nil {
				return ImportedClaudeSession{}, importIOError(err)
			}
			captured := path
			museCapture = &captured
		}
		artifact.Data = core.NativeArtifactDataFile(path)
	}
	transcriptForImport := request.Transcript
	if harness == core.HarnessKindMuse {
		if control != nil {
			if err := control.checkCancelled(); err != nil {
				return ImportedClaudeSession{}, err
			}
		}
		if museCapture == nil {
			return ImportedClaudeSession{}, errors.New("selected Muse log is missing from the capture")
		}
		current, err := readMuseTranscript(*museCapture, sourcePath)
		if err != nil {
			return ImportedClaudeSession{}, fmt.Errorf("validate captured Muse session: %w", err)
		}
		// The preview can precede confirmation by minutes. Reject changes
		// that could invalidate its bundle, safety, or conversation.
		previewed := request.Transcript
		if current.Cwd != previewed.Cwd ||
			!slices.Equal(current.EditedPaths, previewed.EditedPaths) ||
			!transcriptItemsEqual(current.items, previewed.items) ||
			!optionalInt64Equal(current.lastActivityAtMs, previewed.lastActivityAtMs) {
			return ImportedClaudeSession{}, errors.New("native session changed after it was selected; select it again")
		}
		recollected, err := checkpoint.CollectImportNativeArtifacts(harness, request.HarnessHome, nativeSessionID, sourcePath)
		if err != nil {
			return ImportedClaudeSession{}, err
		}
		unchanged, err := nativeImportArtifactsMatch(nativeArtifacts, recollected)
		if err != nil {
			return ImportedClaudeSession{}, err
		}
		if !unchanged {
			return ImportedClaudeSession{}, errors.New("native session changed while being imported; stop its harness and retry")
		}
		transcriptForImport = &current
	}
	targets, err := SessionEditTargetsFor(transcriptForImport, request.HarnessHome)
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	rawDirectory, rawTargetID, raw := RawProjectImport(config, &targets)
	recordedRoot := ImportProjectRoot(transcriptForImport, &targets)
	var remap *importProjectRemap
	if destination := request.DaemonProjectRoot; destination != nil && importComparePaths(*destination, recordedRoot) != 0 {
		created, err := newImportProjectRemap(recordedRoot, *destination, targets.GitRoots)
		if err != nil {
			return ImportedClaudeSession{}, err
		}
		remap = &created
	}
	repositories, err := collectLocalRepositories(&bundle, targets.GitRoots, !raw, control)
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	sessionID, err := core.NewSessionID()
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	canonicalSession, err := transcript.CanonicalSessionFromMaterializedOwned(transcriptForImport.materialize(sessionID))
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	eventFrontier := canonicalSession.EventFrontier
	createdAt := timestamp()
	profileID, err := importProfileID(config, request.ProfileID, harness, request.HarnessHome)
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	targetID := defaultImportTargetID(config)
	project, err := Snapshot(&bundle, core.CancellableProcessExecutorWithTimeout(15*time.Second), !raw)
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	var remappedBundle *core.ProjectBundle
	if remap != nil {
		if err := remap.apply(&project, repositories); err != nil {
			return ImportedClaudeSession{}, err
		}
		remapped := project.Bundle
		remappedBundle = &remapped
	}
	recordedBundleID := bundleID
	if remappedBundle != nil {
		recordedBundleID = "import-" + sessionID
	}
	archivePath := filepath.Join(request.ArchiveDirectory, sessionID+".hel.zip")
	if control != nil {
		if err := control.Report(ImportArchiveProgressWritingArchive{}); err != nil {
			return ImportedClaudeSession{}, err
		}
	}
	verified, err := checkpoint.WriteArchiveAtomic(archivePath, &checkpoint.ArchiveInput{
		Session: core.SessionManifest{
			ID:              sessionID,
			Title:           title,
			HarnessKind:     harness,
			ProfileID:       profileID,
			NativeSessionID: nativeSessionID,
			CreatedAt:       createdAt,
			CheckpointedAt:  createdAt,
			HelVersion:      core.PackageVersion,
			RelayVersion:    core.PackageVersion,
			AdapterVersion:  "acp-v1",
		},
		Target: core.TargetManifest{
			TemplateID: targetID,
			TargetKind: "import",
			Details:    map[string]string{"source": harness.ID() + "-import"},
		},
		Bundle: core.BundleManifest{
			ID:                recordedBundleID,
			PrimaryRepository: bundle.PrimaryRepo,
		},
		CanonicalSession: canonicalSession,
		NativeArtifacts:  nativeArtifacts,
		Repositories:     repositories,
	})
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	if control != nil {
		if err := control.checkCancelled(); err != nil {
			_ = os.Remove(archivePath)
			return ImportedClaudeSession{}, err
		}
	}
	checkpointMetadata := core.CheckpointMetadata{
		ArchivePath:   archivePath,
		Sha256:        verified.ArchiveSha256,
		CreatedAt:     createdAt,
		EventFrontier: eventFrontier,
	}
	containerWorkspace, err := core.NewContainerWorkspace(sessionID)
	if err != nil {
		return ImportedClaudeSession{}, err
	}
	var projectDirectory *string
	recordedTargetID := targetID
	if raw {
		directory := rawDirectory
		if request.DaemonProjectRoot != nil {
			directory = *request.DaemonProjectRoot
		}
		projectDirectory = &directory
		recordedTargetID = rawTargetID
	}
	recordedNativeSessionID := nativeSessionID
	state.Sessions.Insert(sessionID, core.SessionRecord{
		OracleChoice:         nil,
		Oracle:               true,
		NotificationsIgnored: false,
		Project:              &project,
		TargetRuntime:        nil,
		LaunchBase:           nil,
		LaunchBranch:         nil,
		Checkout:             nil,
		Publication:          nil,
		BuildCache:           nil,
		Subagents:            nil,
		// An imported history is a new session: when it is resumed into a
		// container it gets its own workspace, like any session created now.
		ContainerWorkspace:        &containerWorkspace,
		CreateManagedWorktree:     nil,
		WorkspaceID:               core.DefaultWorkspaceID,
		Archived:                  false,
		ID:                        sessionID,
		Title:                     title,
		HarnessKind:               harness,
		LastProfile:               profileID,
		BundleID:                  recordedBundleID,
		ProjectDirectory:          projectDirectory,
		ManagedWorktree:           nil,
		Review:                    nil,
		NoProjectMemory:           false,
		NoGithubEvents:            false,
		NoGithubCredentials:       !config.Github.SessionCredentials,
		TargetTemplateID:          recordedTargetID,
		ResourceAllocation:        nil,
		AdditionalMounts:          []core.AdditionalMount{},
		State:                     core.SessionStateStopped,
		Target:                    nil,
		NativeSessionID:           &recordedNativeSessionID,
		ACPSessionTitle:           nil,
		SessionTitleOverride:      sessionTitleOverride,
		CreatedAt:                 createdAt,
		UpdatedAt:                 createdAt,
		ViewedThroughEventOrdinal: 0,
		DraftInput:                "",
		LastError:                 nil,
		DismissedFailure:          nil,
		FailureGeneration:         0,
		LastCheckpointError:       nil,
		Checkpoint:                &checkpointMetadata,
	})
	return ImportedClaudeSession{
		SessionID:       sessionID,
		NativeSessionID: nativeSessionID,
		SourceJSONL:     sourcePath,
		SourceCwd:       transcriptForImport.Cwd,
		BundleID:        recordedBundleID,
		ArchivePath:     archivePath,
		RemappedBundle:  remappedBundle,
	}, nil
}

// copyNativeArtifact copies one native artifact's bytes into path, checking
// for cancellation before every read.
func copyNativeArtifact(artifact *core.NativeArtifact, path string, control *ImportControl) error {
	reader, err := artifact.Reader()
	if err != nil {
		return err
	}
	defer reader.Close()
	target, err := os.Create(path)
	if err != nil {
		return importIOError(err)
	}
	if _, err := io.Copy(target, &cancellableRead{inner: reader, control: control}); err != nil {
		target.Close()
		return importIOError(err)
	}
	if err := target.Close(); err != nil {
		return importIOError(err)
	}
	return nil
}

// optionalInt64Equal is Rust's `Option<i64> == Option<i64>`.
func optionalInt64Equal(left, right *int64) bool {
	if left == nil || right == nil {
		return left == nil && right == nil
	}
	return *left == *right
}

func defaultImportTargetID(config *core.Config) string {
	if _, ok := config.Targets["podman"]; ok {
		return "podman"
	}
	ids := slices.Sorted(maps.Keys(config.Targets))
	for _, id := range ids {
		switch config.Targets[id].(type) {
		case core.TargetTemplateLocalPodman, core.TargetTemplateLocalDocker,
			core.TargetTemplateSshPodman, core.TargetTemplateSshDocker:
			return id
		}
	}
	if len(ids) > 0 {
		return ids[0]
	}
	return "import"
}

// rawImportTargetID is the target that hosts raw project sessions on this
// machine.
func rawImportTargetID(config *core.Config) (string, bool) {
	localBare := func(template core.TargetTemplate) bool {
		_, ok := template.(core.TargetTemplateLocalBare)
		return ok
	}
	if template, ok := config.Targets["localhost"]; ok && localBare(template) {
		return "localhost", true
	}
	for _, id := range slices.Sorted(maps.Keys(config.Targets)) {
		if localBare(config.Targets[id]) {
			return id, true
		}
	}
	return "", false
}

// RawProjectImport is the project directory and target of a raw project
// import, ok false when the session is imported as a bundle session.
//
// A session that only wrote to its own repository can keep working in that
// directory, so import it as a raw project session instead of a bundle
// session. SessionEditTargetsFor always records the cwd root, so a single
// durable root is that root.
func RawProjectImport(config *core.Config, targets *SessionEditTargets) (directory string, targetID string, ok bool) {
	if len(targets.GitRoots) != 1 {
		return "", "", false
	}
	targetID, ok = rawImportTargetID(config)
	if !ok {
		return "", "", false
	}
	return targets.GitRoots[0], targetID, true
}

func collectLocalRepositories(
	bundle *core.ProjectBundle,
	detectedRoots []string,
	isolated bool,
	control *ImportControl,
) ([]checkpoint.RepositorySnapshot, error) {
	// Rust collects (identity, root) pairs into a BTreeMap, so a later root
	// with the same identity replaces an earlier one.
	detected := map[core.RepositoryIdentity]string{}
	for _, root := range detectedRoots {
		identity, err := rootIdentity(root)
		if err != nil {
			return nil, err
		}
		detected[identity] = root
	}
	repositoryPaths := map[string]string{}
	for index := range bundle.Repositories {
		repository := &bundle.Repositories[index]
		var path string
		var found bool
		if repository.Local != nil {
			// A local source remains identified by its configured path even
			// after its checkout gains a network origin. GitHub-origin
			// detection is still used for configured network sources.
			configuredPath := importCanonicalizeOr(*repository.Local)
			for _, root := range detectedRoots {
				if importComparePaths(importCanonicalizeOr(root), configuredPath) == 0 {
					path, found = root, true
					break
				}
			}
		} else {
			identity, err := ConfiguredRepositoryIdentity(repository)
			if err != nil {
				return nil, err
			}
			if identity == nil {
				return nil, fmt.Errorf("repository %s has no usable source", core.RustDebugString(repository.ID))
			}
			path, found = detected[identity]
		}
		if !found {
			return nil, fmt.Errorf("repository %s was not detected in the native session", core.RustDebugString(repository.ID))
		}
		repositoryPaths[repository.ID] = path
	}
	git := checkpoint.SystemGit{}
	repositoryCount := len(bundle.Repositories)
	untrackedFiles := checkpoint.UntrackedFilesInclude
	if control != nil && !control.IncludeUntracked {
		untrackedFiles = checkpoint.UntrackedFilesExclude
	}
	// Rust's indexed parallel iteration keeps repository and manifest order
	// identical to the configured bundle; so does collecting by index here.
	snapshots := make([]checkpoint.RepositorySnapshot, repositoryCount)
	failures := make([]error, repositoryCount)
	var group sync.WaitGroup
	for index := range bundle.Repositories {
		group.Add(1)
		go func(index int) {
			defer group.Done()
			repository := &bundle.Repositories[index]
			snapshot, err := collectLocalRepository(git, repository, repositoryPaths[repository.ID], index, repositoryCount, isolated, untrackedFiles, control)
			snapshots[index], failures[index] = snapshot, err
		}(index)
	}
	group.Wait()
	for _, err := range failures {
		if err != nil {
			return nil, err
		}
	}
	return snapshots, nil
}

// collectLocalRepository snapshots one bundle repository from its detected
// checkout at path.
func collectLocalRepository(
	git checkpoint.SystemGit,
	repository *core.ProjectRepository,
	path string,
	index int,
	total int,
	isolated bool,
	untrackedFiles checkpoint.UntrackedFiles,
	control *ImportControl,
) (checkpoint.RepositorySnapshot, error) {
	if control != nil {
		if err := control.Report(ImportArchiveProgressRepository{Current: index + 1, Total: total, ID: repository.ID}); err != nil {
			return checkpoint.RepositorySnapshot{}, err
		}
	}
	if !importIsDir(path) {
		return checkpoint.RepositorySnapshot{}, fmt.Errorf("local repository %s is missing at %s",
			core.RustDebugString(repository.ID), path)
	}
	var source *core.NetworkGitSource
	if isolated {
		resolved, err := core.ResolveRepository(repository, core.ProcessExecutor{})
		if err != nil {
			return checkpoint.RepositorySnapshot{}, fmt.Errorf("resolve network source for %s: %w",
				core.RustDebugString(repository.ID), err)
		}
		source = &resolved
	}
	// Isolated imports restore the native session's committed work on top
	// of the source's available network baseline. Raw imports continue to
	// use the live local checkout, including repositories without any remote.
	var history checkpoint.GitHistoryMode = checkpoint.GitHistoryModeNoBundle{}
	var originOverride string
	if source != nil {
		base, err := importDeltaBase(path, source.FetchURL)
		if err != nil {
			return checkpoint.RepositorySnapshot{}, err
		}
		history = checkpoint.GitHistoryModeDeltaFrom(base)
		originOverride = source.FetchURL
	} else {
		originOverride = strings.ToValidUTF8(path, "�")
	}
	snapshot, err := checkpoint.CollectGitSnapshotWithProgress(
		git,
		path,
		&checkpoint.GitCollectionSpec{
			ID:                  repository.ID,
			RelativeDestination: repository.Destination,
			History:             history,
			OriginOverride:      &originOverride,
		},
		untrackedFiles,
		func(progress checkpoint.GitSnapshotProgress) error {
			if control == nil {
				return nil
			}
			switch progress := progress.(type) {
			case checkpoint.GitSnapshotProgressUntrackedFile:
				return control.Report(ImportArchiveProgressUntrackedFile{
					RepositoryID: repository.ID,
					Current:      progress.Current,
					Total:        progress.Total,
					Path:         progress.Path,
				})
			}
			return nil
		},
	)
	if err != nil {
		return checkpoint.RepositorySnapshot{}, fmt.Errorf("collect local repository %s: %w",
			core.RustDebugString(repository.ID), err)
	}
	if source != nil {
		pushURLs := make([]string, 0, len(source.PushURLs))
		for _, url := range source.PushURLs {
			redacted, err := checkpoint.RedactOriginCredentials(url)
			if err != nil {
				return checkpoint.RepositorySnapshot{}, err
			}
			pushURLs = append(pushURLs, redacted)
		}
		snapshot.Metadata.PushURLs = pushURLs
		snapshot.Metadata.RemoteWorkspace = true
	} else {
		snapshot.Metadata.PushURLs = []string{}
	}
	return snapshot, nil
}

func defaultProfile(config *core.Config, harness core.HarnessKind, home string) string {
	source := importCanonicalizeOr(home)
	for id, profile := range config.EnabledProfiles() {
		if profile.Kind == harness && importComparePaths(importCanonicalizeOr(profile.Home), source) == 0 {
			return id
		}
	}
	for id, profile := range config.EnabledProfiles() {
		if profile.Kind == harness {
			return id
		}
	}
	return harness.ID() + "-import"
}

// importProfileID is the profile an import records; requested is nil when
// the user named none.
func importProfileID(config *core.Config, requested *string, harness core.HarnessKind, home string) (string, error) {
	if requested == nil {
		return defaultProfile(config, harness, home), nil
	}
	profile, err := config.NamedProfile(*requested)
	if err != nil {
		return "", err
	}
	if profile.Kind != harness {
		return "", fmt.Errorf("import profile %s does not use %s",
			core.RustDebugString(*requested), importHarnessDebug(harness))
	}
	return *requested, nil
}

// importDeltaBase is the upstream revision an imported repository deltas
// from. A repository without remote-tracking refs cannot tell us which
// ancestry a newly provisioned clone has, and Hel never bundles full
// history, so it fails here.
func importDeltaBase(path string, fetchURL string) (string, error) {
	remotes, _, err := gitOptionalText(path, "remote")
	if err != nil {
		return "", err
	}
	upstreamRef, hasUpstream, err := gitOptionalText(path,
		"rev-parse", "--symbolic-full-name", "--verify", "--quiet", "@{upstream}")
	if err != nil {
		return "", err
	}
	for _, remote := range importLines(remotes) {
		url, ok, err := gitOptionalText(path, "remote", "get-url", remote)
		if err != nil {
			return "", err
		}
		if !ok {
			continue
		}
		// `url` comes from `git remote get-url`, where `owner/repo` is a
		// relative path; `fetchURL` is a resolved source.
		sameSource := url == fetchURL
		if !sameSource {
			if identity := core.ProjectSourceIdentityGitRemote(url, core.ShorthandRejected); identity != nil {
				fetched := core.ProjectSourceIdentityGitRemote(fetchURL, core.ShorthandAllowed)
				sameSource = fetched != nil && *identity == *fetched
			}
		}
		if !sameSource {
			continue
		}
		prefix := "refs/remotes/" + remote + "/"
		revision := prefix + "HEAD"
		if hasUpstream && strings.HasPrefix(upstreamRef, prefix) {
			revision = upstreamRef
		}
		base, ok, err := gitOptionalText(path, "rev-parse", "--verify", "--quiet", revision)
		if err != nil {
			return "", err
		}
		if ok {
			return base, nil
		}
	}
	return "", fmt.Errorf("repository %s has no remote-tracking refs to import against for its selected network source; fetch its remote first", path)
}

// gitOptionalText runs `git <arguments>` in cwd and returns its trimmed
// output; false when Git fails or prints nothing.
func gitOptionalText(cwd string, arguments ...string) (string, bool, error) {
	command := exec.Command("git", arguments...)
	command.Dir = cwd
	output, err := core.RunWithInput(command, nil)
	if err != nil {
		return "", false, fmt.Errorf("start git in %s: %w", cwd, err)
	}
	if !output.Status.Success() {
		return "", false, nil
	}
	if !utf8.Valid(output.Stdout) {
		return "", false, errors.New("decode Git output: invalid utf-8 sequence")
	}
	text := strings.TrimSpace(string(output.Stdout))
	if text == "" {
		return "", false, nil
	}
	return text, true, nil
}

// timestamp is now in RFC 3339 with whole seconds and a `Z` offset.
func timestamp() string {
	return time.Now().UTC().Format("2006-01-02T15:04:05Z")
}

// PersistImportedSessionLocally saves an imported session and its
// materialized transcript, read back from its verified archive.
func PersistImportedSessionLocally(session *core.SessionRecord) error {
	if session.Checkpoint == nil {
		return errors.New("imported session has no checkpoint")
	}
	verified, err := checkpoint.VerifyArchiveStreaming(session.Checkpoint.ArchivePath)
	if err != nil {
		return err
	}
	materialized, err := transcript.MaterializedSessionFromCanonical(session.ID, &verified.CanonicalSession)
	if err != nil {
		return err
	}
	return store.SaveImportedSession(session, &materialized)
}

// ImportProjectRoot is the recorded project root used by both the path
// prompt and archive relocation.
//
// Git reports the roots as physical paths, while the transcript keeps the
// working directory as the harness saw it, which can run through a symlink
// (macOS keeps /var and /tmp behind /private). Both are compared, and the
// root is returned, as physical paths.
func ImportProjectRoot(transcript *ClaudeTranscript, targets *SessionEditTargets) string {
	cwd := canonicalizeExistingAncestor(transcript.Cwd)
	for _, root := range targets.GitRoots {
		if importPathStartsWith(cwd, root) {
			return root
		}
	}
	return cwd
}

// importProjectRemap relocates operational project metadata together;
// native history retains its provenance.
type importProjectRemap struct {
	recorded string
	// recordedPhysical is recorded as a physical path. Every containment
	// test is made there: the paths being moved come spelled as configured,
	// as Git reported them or as the harness recorded them, and one spelling
	// may run through a symlink another resolved (macOS keeps /var and /tmp
	// behind /private).
	recordedPhysical string
	destination      string
}

// newImportProjectRemap is Rust's `ImportProjectRemap::new`.
func newImportProjectRemap(recorded string, destination string, roots []string) (importProjectRemap, error) {
	if !core.IsAbsoluteOnHostOrTarget(destination) {
		return importProjectRemap{}, errors.New("Enter an absolute project root on the daemon host, or keep the recorded root.")
	}
	remap := importProjectRemap{
		recorded:         recorded,
		recordedPhysical: canonicalizeExistingAncestor(recorded),
		destination:      destination,
	}
	outside := []string{}
	for _, root := range roots {
		if _, ok := remap.within(root); !ok {
			outside = append(outside, root)
		}
	}
	if len(outside) > 0 {
		return importProjectRemap{}, fmt.Errorf("cannot relocate repositories outside recorded project root %s: %s. Import one repository at a time, or record the session from their common parent and import again with --path pointing to that parent's daemon-host counterpart.",
			recorded, strings.Join(outside, ", "))
	}
	return remap, nil
}

// within is path relative to the recorded root; false when it lies outside
// it or is not a filesystem path (a remote URL).
func (r *importProjectRemap) within(path string) (string, bool) {
	if !filepath.IsAbs(path) {
		return "", false
	}
	return importPathStripPrefix(canonicalizeExistingAncestor(path), r.recordedPhysical)
}

// remap is path moved under the destination; false when it lies outside the
// recorded root.
func (r *importProjectRemap) remap(path string) (string, bool) {
	relative, ok := r.within(path)
	if !ok {
		return "", false
	}
	if relative == "" {
		return r.destination, true
	}
	// Daemon project paths obey the POSIX session-target contract.
	return core.Join(r.destination, relative), true
}

func (r *importProjectRemap) apply(project *core.ProjectBundleSnapshot, repositories []checkpoint.RepositorySnapshot) error {
	remapURL := func(url *string) {
		if path, ok := r.remap(*url); ok {
			*url = strings.ToValidUTF8(path, "�")
		}
	}
	for index := range project.Bundle.Repositories {
		repository := &project.Bundle.Repositories[index]
		if repository.Local == nil {
			continue
		}
		mapped, ok := r.remap(*repository.Local)
		if !ok {
			return fmt.Errorf("repository %s at %s is outside recorded project root %s; import one repository at a time",
				repository.ID, *repository.Local, r.recorded)
		}
		repository.Local = &mapped
	}
	for id, identity := range project.Identities {
		if local, ok := identity.(core.RepositoryIdentityLocal); ok {
			if mapped, ok := r.remap(local.Root); ok {
				project.Identities[id] = core.RepositoryIdentityLocal{Root: mapped}
			}
		}
	}
	for id, source := range project.NetworkSources {
		remapURL(&source.FetchURL)
		pushURLs := append([]string(nil), source.PushURLs...)
		for index := range pushURLs {
			remapURL(&pushURLs[index])
		}
		source.PushURLs = pushURLs
		project.NetworkSources[id] = source
	}
	for index := range repositories {
		metadata := &repositories[index].Metadata
		remapURL(&metadata.Origin)
		for urlIndex := range metadata.PushURLs {
			remapURL(&metadata.PushURLs[urlIndex])
		}
	}
	return nil
}

// nativeImportArtifactsMatch compares captured contents, independently of
// whether bodies live in memory or private snapshot files. Reader chunk
// boundaries need not agree.
func nativeImportArtifactsMatch(captured []core.NativeArtifact, current []core.NativeArtifact) (bool, error) {
	if len(captured) != len(current) {
		return false, nil
	}
	for index := range captured {
		if captured[index].RelativePath != current[index].RelativePath || captured[index].Mode != current[index].Mode {
			return false, nil
		}
		same, err := nativeArtifactContentsMatch(&captured[index], &current[index])
		if err != nil || !same {
			return false, err
		}
	}
	return true, nil
}

// nativeArtifactContentsMatch compares two artifacts' bytes as they stream.
func nativeArtifactContentsMatch(captured, current *core.NativeArtifact) (bool, error) {
	capturedReader, err := captured.Reader()
	if err != nil {
		return false, err
	}
	defer capturedReader.Close()
	currentReader, err := current.Reader()
	if err != nil {
		return false, err
	}
	defer currentReader.Close()
	left := bufio.NewReader(capturedReader)
	right := bufio.NewReader(currentReader)
	for {
		leftBuffer, leftErr := nativeFillBuffer(left)
		if leftErr != nil {
			return false, leftErr
		}
		rightBuffer, rightErr := nativeFillBuffer(right)
		if rightErr != nil {
			return false, rightErr
		}
		if len(leftBuffer) == 0 && len(rightBuffer) == 0 {
			return true, nil
		}
		size := min(len(leftBuffer), len(rightBuffer))
		if size == 0 || !bytes.Equal(leftBuffer[:size], rightBuffer[:size]) {
			return false, nil
		}
		if _, err := left.Discard(size); err != nil {
			return false, importIOError(err)
		}
		if _, err := right.Discard(size); err != nil {
			return false, importIOError(err)
		}
	}
}

// nativeFillBuffer is Rust's `BufRead::fill_buf`: the buffered bytes, read
// from the source when none are buffered, empty at the end.
func nativeFillBuffer(reader *bufio.Reader) ([]byte, error) {
	if reader.Buffered() == 0 {
		if _, err := reader.Peek(1); err != nil && !errors.Is(err, io.EOF) {
			return nil, importIOError(err)
		}
	}
	return reader.Peek(reader.Buffered())
}

// cancellableRead checks import cancellation before every read, so
// cancelling stops a copy of an arbitrarily large native file instead of
// waiting for it to finish. control is nil when the import cannot be
// cancelled.
type cancellableRead struct {
	inner   io.Reader
	control *ImportControl
}

func (r *cancellableRead) Read(buffer []byte) (int, error) {
	if r.control != nil {
		if err := r.control.checkCancelled(); err != nil {
			return 0, err
		}
	}
	return r.inner.Read(buffer)
}
