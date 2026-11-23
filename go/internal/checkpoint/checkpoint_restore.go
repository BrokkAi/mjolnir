// Port of mj-checkpoint/src/checkpoint/restore.rs.

package checkpoint

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"maps"
	"os"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"
	acp "github.com/BrokkAi/acp-go/schema/unstable"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// QueueRestorePolicy is who dispatches the archived queue and the archived
// worker's pending mail.
//
// Pending mail joins the same dispatch decision as the queue: a restored
// worker given a waking event starts a turn on its own, so mail the
// controller must hold back (behind a Move's ownership commit, or behind a
// fresh native session's handoff) is admitted by the controller with the
// queue. Mail already claimed by a queued prompt travels with that prompt.
// (The mail half is Rust commit 39722ace5, after this file's base.)
type QueueRestorePolicy string

const (
	// QueueRestorePolicyRestore: the destination worker owns its queue and
	// pending mail at once.
	QueueRestorePolicyRestore QueueRestorePolicy = "restore"
	// QueueRestorePolicyDiscard: drop the queue and the mail its prompts
	// claimed; the destination worker owns pending mail at once.
	QueueRestorePolicyDiscard QueueRestorePolicy = "discard"
	// QueueRestorePolicyDefer: the controller admits the queue and pending
	// mail after readiness. The mail a queued prompt claimed is bound to its
	// command ID now, so it is there before that prompt can start.
	QueueRestorePolicyDefer QueueRestorePolicy = "defer"
	// QueueRestorePolicyDiscardDeferringMail: drop the queue and the mail
	// its prompts claimed; the controller admits pending mail after
	// readiness.
	QueueRestorePolicyDiscardDeferringMail QueueRestorePolicy = "discard_deferring_mail"
)

// DefersMail reports whether the controller, not the restored worker,
// admits pending mail.
func (p QueueRestorePolicy) DefersMail() bool {
	return p == QueueRestorePolicyDefer || p == QueueRestorePolicyDiscardDeferringMail
}

func (p QueueRestorePolicy) keepsQueue() bool {
	return p == QueueRestorePolicyRestore || p == QueueRestorePolicyDefer
}

// UnmarshalJSON accepts only the snake_case variant names, as serde does.
func (p *QueueRestorePolicy) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch policy := QueueRestorePolicy(text); policy {
	case QueueRestorePolicyRestore, QueueRestorePolicyDiscard, QueueRestorePolicyDefer, QueueRestorePolicyDiscardDeferringMail:
		*p = policy
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected one of `restore`, `discard`, `defer`, `discard_deferring_mail`", text)
}

// RelaySeedForRestore is the relay metadata needed to continue a verified
// canonical session. An in-place native Move already has its history and
// attachments on the target.
func RelaySeedForRestore(canonical *core.CanonicalSessionSnapshot, nativeContinuity bool, queuePolicy QueueRestorePolicy) (core.RestoredRelaySeed, error) {
	seed := core.RestoredRelaySeed{
		EventFrontier:       canonical.EventFrontier,
		EventFrontierDigest: canonical.EventFrontierDigest,
		QueuedPrompts:       []core.CanonicalQueuedPrompt{},
	}
	if nativeContinuity {
		if !jsonvalue.IsNull(canonical.AssessmentState) {
			seed.AssessmentState = slices.Clone(canonical.AssessmentState)
		}
		seed.NotificationStack = canonical.NotificationStack
		seed.AcceptedConfig = core.AcceptedSessionConfigFromArchivedValues(canonical.Session.Configuration.Values)
	}
	if queuePolicy == QueueRestorePolicyRestore {
		seed.QueuedPrompts = append(seed.QueuedPrompts, canonical.QueuedPrompts...)
	}
	seed.NativeSessionUnused = nativeContinuity && CurrentNativeSessionIsUnused(canonical, nil)
	if archived := canonical.Mailbox; archived != nil {
		mailbox := core.CanonicalMailbox{DeliveredEventKeys: maps.Clone(archived.DeliveredEventKeys)}
		if !queuePolicy.DefersMail() {
			mailbox.PendingEvents = slices.Clone(archived.PendingEvents)
		}
		if queuePolicy.keepsQueue() {
			mailbox.PromptContexts = maps.Clone(archived.PromptContexts)
		}
		if !mailbox.IsEmpty() {
			seed.Mailbox = &mailbox
		}
	}
	if nativeContinuity && canonical.NativeUseEvidence != nil {
		evidence := *canonical.NativeUseEvidence
		seed.NativeUseEvidence = &evidence
	}
	if err := seed.Validate(); err != nil {
		return core.RestoredRelaySeed{}, err
	}
	return seed, nil
}

// deserializeQueuePolicy reads the queue policy or, from older controllers,
// the legacy discard_queued_prompts boolean (an untagged enum in Rust).
func deserializeQueuePolicy(data []byte) (QueueRestorePolicy, error) {
	var legacy bool
	if json.Unmarshal(data, &legacy) == nil {
		if legacy {
			return QueueRestorePolicyDiscard, nil
		}
		return QueueRestorePolicyRestore, nil
	}
	var policy QueueRestorePolicy
	if json.Unmarshal(data, &policy) == nil {
		return policy, nil
	}
	return "", errors.New("data did not match any variant of untagged enum Wire")
}

type CheckpointRestoreSpec struct {
	ArchivePath   string `json:"archive_path"`
	WorkspaceRoot string `json:"workspace_root"`
	// ReportRoot is the target-side root for report files; older controllers
	// restore beside repositories.
	ReportRoot          *string `json:"report_root,omitempty"`
	RelayRoot           string  `json:"relay_root"`
	HarnessHome         string  `json:"harness_home"`
	RestoreRepositories bool    `json:"restore_repositories"`
	RestoreNative       bool    `json:"restore_native"`
	// QueuePolicy is also read from the legacy alias
	// "discard_queued_prompts", as a policy or a boolean.
	QueuePolicy QueueRestorePolicy `json:"queue_policy"`
	// PrimaryRepositoryRoot is where the primary repository actually sits,
	// when that is not WorkspaceRoot joined with the archived destination. A
	// resume that moves a session between representations puts the checkout
	// somewhere the archive could not have named, and the restored harness
	// session has to point at the real working directory.
	PrimaryRepositoryRoot *string `json:"primary_repository_root,omitempty"`
}

// UnmarshalJSON refuses unknown fields, reads queue_policy through its alias
// and legacy boolean form, and requires it.
func (s *CheckpointRestoreSpec) UnmarshalJSON(data []byte) error {
	members, err := checkpointObject(data)
	if err != nil {
		return err
	}
	raw, ok := members["queue_policy"]
	if legacy, hasLegacy := members["discard_queued_prompts"]; hasLegacy {
		if ok {
			return errors.New("duplicate field `queue_policy`")
		}
		raw, ok = legacy, true
		delete(members, "discard_queued_prompts")
	}
	if ok {
		policy, err := deserializeQueuePolicy(raw)
		if err != nil {
			return err
		}
		members["queue_policy"] = json.RawMessage(strconv.Quote(string(policy)))
		if data, err = json.Marshal(members); err != nil {
			return err
		}
	}
	type plain CheckpointRestoreSpec
	return checkpointStrictDecode(data, (*plain)(s), true)
}

func RestoreCheckpoint(spec *CheckpointRestoreSpec, git GitCommandRunner) error {
	return RestoreCheckpointWithNativeState(spec, git, noNativeCheckpointState{})
}

func RestoreCheckpointWithNativeState(spec *CheckpointRestoreSpec, git GitCommandRunner, nativeState NativeCheckpointState) error {
	_, err := RestoreCheckpointWithNotices(spec, git, nativeState)
	return err
}

// RestoreCheckpointWithNotices restores a checkpoint and returns the resume
// notices its repositories reported.
func RestoreCheckpointWithNotices(spec *CheckpointRestoreSpec, git GitCommandRunner, nativeState NativeCheckpointState) ([]string, error) {
	if !checkpointIsDir(spec.WorkspaceRoot) {
		return nil, errors.New("restore workspace is missing")
	}
	// A restore seeds a relay that has no durable state yet. Existing state
	// means either a leaked worker still writing here or an unfinished
	// teardown, and the seed would be ignored in favour of the stale
	// snapshot, leaving a frontier no journal can support.
	if checkpointExists(core.Join(spec.RelayRoot, core.RelayStateFile)) {
		return nil, fmt.Errorf("relay state already present in %s; a previous worker may still be running, refusing to restore over it", spec.RelayRoot)
	}
	archive, err := readArchiveForRestore(spec.ArchivePath)
	if err != nil {
		return nil, err
	}
	payloadReader, err := CheckpointPayloadReaderOpen(spec.ArchivePath)
	if err != nil {
		return nil, err
	}
	defer payloadReader.Close()
	// Deserialize and validate the schema-2 canonical projection before any
	// repository, relay, or native-session state can be mutated.
	canonicalSession, err := archive.CanonicalSession()
	if err != nil {
		return nil, err
	}
	// The relay that opens next needs the frontier it continues from and the
	// commands still queued, and nothing else. The transcript stays in the
	// archive; the controller already holds it as the durable projection.
	seed, err := RelaySeedForRestore(&canonicalSession, spec.RestoreNative, spec.QueuePolicy)
	if err != nil {
		return nil, err
	}
	// Without repository restoration a Move has already transported
	// workspace files, but review refs still need their destination
	// checkout names without changing its index.
	notices, err := restoreRepositoriesFromArchive(spec.ArchivePath, &archive, spec.WorkspaceRoot,
		spec.PrimaryRepositoryRoot, git, spec.RestoreRepositories)
	if err != nil {
		return nil, err
	}

	// Attachment data belongs to the relay, even when native harness history
	// is deliberately not restored. Install before publishing the queue seed.
	imageStore := core.AttachmentStoreWorker(spec.RelayRoot)
	if err := restoreArchivedAttachments(&archive, &imageStore); err != nil {
		return nil, err
	}
	reportRoot := core.Join(spec.WorkspaceRoot, core.ReportRootDir)
	if spec.ReportRoot != nil {
		if reportRoot, err = ResolveTargetPath(*spec.ReportRoot); err != nil {
			return nil, err
		}
	}
	if err := restoreSubagentReports(&archive, spec.WorkspaceRoot, reportRoot); err != nil {
		return nil, err
	}
	for _, queued := range seed.QueuedPrompts {
		blocks := make([]acp.ContentBlock, 0, len(queued.Content))
		for _, content := range queued.Content {
			var block acp.ContentBlock
			if err := json.Unmarshal(content, &block); err != nil {
				return nil, err
			}
			blocks = append(blocks, block)
		}
		references, err := core.References(blocks)
		if err != nil {
			return nil, err
		}
		for i := range references {
			if _, err := imageStore.Read(&references[i]); err != nil {
				return nil, err
			}
		}
	}
	if err := os.MkdirAll(spec.RelayRoot, 0o777); err != nil {
		return nil, err
	}
	if err := core.ClearNativeSessionIdentity(spec.RelayRoot); err != nil {
		return nil, err
	}
	seedBody, err := checkpointMarshal(seed)
	if err != nil {
		return nil, err
	}
	if err := writePrivateFile(spec.RelayRoot, core.RestoredRelaySeedFile, seedBody, 0o600); err != nil {
		return nil, err
	}

	if spec.RestoreNative {
		if _, err := restoreNativeArtifacts(&archive, payloadReader, spec, nativeState, false); err != nil {
			return nil, err
		}
	}
	return notices, nil
}

// restoreArchivedAttachments installs the archive's attachment blobs in the
// relay's attachment store.
func restoreArchivedAttachments(archive *VerifiedArchive, store *core.AttachmentStore) error {
	for i := range archive.Manifest.Payloads {
		descriptor := &archive.Manifest.Payloads[i]
		if descriptor.Role.Kind != core.PayloadRoleNativeArtifact ||
			!checkpointPathStartsWith(descriptor.Role.RelativePath, core.ArchiveAttachmentDir) {
			continue
		}
		payload, err := archive.Payload(descriptor)
		if err != nil {
			return err
		}
		if _, err := store.RestoreArtifact(descriptor.Role.RelativePath, payload); err != nil {
			return err
		}
	}
	return nil
}

// CheckoutRecoverySpec is retryable reconstruction in a private staging
// checkout. Publishing must atomically refuse any directory that appeared at
// the current checkout path.
type CheckoutRecoverySpec struct {
	Version             uint32                        `json:"version"`
	CheckpointSha256    string                        `json:"checkpoint_sha256"`
	Restore             CheckpointRestoreSpec         `json:"restore"`
	StagingRoot         string                        `json:"staging_root"`
	StagedCheckout      string                        `json:"staged_checkout"`
	DestinationCheckout string                        `json:"destination_checkout"`
	AdditionalCheckouts []CheckoutRecoveryPublication `json:"additional_checkouts" serde:"default"`
}

// MarshalJSON writes an empty list as [], as serde does.
func (s CheckoutRecoverySpec) MarshalJSON() ([]byte, error) {
	type plain CheckoutRecoverySpec
	v := plain(s)
	if v.AdditionalCheckouts == nil {
		v.AdditionalCheckouts = []CheckoutRecoveryPublication{}
	}
	return checkpointMarshal(v)
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (s *CheckoutRecoverySpec) UnmarshalJSON(data []byte) error {
	type plain CheckoutRecoverySpec
	return checkpointStrictDecode(data, (*plain)(s), true)
}

type CheckoutRecoveryPublication struct {
	StagedCheckout      string `json:"staged_checkout"`
	DestinationCheckout string `json:"destination_checkout"`
}

// UnmarshalJSON refuses unknown fields (serde deny_unknown_fields).
func (p *CheckoutRecoveryPublication) UnmarshalJSON(data []byte) error {
	type plain CheckoutRecoveryPublication
	return checkpointStrictDecode(data, (*plain)(p), true)
}

// ParentRecoveryOutcome: target-side partial recovery never writes checkout
// files, refs or child reports.
type ParentRecoveryOutcome struct {
	NativeRestored bool `json:"native_restored"`
	RelayRestored  bool `json:"relay_restored"`
}

// UnmarshalJSON requires both fields and accepts unknown ones, as serde does
// for this struct.
func (o *ParentRecoveryOutcome) UnmarshalJSON(data []byte) error {
	type plain ParentRecoveryOutcome
	return checkpointStrictDecode(data, (*plain)(o), false)
}

func RestoreMissingParentCheckpoint(spec *CheckpointRestoreSpec, nativeState NativeCheckpointState, currentSeed *core.RestoredRelaySeed) (ParentRecoveryOutcome, error) {
	if spec.RestoreRepositories {
		return ParentRecoveryOutcome{}, errors.New("parent recovery cannot restore repositories")
	}
	if !checkpointIsDir(spec.WorkspaceRoot) {
		return ParentRecoveryOutcome{}, errors.New("current workspace is missing")
	}
	// Runtime reconstruction needs no archive when native history survives
	// and the controller supplies the current durable queue/frontier.
	if !spec.RestoreNative && currentSeed != nil {
		restored, err := restoreMissingParentRelay(spec.RelayRoot, currentSeed)
		if err != nil {
			return ParentRecoveryOutcome{}, err
		}
		return ParentRecoveryOutcome{RelayRestored: restored}, nil
	}
	archive, err := readArchiveForRestore(spec.ArchivePath)
	if err != nil {
		return ParentRecoveryOutcome{}, err
	}
	payloadReader, err := CheckpointPayloadReaderOpen(spec.ArchivePath)
	if err != nil {
		return ParentRecoveryOutcome{}, err
	}
	defer payloadReader.Close()
	canonical, err := archive.CanonicalSession()
	if err != nil {
		return ParentRecoveryOutcome{}, err
	}
	var outcome ParentRecoveryOutcome
	if spec.RestoreNative {
		if outcome.NativeRestored, err = restoreNativeArtifacts(&archive, payloadReader, spec, nativeState, true); err != nil {
			return ParentRecoveryOutcome{}, err
		}
	}
	// A current journal always wins over the archive.
	missing, err := parentRelayMissing(spec.RelayRoot)
	if err != nil {
		return ParentRecoveryOutcome{}, err
	}
	if missing {
		seed := currentSeed
		if seed == nil {
			archived, err := RelaySeedForRestore(&canonical, true, spec.QueuePolicy)
			if err != nil {
				return ParentRecoveryOutcome{}, err
			}
			seed = &archived
		}
		images := core.AttachmentStoreWorker(spec.RelayRoot)
		if err := restoreArchivedAttachments(&archive, &images); err != nil {
			return ParentRecoveryOutcome{}, err
		}
		if outcome.RelayRestored, err = restoreMissingParentRelay(spec.RelayRoot, seed); err != nil {
			return ParentRecoveryOutcome{}, err
		}
	}
	return outcome, nil
}

func parentRelayMissing(root string) (bool, error) {
	state, err := checkpointTryExists(core.Join(root, core.RelayStateFile))
	if err != nil || state {
		return false, err
	}
	journal, err := checkpointTryExists(core.Join(root, core.RelayJournalDir))
	if err != nil {
		return false, err
	}
	return !journal, nil
}

func restoreMissingParentRelay(root string, seed *core.RestoredRelaySeed) (bool, error) {
	missing, err := parentRelayMissing(root)
	if err != nil || !missing {
		return false, err
	}
	relative := core.RestoredRelaySeedFile
	if err := ensureNoSymlinkAncestors(root, relative); err != nil {
		return false, err
	}
	if err := os.MkdirAll(root, 0o777); err != nil {
		return false, err
	}
	destination := core.Join(root, relative)
	switch info, err := os.Lstat(destination); {
	case err == nil:
		if !info.Mode().IsRegular() || checkpointIsSymlink(info) {
			return false, errors.New("invalid parent relay seed destination")
		}
	case errors.Is(err, fs.ErrNotExist):
	default:
		return false, err
	}
	// A seed is bootstrap input, not the durable journal. Once that journal
	// is lost, only the chosen current projection can authorize its queue.
	body, err := checkpointMarshal(seed)
	if err != nil {
		return false, err
	}
	temporary, err := os.CreateTemp(root, ".tmp*")
	if err != nil {
		return false, err
	}
	persisted := false
	defer func() {
		if !persisted {
			temporary.Close()
			os.Remove(temporary.Name())
		}
	}()
	if _, err := temporary.Write(body); err != nil {
		return false, err
	}
	if err := temporary.Sync(); err != nil {
		return false, err
	}
	if err := temporary.Close(); err != nil {
		return false, err
	}
	if err := os.Rename(temporary.Name(), destination); err != nil {
		return false, err
	}
	persisted = true
	if runtime.GOOS != "windows" {
		directory, err := os.Open(root)
		if err != nil {
			return false, err
		}
		err = directory.Sync()
		directory.Close()
		if err != nil {
			return false, err
		}
	}
	return true, nil
}

// restoreMissingSqliteFamilies collapses a legacy database/WAL family before
// publishing any authoritative file. A retry can then treat a present
// database as complete, with no receipt or partially published sidecars to
// reconcile. It returns whether anything changed and the family paths it
// owns (a Rust BTreeSet<PathBuf>, keyed here by normalized path).
func restoreMissingSqliteFamilies(archive *VerifiedArchive, payloadReader *CheckpointPayloadReader, spec *CheckpointRestoreSpec, targetCwd *string) (bool, map[string]struct{}, error) {
	changed := false
	families := map[string]struct{}{}
	harness := archive.Manifest.Session.HarnessKind
	for i := range archive.Manifest.Payloads {
		database := &archive.Manifest.Payloads[i]
		if database.Role.Kind != core.PayloadRoleNativeArtifact {
			continue
		}
		relativePath := database.Role.RelativePath
		if !isSqliteNativeDatabase(harness, relativePath) {
			continue
		}
		sidecars := sqliteSidecars(relativePath)
		families[checkpointNormalize(relativePath)] = struct{}{}
		for _, sidecar := range sidecars {
			families[checkpointNormalize(sidecar)] = struct{}{}
		}
		present, err := checkpointTryExists(core.Join(spec.HarnessHome, relativePath))
		if err != nil {
			return false, nil, err
		}
		if present {
			continue
		}
		written, err := recoverMissingSqliteFamily(archive, payloadReader, spec, targetCwd, database, sidecars)
		if err != nil {
			return false, nil, err
		}
		changed = changed || written
	}
	return changed, families, nil
}

// recoverMissingSqliteFamily is one absent database of
// restoreMissingSqliteFamilies: its archived database and sidecars are
// collapsed into one snapshot in a scratch directory, which is then
// published if the database is still absent.
func recoverMissingSqliteFamily(archive *VerifiedArchive, payloadReader *CheckpointPayloadReader, spec *CheckpointRestoreSpec, targetCwd *string, database *core.PayloadDescriptor, sidecars [2]string) (bool, error) {
	harness := archive.Manifest.Session.HarnessKind
	relativePath := database.Role.RelativePath
	for _, sidecar := range sidecars {
		present, err := checkpointTryExists(core.Join(spec.HarnessHome, sidecar))
		if err != nil {
			return false, err
		}
		if present {
			return false, errors.New("native database is absent but its sidecar remains; retaining both for recovery")
		}
	}
	scratch, err := os.MkdirTemp("", ".tmp*")
	if err != nil {
		return false, err
	}
	defer os.RemoveAll(scratch)
	copyPayload := func(descriptor *core.PayloadDescriptor) func(*os.File) (struct{}, error) {
		return func(file *os.File) (struct{}, error) {
			return struct{}{}, payloadReader.CopyPayload(descriptor, file)
		}
	}
	if _, err := writePrivateWith(scratch, relativePath, database.Mode, copyPayload(database)); err != nil {
		return false, err
	}
	for _, sidecar := range sidecars {
		for i := range archive.Manifest.Payloads {
			payload := &archive.Manifest.Payloads[i]
			if payload.Role.Kind == core.PayloadRoleNativeArtifact && checkpointPathEqual(payload.Role.RelativePath, sidecar) {
				if _, err := writePrivateWith(scratch, sidecar, payload.Mode, copyPayload(payload)); err != nil {
					return false, err
				}
				break
			}
		}
	}
	// The collapsed copy stays a file; only a relocating provider needs its
	// bytes in memory to rewrite them.
	snapshot, err := sqliteSnapshotFile(core.Join(scratch, relativePath), "recovered native", func(*sql.Conn) error { return nil })
	if err != nil {
		return false, err
	}
	defer removeSqliteSnapshotFile(snapshot)
	if _, relocates := nativeArtifactRelocation(harness, relativePath); targetCwd == nil || !relocates {
		return writeMissingPrivateWith(spec.HarnessHome, relativePath, database.Mode, func(file *os.File) error {
			source, err := os.Open(snapshot.Name())
			if err != nil {
				return err
			}
			defer source.Close()
			_, err = io.Copy(file, source)
			return err
		})
	}
	data, err := os.ReadFile(snapshot.Name())
	if err != nil {
		return false, err
	}
	complete, err := restoredNativeArtifactBytes(harness, relativePath, data, targetCwd, spec.HarnessHome)
	if err != nil {
		return false, err
	}
	return writeMissingPrivateFile(spec.HarnessHome, relativePath, complete, database.Mode)
}

func restoreNativeArtifacts(archive *VerifiedArchive, payloadReader *CheckpointPayloadReader, spec *CheckpointRestoreSpec, nativeState NativeCheckpointState, missingOnly bool) (bool, error) {
	targetCwd := targetPrimaryCwd(archive.Manifest.Bundle.PrimaryRepository, archive.Manifest.Repositories, spec.WorkspaceRoot)
	if spec.PrimaryRepositoryRoot != nil {
		root := *spec.PrimaryRepositoryRoot
		targetCwd = &root
	}
	harness := archive.Manifest.Session.HarnessKind
	changed := false
	sqliteFamilies := map[string]struct{}{}
	if missingOnly {
		var err error
		if changed, sqliteFamilies, err = restoreMissingSqliteFamilies(archive, payloadReader, spec, targetCwd); err != nil {
			return false, err
		}
	}
	// A restored database replaces the one in the home, and SQLite would
	// replay a write-ahead log left beside the old one over it. An archive
	// from before OpenCode snapshots carries its own log, which the loop
	// below writes after this.
	for i := range archive.Manifest.Payloads {
		descriptor := &archive.Manifest.Payloads[i]
		if descriptor.Role.Kind != core.PayloadRoleNativeArtifact || !isSqliteNativeDatabase(harness, descriptor.Role.RelativePath) {
			continue
		}
		relativePath := descriptor.Role.RelativePath
		for _, sidecar := range sqliteSidecars(relativePath) {
			if !missingOnly {
				if err := removePrivateFile(spec.HarnessHome, sidecar); err != nil {
					return false, err
				}
				continue
			}
			sidecarPresent, err := checkpointTryExists(core.Join(spec.HarnessHome, sidecar))
			if err != nil {
				return false, err
			}
			if !sidecarPresent {
				continue
			}
			databasePresent, err := checkpointTryExists(core.Join(spec.HarnessHome, relativePath))
			if err != nil {
				return false, err
			}
			if !databasePresent {
				return false, errors.New("native database is absent but its sidecar remains; retaining both for recovery")
			}
		}
	}
	for i := range archive.Manifest.Payloads {
		descriptor := &archive.Manifest.Payloads[i]
		if descriptor.Role.Kind != core.PayloadRoleNativeArtifact {
			continue
		}
		relativePath := descriptor.Role.RelativePath
		if checkpointPathStartsWith(relativePath, core.ArchiveAttachmentDir) ||
			checkpointPathStartsWith(relativePath, core.ArchiveReportDir) {
			continue
		}
		if _, family := sqliteFamilies[checkpointNormalize(relativePath)]; missingOnly && family {
			continue
		}
		copyPayload := func(file *os.File) error { return payloadReader.CopyPayload(descriptor, file) }
		if !checkpointPathStartsWith(relativePath, core.NativeArtifactRoot) {
			destination, err := restoredNativeRelativePath(harness, relativePath, targetCwd)
			if err != nil {
				return false, err
			}
			// Provider relocation rewrites Kimi/Grok/Muse/DSH records and
			// agy's SQLite database. Claude and Codex transcripts only remap
			// paths.
			if _, relocates := nativeArtifactRelocation(harness, destination); targetCwd == nil || !relocates {
				if missingOnly {
					written, err := writeMissingPrivateWith(spec.HarnessHome, destination, descriptor.Mode, copyPayload)
					if err != nil {
						return false, err
					}
					changed = changed || written
				} else if _, err := writePrivateWith(spec.HarnessHome, destination, descriptor.Mode, func(file *os.File) (struct{}, error) {
					return struct{}{}, copyPayload(file)
				}); err != nil {
					return false, err
				}
				continue
			}
		}
		nativeData, ok := archive.Payloads[descriptor.Path]
		if !ok {
			var loaded bytes.Buffer
			if err := payloadReader.CopyPayload(descriptor, &loaded); err != nil {
				return false, err
			}
			nativeData = loaded.Bytes()
		}
		if missingOnly {
			written, handled, err := nativeState.RestoreMissing(&archive.Manifest.Session, spec.HarnessHome, relativePath, nativeData)
			if err != nil {
				return false, err
			}
			if handled {
				changed = changed || written
				continue
			}
		} else {
			restored, err := nativeState.Restore(&archive.Manifest.Session, spec.HarnessHome, relativePath, nativeData)
			if err != nil {
				return false, err
			}
			if restored {
				continue
			}
		}
		if checkpointPathStartsWith(relativePath, core.NativeArtifactRoot) {
			return false, errors.New("this checkpoint requires native goal restoration by the target worker")
		}
		relocated, err := restoredNativeRelativePath(harness, relativePath, targetCwd)
		if err != nil {
			return false, err
		}
		if nativeData, err = restoredNativeArtifactBytes(harness, relocated, nativeData, targetCwd, spec.HarnessHome); err != nil {
			return false, err
		}
		if err := validateRelativePath(relocated); err != nil {
			return false, err
		}
		if missingOnly {
			written, err := writeMissingPrivateFile(spec.HarnessHome, relocated, nativeData, descriptor.Mode)
			if err != nil {
				return false, err
			}
			changed = changed || written
		} else if err := writePrivateFile(spec.HarnessHome, relocated, nativeData, descriptor.Mode); err != nil {
			return false, err
		}
	}
	return changed, nil
}

// ReadCheckpointSession reads the controller-owned projection without
// restoring target artifacts.
func ReadCheckpointSession(path string) (core.CanonicalSessionSnapshot, error) {
	metadata, err := VerifyArchiveStreaming(path)
	if err != nil {
		return core.CanonicalSessionSnapshot{}, err
	}
	return metadata.CanonicalSession, nil
}

func RestoreRepositories(archivePath string, workspaceRoot string, git GitCommandRunner) error {
	if !checkpointIsDir(workspaceRoot) {
		return errors.New("restore workspace is missing")
	}
	archive, err := readArchiveRepositories(archivePath)
	if err != nil {
		return err
	}
	_, err = restoreRepositoriesFromArchive(archivePath, &archive, workspaceRoot, nil, git, true)
	return err
}

func restoreRepositoriesFromArchive(
	archivePath string,
	archive *VerifiedArchive,
	workspaceRoot string,
	primaryRepositoryRoot *string,
	git GitCommandRunner,
	restoreContents bool,
) ([]string, error) {
	allNotices := []string{}
	for i := range archive.Manifest.Repositories {
		repository := &archive.Manifest.Repositories[i]
		id := repository.Metadata.ID
		inRepository := func(err error) error {
			return fmt.Errorf("restore repository %s: %w", checkpointDebugString(id), err)
		}
		if delta := repository.Metadata.OriginDelta; delta != nil {
			if !restoreContents {
				continue
			}
			notices, err := restoreOriginDeltaRepository(archivePath, archive, repository, delta, workspaceRoot,
				primaryRepositoryRoot, git)
			if err != nil {
				return nil, err
			}
			allNotices = append(allNotices, notices...)
			continue
		}
		// A plain project folder is the user's own directory on a bare local
		// target. Its files were never captured, so a restore must never
		// check out, rewrite or remove anything in it.
		if repository.Metadata.PlainDirectory {
			continue
		}
		if !restoreContents && !savesMjolnirRefs(repository.Metadata.SavedRefs) {
			continue
		}
		snapshot, err := archivedRepositorySnapshot(archive, repository)
		if err != nil {
			return nil, err
		}
		path, err := checkoutTopLevel(core.Join(workspaceRoot, repository.Metadata.RelativeDestination), &repository.Metadata)
		if err != nil {
			return nil, inRepository(err)
		}
		session := archive.Manifest.Session.ID
		notices, err := restoreGitSnapshotForSession(git, path, &snapshot, &session, restoreContents)
		if err != nil {
			return nil, inRepository(err)
		}
		for _, notice := range notices {
			slog.Info(notice, "repository_id", id)
		}
		allNotices = append(allNotices, notices...)
	}
	return allNotices, nil
}

// restoreOriginDeltaRepository restores one origin-delta repository from
// its snapshot metadata and its pack, streamed out of the archive into a
// temporary file that is removed afterwards.
func restoreOriginDeltaRepository(
	archivePath string,
	archive *VerifiedArchive,
	repository *core.RepositoryManifest,
	delta *core.WorkspaceSnapshotMetadata,
	workspaceRoot string,
	primaryRepositoryRoot *string,
	git GitCommandRunner,
) ([]string, error) {
	id := repository.Metadata.ID
	snapshot := WorkspaceSnapshot{Metadata: *delta}
	if packPath := repository.OriginDeltaPackPath; packPath != nil {
		role := core.PayloadRole{Kind: core.PayloadRoleGitOriginDeltaPack, RepositoryID: id}
		var descriptor *core.PayloadDescriptor
		for index := range archive.Manifest.Payloads {
			payload := &archive.Manifest.Payloads[index]
			if payload.Path == *packPath && payload.Role == role {
				descriptor = payload
				break
			}
		}
		if descriptor == nil {
			return nil, errors.New("origin delta pack descriptor is missing")
		}
		temporary, err := os.CreateTemp("", ".tmp")
		if err != nil {
			return nil, fmt.Errorf("create temporary origin delta pack file: %w", err)
		}
		defer func() {
			temporary.Close()
			os.Remove(temporary.Name())
		}()
		reader, err := CheckpointPayloadReaderOpen(archivePath)
		if err != nil {
			return nil, err
		}
		err = reader.CopyPayload(descriptor, temporary)
		reader.Close()
		if err != nil {
			return nil, fmt.Errorf("stream origin delta pack payload: %w", err)
		}
		if err := temporary.Sync(); err != nil {
			return nil, fmt.Errorf("flush temporary origin delta pack file: %w", err)
		}
		pack := temporary.Name()
		snapshot.Pack = &pack
	}
	inRepository := func(err error) error {
		return fmt.Errorf("restore repository %s: %w", checkpointDebugString(id), err)
	}
	var path string
	if primary := primaryRepositoryRoot; id == archive.Manifest.Bundle.PrimaryRepository && primary != nil {
		subdirectory := repository.Metadata.CheckoutSubdirectory
		if subdirectory != nil && checkpointPathEndsWith(*primary, *subdirectory) {
			topLevel, err := checkoutTopLevel(*primary, &repository.Metadata)
			if err != nil {
				return nil, inRepository(err)
			}
			path = topLevel
		} else {
			path = *primary
		}
	} else {
		topLevel, err := checkoutTopLevel(core.Join(workspaceRoot, repository.Metadata.RelativeDestination),
			&repository.Metadata)
		if err != nil {
			return nil, inRepository(err)
		}
		path = topLevel
	}
	notices, err := RestoreWorkspaceSnapshot(git, snapshot, path)
	if err != nil {
		return nil, inRepository(err)
	}
	for _, notice := range notices {
		slog.Info(notice, "repository_id", id)
	}
	if err := RepairWorkspaceSnapshot(git, snapshot, path); err != nil {
		return nil, fmt.Errorf("repair repository %s: %w", checkpointDebugString(id), err)
	}
	return notices, nil
}

// savesMjolnirRefs is whether any saved ref is one of Mjolnir's private
// refs, which a contents-free restore still has to publish.
func savesMjolnirRefs(refs map[string]string) bool {
	for name := range refs {
		if strings.HasPrefix(name, core.MjolnirRefNamespace) {
			return true
		}
	}
	return false
}

// checkoutTopLevel is the top level of the checkout a repository's session
// directory belongs to.
//
// A session working in a subdirectory of its checkout is described by that
// subdirectory, while its snapshot covers the whole checkout, so the restore
// climbs back out of the subdirectory the snapshot recorded.
func checkoutTopLevel(directory string, metadata *core.RepositoryMetadata) (string, error) {
	if metadata.CheckoutSubdirectory == nil {
		return directory, nil
	}
	subdirectory := *metadata.CheckoutSubdirectory
	if !checkpointPathEndsWith(directory, subdirectory) {
		return "", fmt.Errorf("%s is not the checkout subdirectory %s the checkpoint was taken from", directory, subdirectory)
	}
	topLevel := directory
	for range checkpointComponents(subdirectory) {
		parent, ok := checkpointParent(topLevel)
		if !ok {
			return "", fmt.Errorf("%s has no checkout above it", directory)
		}
		topLevel = parent
	}
	return topLevel, nil
}

func archivedRepositorySnapshot(archive *VerifiedArchive, repository *core.RepositoryManifest) (RepositorySnapshot, error) {
	id := repository.Metadata.ID
	if repository.Metadata.OriginDelta != nil {
		return RepositorySnapshot{}, errors.New("origin delta requires managed clone provisioning")
	}
	payload := func(role core.PayloadRole) ([]byte, error) {
		data, err := archive.PayloadByRole(&role)
		if err != nil {
			return nil, err
		}
		return slices.Clone(data), nil
	}
	repositoryRole := func(kind core.PayloadRoleKind) core.PayloadRole {
		return core.PayloadRole{Kind: kind, RepositoryID: id}
	}
	worktreeRole := func(kind core.PayloadRoleKind, worktree uint32) core.PayloadRole {
		return core.PayloadRole{Kind: kind, RepositoryID: id, Worktree: worktree}
	}
	snapshot := RepositorySnapshot{Metadata: repository.Metadata}
	var err error
	if snapshot.CommittedBundle, err = payload(repositoryRole(core.PayloadRoleGitBundle)); err != nil {
		return RepositorySnapshot{}, err
	}
	if snapshot.StagedPatch, err = payload(repositoryRole(core.PayloadRoleGitStagedPatch)); err != nil {
		return RepositorySnapshot{}, err
	}
	if snapshot.UnstagedPatch, err = payload(repositoryRole(core.PayloadRoleGitUnstagedPatch)); err != nil {
		return RepositorySnapshot{}, err
	}
	if snapshot.UntrackedTar, err = payload(repositoryRole(core.PayloadRoleGitUntrackedTar)); err != nil {
		return RepositorySnapshot{}, err
	}
	if repository.Metadata.GitOperation != nil {
		var operation GitOperationPayloads
		if operation.ConflictedWorktreeTar, err = payload(repositoryRole(core.PayloadRoleGitConflictedWorktreeTar)); err != nil {
			return RepositorySnapshot{}, err
		}
		if operation.StateTar, err = payload(repositoryRole(core.PayloadRoleGitOperationStateTar)); err != nil {
			return RepositorySnapshot{}, err
		}
		snapshot.OperationPayloads = &operation
	}
	snapshot.LinkedWorktrees = make([]LinkedWorktreeSnapshot, 0, len(repository.LinkedWorktrees))
	for index := range repository.LinkedWorktrees {
		worktree := &repository.LinkedWorktrees[index]
		worktreeIndex := uint32(index)
		linked := LinkedWorktreeSnapshot{Metadata: worktree.Metadata}
		if linked.StagedPatch, err = payload(worktreeRole(core.PayloadRoleGitLinkedWorktreeStagedPatch, worktreeIndex)); err != nil {
			return RepositorySnapshot{}, err
		}
		if linked.UnstagedPatch, err = payload(worktreeRole(core.PayloadRoleGitLinkedWorktreeUnstagedPatch, worktreeIndex)); err != nil {
			return RepositorySnapshot{}, err
		}
		if linked.UntrackedTar, err = payload(worktreeRole(core.PayloadRoleGitLinkedWorktreeUntrackedTar, worktreeIndex)); err != nil {
			return RepositorySnapshot{}, err
		}
		if worktree.Metadata.GitOperation != nil {
			var operation GitOperationPayloads
			if operation.ConflictedWorktreeTar, err = payload(worktreeRole(core.PayloadRoleGitLinkedWorktreeConflictedWorktreeTar, worktreeIndex)); err != nil {
				return RepositorySnapshot{}, err
			}
			if operation.StateTar, err = payload(worktreeRole(core.PayloadRoleGitLinkedWorktreeOperationStateTar, worktreeIndex)); err != nil {
				return RepositorySnapshot{}, err
			}
			linked.OperationPayloads = &operation
		}
		snapshot.LinkedWorktrees = append(snapshot.LinkedWorktrees, linked)
	}
	return snapshot, nil
}

// RestoreSingleRepositoryOntoBranch restores a checkpoint's only repository
// into an existing checkout, on a branch the caller names.
//
// A resume that moves a session out of its workspace restores it into a
// worktree of the user's own repository, where the archived branch is
// usually already checked out somewhere else and `git checkout -B` would
// refuse it. Returns the resume notices of the restore.
func RestoreSingleRepositoryOntoBranch(archivePath string, repositoryPath string, branch string, git GitCommandRunner) ([]string, error) {
	if !checkpointIsDir(repositoryPath) {
		return nil, errors.New("restore checkout is missing")
	}
	archive, err := readArchiveRepositories(archivePath)
	if err != nil {
		return nil, err
	}
	if len(archive.Manifest.Repositories) != 1 {
		return nil, fmt.Errorf("this checkpoint holds %d repositories; exactly one can be restored into a checkout", len(archive.Manifest.Repositories))
	}
	repository := &archive.Manifest.Repositories[0]
	if repository.Metadata.PlainDirectory {
		return nil, errors.New("this session's project is a directory without a committed Git checkout, so it cannot be restored into one")
	}
	snapshot, err := archivedRepositorySnapshot(&archive, repository)
	if err != nil {
		return nil, err
	}
	snapshot.Metadata.Branch = &branch
	// This explicit host restore uses a linked worktree whose local Git
	// configuration is shared with the user's main checkout; keep managed
	// network configuration out of that shared config.
	snapshot.Metadata.RemoteWorkspace = false
	session := archive.Manifest.Session.ID
	notices, err := restoreGitSnapshotForSession(git, repositoryPath, &snapshot, &session, true)
	if err != nil {
		return nil, fmt.Errorf("restore repository %s: %w", checkpointDebugString(repository.Metadata.ID), err)
	}
	return notices, nil
}

// RestoreSingleRepositoryIntoCheckout restores a retired independent clone
// on the branch saved in its archive.
func RestoreSingleRepositoryIntoCheckout(archivePath string, repositoryPath string, git GitCommandRunner) ([]string, error) {
	if !checkpointIsDir(repositoryPath) {
		return nil, errors.New("restore checkout is missing")
	}
	archive, err := readArchiveRepositories(archivePath)
	if err != nil {
		return nil, err
	}
	if len(archive.Manifest.Repositories) != 1 {
		return nil, errors.New("an isolated checkout requires exactly one archived repository")
	}
	repository := &archive.Manifest.Repositories[0]
	if repository.Metadata.PlainDirectory {
		return nil, errors.New("this session's project is a directory without a committed Git checkout, so it cannot be restored into one")
	}
	snapshot, err := archivedRepositorySnapshot(&archive, repository)
	if err != nil {
		return nil, err
	}
	snapshot.Metadata.RemoteWorkspace = false
	session := archive.Manifest.Session.ID
	notices, err := restoreGitSnapshotForSession(git, repositoryPath, &snapshot, &session, true)
	if err != nil {
		return nil, fmt.Errorf("restore independent checkout before moving it into a target: %w", err)
	}
	return notices, nil
}

// RestoreSingleRepositoryIntoClone restores a legacy single-repository
// archive into a fresh managed clone. It checks out the saved commit on the
// saved branch name, or as a detached HEAD (with a resume notice) if that
// branch name already exists pointing at a different commit.
func RestoreSingleRepositoryIntoClone(archivePath string, repositoryPath string, git GitCommandRunner) ([]string, error) {
	if !checkpointIsDir(repositoryPath) {
		return nil, errors.New("restore checkout is missing")
	}
	archive, err := readArchiveRepositories(archivePath)
	if err != nil {
		return nil, err
	}
	if len(archive.Manifest.Repositories) != 1 {
		return nil, errors.New("an isolated clone requires exactly one archived repository")
	}
	repository := &archive.Manifest.Repositories[0]
	if repository.Metadata.PlainDirectory {
		return nil, errors.New("this session's project is a directory without a committed Git checkout, so it cannot be restored into one")
	}
	snapshot, err := archivedRepositorySnapshot(&archive, repository)
	if err != nil {
		return nil, err
	}
	snapshot.Metadata.RemoteWorkspace = false
	session := archive.Manifest.Session.ID
	notices, err := restoreGitSnapshotForSessionWithOptions(git, repositoryPath, &snapshot, &session, true,
		LegacyRestoreOptions{DetachOnBranchCollision: true})
	if err != nil {
		return nil, fmt.Errorf("restore legacy archive into fresh managed clone: %w", err)
	}
	return notices, nil
}

// restoredNativeRelativePath: native session files use harness-specific
// working-directory keys. Rewrite the archived key to the one the harness
// derives from targetCwd, the directory the worker launches it in.
func restoredNativeRelativePath(harness core.HarnessKind, relativePath string, targetCwd *string) (string, error) {
	if targetCwd == nil {
		return relativePath, nil
	}
	cwd := harnessStorageCwd(harness, *targetCwd)
	var root, key string
	switch harness {
	case core.HarnessKindClaude:
		root, key = "projects", ClaudeProjectSlug(cwd)
	case core.HarnessKindKimi:
		root, key = "sessions", kimiWorkspaceKey(cwd)
	case core.HarnessKindGrok:
		root, key = "sessions", grokCwdKey(cwd)
	default:
		// OpenCode's database records session directories itself, and
		// dsh-acp records its directory inside the session file; neither has
		// a per-workspace path key to rewrite.
		return relativePath, nil
	}
	components := checkpointComponents(relativePath)
	if len(components) < 2 || components[0] != (checkpointComponent{checkpointComponentNormal, root}) {
		return relativePath, nil
	}
	rewritten := core.Join(root, key)
	for _, component := range components[2:] {
		rewritten = core.Join(rewritten, component.text)
	}
	return rewritten, nil
}

func targetPrimaryCwd(primaryRepository string, repositories []core.RepositoryManifest, workspaceRoot string) *string {
	for i := range repositories {
		if repositories[i].Metadata.ID == primaryRepository {
			cwd := core.Join(workspaceRoot, repositories[i].Metadata.RelativeDestination)
			return &cwd
		}
	}
	return nil
}

type nativeRelocation int

const (
	nativeRelocationMuse nativeRelocation = iota
	nativeRelocationDsh
	nativeRelocationAntigravity
	nativeRelocationGrokSummary
	nativeRelocationKimiRegistry
	nativeRelocationKimiIndex
	nativeRelocationKimiState
)

// nativeArtifactRelocation is the single owner of which native formats
// actually rewrite their contents; false when the format does not.
func nativeArtifactRelocation(harness core.HarnessKind, path string) (nativeRelocation, bool) {
	switch harness {
	case core.HarnessKindMuse:
		if name, ok := checkpointFileName(path); ok && name == "session.jsonl" {
			return nativeRelocationMuse, true
		}
	case core.HarnessKindDsh:
		if checkpointPathStartsWith(path, "sessions") {
			return nativeRelocationDsh, true
		}
	case core.HarnessKindAntigravity:
		if isSqliteNativeDatabase(harness, path) {
			return nativeRelocationAntigravity, true
		}
	case core.HarnessKindGrok:
		if isGrokSessionSummary(path) {
			return nativeRelocationGrokSummary, true
		}
	case core.HarnessKindKimi:
		switch {
		case checkpointPathEqual(path, "workspaces.json"):
			return nativeRelocationKimiRegistry, true
		case checkpointPathEqual(path, "session_index.jsonl"):
			return nativeRelocationKimiIndex, true
		case isKimiSessionState(path):
			return nativeRelocationKimiState, true
		}
	}
	return 0, false
}

func restoredNativeArtifactBytes(harness core.HarnessKind, relativePath string, data []byte, targetCwd *string, harnessHome string) ([]byte, error) {
	if targetCwd == nil {
		return slices.Clone(data), nil
	}
	cwd := harnessStorageCwd(harness, *targetCwd)
	relocation, ok := nativeArtifactRelocation(harness, relativePath)
	if !ok {
		return slices.Clone(data), nil
	}
	switch relocation {
	case nativeRelocationMuse:
		return MuseRelocate(data, cwd)
	case nativeRelocationDsh:
		return rewriteDshSessionCwd(data, cwd)
	case nativeRelocationAntigravity:
		relocated, err := Relocate(data, cwd)
		if err != nil {
			return nil, fmt.Errorf("relocate Antigravity conversation %s to %s: %w", relativePath, cwd, err)
		}
		return relocated, nil
	case nativeRelocationGrokSummary:
		return rewriteGrokSessionSummary(data, cwd, harnessHome)
	case nativeRelocationKimiRegistry:
		return rewriteKimiWorkspaceRegistry(data, cwd)
	case nativeRelocationKimiIndex:
		return rewriteKimiSessionIndex(data, cwd, harnessHome)
	}
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return nil, fmt.Errorf("parse Kimi native session state: %w", err)
	}
	state, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, errors.New("Kimi native session state is not a JSON object")
	}
	for _, key := range []string{"workDir", "cwd"} {
		if _, ok := state.Get(key); ok {
			state.Set(key, checkpointLossy([]byte(cwd)))
		}
	}
	return jsonvalue.Marshal(state)
}

// rewriteDshSessionCwd: a dsh-acp session record names the directory it was
// created in, and `session/load` refuses any other, so a restore at a new
// workspace path records the target's. Additional directories need no
// rewrite: every load replaces them with the ones it is given.
func rewriteDshSessionCwd(data []byte, targetCwd string) ([]byte, error) {
	value, err := jsonvalue.Parse(data)
	if err != nil {
		return nil, fmt.Errorf("parse dsh-acp session record: %w", err)
	}
	record, ok := value.(jsonvalue.Object)
	if !ok {
		return nil, errors.New("dsh-acp session record is not a JSON object")
	}
	if _, ok := record.Get("cwd"); !ok {
		return nil, errors.New("dsh-acp session record names no cwd")
	}
	if !utf8.ValidString(targetCwd) {
		return nil, errors.New("dsh-acp target cwd is not valid UTF-8")
	}
	record.Set("cwd", targetCwd)
	return jsonvalue.Marshal(record)
}

// restoreSubagentReports puts a checkpoint's sub-agent report files back
// beside the repositories, whether or not the harness's own history is
// restored: the parent reads them with its own tools. A file already there
// is newer than the archive's copy and is kept.
func restoreSubagentReports(archive *VerifiedArchive, workspaceRoot string, reportRoot string) error {
	root, ok := checkpointStripPrefix(reportRoot, workspaceRoot)
	if !ok {
		return fmt.Errorf("report restore root is outside the workspace: %w", errCheckpointPrefixNotFound)
	}
	for i := range archive.Manifest.Payloads {
		descriptor := &archive.Manifest.Payloads[i]
		if descriptor.Role.Kind != core.PayloadRoleNativeArtifact {
			continue
		}
		report, ok := checkpointStripPrefix(descriptor.Role.RelativePath, core.ArchiveReportDir)
		if !ok {
			continue
		}
		relative := core.Join(root, report)
		if err := validateRelativePath(relative); err != nil {
			return err
		}
		if err := ensureNoSymlinkAncestors(workspaceRoot, relative); err != nil {
			return err
		}
		if _, err := os.Lstat(core.Join(workspaceRoot, relative)); err == nil {
			continue
		}
		payload, err := archive.Payload(descriptor)
		if err != nil {
			return err
		}
		if err := writePrivateFile(workspaceRoot, relative, payload, 0o600); err != nil {
			return err
		}
	}
	return nil
}

// removePrivateFile removes relative from root if it is there, under the
// same rules as writePrivateFile: never through a symlinked directory.
func removePrivateFile(root string, relative string) error {
	if err := validateRelativePath(relative); err != nil {
		return err
	}
	if err := ensureNoSymlinkAncestors(root, relative); err != nil {
		return err
	}
	path := core.Join(root, relative)
	if err := os.Remove(path); err != nil && !errors.Is(err, fs.ErrNotExist) {
		return fmt.Errorf("remove %s: %w", path, err)
	}
	return nil
}

// writePrivateFile writes relative under root with owner-only permissions,
// refusing to follow a symlink anywhere along the way.
//
// A restore lands in a directory the target user can write, so a planted
// symlink must never redirect the write outside root. The destination is
// inspected without following links and, on Unix, opened with O_NOFOLLOW so
// the check cannot be raced.
func writePrivateFile(root string, relative string, data []byte, mode uint32) error {
	_, err := writePrivateReader(root, relative, bytes.NewReader(data), mode)
	return err
}

func writePrivateReader(root string, relative string, reader io.Reader, mode uint32) (uint64, error) {
	return writePrivateWith(root, relative, mode, func(file *os.File) (uint64, error) {
		written, err := io.Copy(file, reader)
		return uint64(written), err
	})
}

func writeMissingPrivateFile(root string, relative string, data []byte, mode uint32) (bool, error) {
	return writeMissingPrivateWith(root, relative, mode, func(file *os.File) error {
		if _, err := file.Write(data); err != nil {
			return fmt.Errorf("write staged recovery file: %w", err)
		}
		return nil
	})
}

// preparePrivateDestination checks the destination and creates its
// parents, returning the path to write. Both the replacing and the
// missing-only writers start here.
func preparePrivateDestination(root string, relative string) (string, error) {
	if err := validateRelativePath(relative); err != nil {
		return "", err
	}
	path := core.Join(root, relative)
	if err := ensureNoSymlinkAncestors(root, relative); err != nil {
		return "", err
	}
	if parent, ok := checkpointParent(path); ok {
		if err := os.MkdirAll(parent, 0o777); err != nil {
			return "", fmt.Errorf("create directory %s: %w", parent, err)
		}
		// MkdirAll walks through symlinked directories, so re-inspect the
		// ancestors it just materialized.
		if err := ensureNoSymlinkAncestors(root, relative); err != nil {
			return "", err
		}
	}
	switch info, err := os.Lstat(path); {
	case err == nil:
		if checkpointIsSymlink(info) {
			return "", fmt.Errorf("refusing to write through symlink %s", path)
		}
	case errors.Is(err, fs.ErrNotExist):
	default:
		return "", fmt.Errorf("inspect %s: %w", path, err)
	}
	return path, nil
}

// checkpointPrivatePerm is the permission a private file is created with:
// the owner's bits of mode on Unix. Elsewhere Rust sets no mode, so the file
// is created writable.
func checkpointPrivatePerm(mode uint32) fs.FileMode {
	if runtime.GOOS == "windows" {
		return 0o666
	}
	return fs.FileMode(mode & 0o700)
}

func writePrivateWith[T any](root string, relative string, mode uint32, write func(*os.File) (T, error)) (T, error) {
	var zero T
	path, err := preparePrivateDestination(root, relative)
	if err != nil {
		return zero, err
	}
	file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_TRUNC|checkpointNoFollow, checkpointPrivatePerm(mode))
	if err != nil {
		return zero, fmt.Errorf("write %s: %w", path, err)
	}
	result, err := write(file)
	closeErr := file.Close()
	if err == nil {
		err = closeErr
	}
	if err != nil {
		return zero, fmt.Errorf("write %s: %w", path, err)
	}
	if runtime.GOOS != "windows" {
		if err := os.Chmod(path, fs.FileMode(mode&0o700)); err != nil {
			return zero, err
		}
	}
	return result, nil
}

// writeMissingPrivateWith streams a file into a staged copy and publishes
// it only when the destination is still absent; returns whether it
// published.
func writeMissingPrivateWith(root string, relative string, mode uint32, write func(*os.File) error) (bool, error) {
	path, err := preparePrivateDestination(root, relative)
	if err != nil {
		return false, err
	}
	return publishMissingPrivateFile(path, mode, write)
}

func publishMissingPrivateFile(path string, mode uint32, write func(*os.File) error) (bool, error) {
	parent, ok := checkpointParent(path)
	if !ok {
		return false, errors.New("recovery file has no parent")
	}
	temporary, err := os.CreateTemp(parent, ".tmp*")
	if err != nil {
		return false, err
	}
	// Rust's NamedTempFile removes the staged copy unless it was published.
	defer func() {
		temporary.Close()
		os.Remove(temporary.Name())
	}()
	if err := write(temporary); err != nil {
		return false, err
	}
	if runtime.GOOS != "windows" {
		if err := temporary.Chmod(fs.FileMode(mode & 0o700)); err != nil {
			return false, err
		}
	}
	if err := temporary.Sync(); err != nil {
		return false, err
	}
	// Publishing links the staged copy under its name, which fails rather
	// than replacing a file another writer put there first (tempfile's
	// persist_noclobber).
	if err := os.Link(temporary.Name(), path); err != nil {
		if errors.Is(err, fs.ErrExist) {
			return false, nil
		}
		return false, fmt.Errorf("publish recovery file %s: %w", path, err)
	}
	return true, nil
}
