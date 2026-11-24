//go:build unix

// Port of mj-worker-staging/src/worker_binary/install.rs's
// `profile_archive_tests` module (`#[cfg(all(test, unix))]`).

package workerstaging

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"crypto/rand"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// systemTar runs each remote profile script locally with sh, in home, as an
// SSH host's shell would, and records what it was sent.
type systemTar struct {
	core.CommandExecutorDefaults
	home                   string
	uploads                [][]byte
	deleteBeforeUpload     int
	warnings               []string
	bin                    *string
	disconnectAfterReceive bool
	collectBeforePublish   int
}

func newSystemTar(home string) *systemTar { return &systemTar{home: home} }

func (s *systemTar) store() string { return filepath.Join(s.home, profileContentStore) }

// storedFiles is the store's content files, without its dot-named entries.
func (s *systemTar) storedFiles() []string {
	entries, err := os.ReadDir(s.store())
	if err != nil {
		panic(err)
	}
	var files []string
	for _, entry := range entries {
		if !strings.HasPrefix(entry.Name(), ".") {
			files = append(files, filepath.Join(s.store(), entry.Name()))
		}
	}
	return files
}

// stagingDirectories is the store's `.stage-*` entries.
func (s *systemTar) stagingDirectories() ([]string, error) {
	entries, err := os.ReadDir(s.store())
	if err != nil {
		return nil, err
	}
	var paths []string
	for _, entry := range entries {
		if strings.HasPrefix(entry.Name(), ".stage-") {
			paths = append(paths, filepath.Join(s.store(), entry.Name()))
		}
	}
	return paths, nil
}

func (s *systemTar) PauseDeadline() *core.DeadlinePause         { return core.DeadlinePauseNone() }
func (s *systemTar) CancellationRequestedWithoutDeadline() bool { return s.CancellationRequested() }
func (s *systemTar) ExecuteCleanup(c *core.CommandSpec) (core.CommandOutput, error) {
	return s.Execute(c)
}
func (s *systemTar) ExecuteCleanupWithTimeout(c *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return s.Execute(c)
}
func (s *systemTar) ExecuteWithoutDeadline(c *core.CommandSpec) (core.CommandOutput, error) {
	return s.Execute(c)
}
func (s *systemTar) ExecuteWithStdinWithoutDeadline(c *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return s.ExecuteWithStdin(c, input)
}
func (s *systemTar) Execute(c *core.CommandSpec) (core.CommandOutput, error) {
	return s.ExecuteWithStdin(c, bytes.NewReader(nil))
}

func (s *systemTar) ExecuteWithStdin(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	sent, err := io.ReadAll(input)
	if err != nil {
		return core.CommandOutput{}, err
	}
	if command.Purpose == "send profile content" {
		s.uploads = append(s.uploads, sent)
		if s.deleteBeforeUpload > 0 {
			s.deleteBeforeUpload--
			for _, path := range s.storedFiles() {
				if err := os.Remove(path); err != nil {
					return core.CommandOutput{}, err
				}
			}
		}
	}
	if command.Purpose == "publish profile content" && s.collectBeforePublish > 0 {
		s.collectBeforePublish--
		stages, err := s.stagingDirectories()
		if err != nil {
			return core.CommandOutput{}, err
		}
		for _, path := range stages {
			// The real receiver already exited quiescently. Model a
			// different daemon collecting the inter-phase gap.
			if err := os.RemoveAll(path); err != nil {
				return core.CommandOutput{}, err
			}
		}
	}
	if len(command.Args) == 0 {
		return core.CommandOutput{}, fmt.Errorf("missing remote command")
	}
	script := command.Args[len(command.Args)-1]
	local := core.NewCommandSpec("sh", []string{"-c", script})
	local.Env = map[string]string{"HOME": s.home}
	home := s.home
	local.Cwd = &home
	if s.bin != nil {
		local.Env["PATH"] = *s.bin + ":" + os.Getenv("PATH")
	}
	output, err := core.ProcessExecutor{}.ExecuteWithStdin(&local, bytes.NewReader(sent))
	if err != nil {
		return core.CommandOutput{}, err
	}
	if command.Purpose == "send profile content" && s.disconnectAfterReceive {
		s.disconnectAfterReceive = false
		pid := os.Getpid()
		birth, err := core.ProcessBirthIdentity(uint32(pid))
		if err != nil {
			return core.CommandOutput{}, err
		}
		stages, err := s.stagingDirectories()
		if err != nil {
			return core.CommandOutput{}, err
		}
		for _, path := range stages {
			if err := os.WriteFile(filepath.Join(path, ".owner"), []byte(fmt.Sprintf("%d\n%s\n", pid, birth)), 0o666); err != nil {
				return core.CommandOutput{}, err
			}
		}
		output.Status = 255
		output.Stderr = []byte("simulated SSH connection loss with a surviving receiver")
	}
	s.warnings = append(s.warnings, lossyText(output.Stderr))
	return output, nil
}

var unusedSSH = core.SshTarget{Destination: "unused", SSHArgs: []string{}}

func legacyProfileUpload(executor core.CommandExecutor, source, destination string) error {
	ssh := unusedSSH
	return uploadLegacyProfileArchive(executor, &ssh, source, destination, SharedProfilePaths(core.HarnessKindClaude))
}

// profileSnapshotEntry is one path's owner permission bits and contents.
type profileSnapshotEntry struct {
	mode  os.FileMode
	bytes string
}

// profileSnapshot is every path below root, following links, with its owner
// permission bits and file contents.
func profileSnapshot(t *testing.T, root string) map[string]profileSnapshotEntry {
	t.Helper()
	result := map[string]profileSnapshotEntry{}
	var visit func(path string)
	visit = func(path string) {
		absolute := filepath.Join(root, path)
		metadata, err := os.Stat(absolute)
		if err != nil {
			t.Fatal(err)
		}
		var contents []byte
		if !metadata.IsDir() {
			if contents, err = os.ReadFile(absolute); err != nil {
				t.Fatal(err)
			}
		}
		result[path] = profileSnapshotEntry{mode: metadata.Mode().Perm() & 0o700, bytes: string(contents)}
		if metadata.IsDir() {
			entries, err := os.ReadDir(absolute)
			if err != nil {
				t.Fatal(err)
			}
			for _, entry := range entries {
				visit(filepath.Join(path, entry.Name()))
			}
		}
	}
	visit("")
	return result
}

// archiveEntryNames is the names in a gzip tar.
func archiveEntryNames(t *testing.T, compressed []byte) []string {
	t.Helper()
	decompressed, err := gzip.NewReader(bytes.NewReader(compressed))
	if err != nil {
		t.Fatal(err)
	}
	reader := tar.NewReader(decompressed)
	var names []string
	for {
		header, err := reader.Next()
		if err == io.EOF {
			return names
		}
		if err != nil {
			t.Fatal(err)
		}
		names = append(names, header.Name)
	}
}

// underMjContent is Rust's `path.starts_with(".mj-content")`.
func underMjContent(name string) bool {
	return name == ".mj-content" || strings.HasPrefix(name, ".mj-content/")
}

// heldTar is a systemTar whose profile transfers wait for the test.
type heldTar struct {
	*systemTar
	reached chan<- struct{}
	release <-chan struct{}
}

func (h *heldTar) CancellationRequestedWithoutDeadline() bool { return false }
func (h *heldTar) ExecuteWithStdinWithoutDeadline(c *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return h.ExecuteWithStdin(c, input)
}
func (h *heldTar) ExecuteWithStdin(c *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	if c.Purpose == "send profile content" {
		h.reached <- struct{}{}
		select {
		case <-h.release:
		case <-time.After(20 * time.Second):
			panic("test did not release transfer")
		}
	}
	return h.systemTar.ExecuteWithStdin(c, input)
}

// Hard-won: 73052bca7: an unrelated profile transfer held the host-wide gate until restart timed out.
func TestDistinctProfileContentTransfersOverlapAndIdenticalContentIsSentOnce(t *testing.T) {
	home := t.TempDir()
	first := filepath.Join(home, "first")
	second := filepath.Join(home, "second")
	for _, source := range []struct {
		path string
		seed byte
	}{{first, 'a'}, {second, 'b'}} {
		if err := os.MkdirAll(filepath.Join(source.path, "plugins"), 0o777); err != nil {
			t.Fatal(err)
		}
		state := uint64(source.seed)
		payload := make([]byte, 100_000)
		for i := range payload {
			state = state*6364136223846793005 + 1
			payload[i] = byte(state >> 32)
		}
		if err := os.WriteFile(filepath.Join(source.path, "plugins/content"), payload, 0o666); err != nil {
			t.Fatal(err)
		}
	}
	ssh := unusedSSH
	plugins := []string{"plugins"}

	reached := make(chan struct{}, 1)
	release := make(chan struct{}, 1)
	var group sync.WaitGroup
	group.Add(1)
	go func() {
		defer group.Done()
		held := &heldTar{systemTar: newSystemTar(home), reached: reached, release: release}
		if err := uploadLegacyProfileArchive(held, &ssh, first, filepath.Join(home, "held"), plugins); err != nil {
			t.Errorf("held upload: %v", err)
			return
		}
		if len(held.uploads[0]) <= 64*1024 {
			t.Errorf("held upload sent only %d bytes", len(held.uploads[0]))
		}
	}()
	select {
	case <-reached:
	case <-time.After(5 * time.Second):
		t.Fatal("the held transfer never started")
	}
	done := make(chan struct{}, 1)
	group.Add(2)
	go func() {
		defer group.Done()
		tar := newSystemTar(home)
		if err := uploadLegacyProfileArchive(tar, &ssh, second, filepath.Join(home, "independent"), plugins); err != nil {
			t.Errorf("independent upload: %v", err)
			return
		}
		done <- struct{}{}
	}()
	go func() {
		defer group.Done()
		tar := newSystemTar(home)
		if err := uploadLegacyProfileArchive(tar, &ssh, first, filepath.Join(home, "same"), plugins); err != nil {
			t.Errorf("same upload: %v", err)
			return
		}
		for _, name := range archiveEntryNames(t, tar.uploads[0]) {
			if underMjContent(name) {
				t.Errorf("identical content was transferred twice")
			}
		}
	}()
	overlapped := false
	select {
	case <-done:
		overlapped = true
	case <-time.After(10 * time.Second):
	}
	release <- struct{}{}
	group.Wait()
	if !overlapped {
		t.Fatal("unrelated content queued behind a held transfer")
	}

	// Already cached shared content must not serialize private archives,
	// even when both profiles have the exact same content-key set.
	reached = make(chan struct{}, 1)
	release = make(chan struct{}, 1)
	group.Add(1)
	go func() {
		defer group.Done()
		held := &heldTar{systemTar: newSystemTar(home), reached: reached, release: release}
		if err := uploadLegacyProfileArchive(held, &ssh, first, filepath.Join(home, "cached-held"), plugins); err != nil {
			t.Errorf("cached held upload: %v", err)
		}
	}()
	select {
	case <-reached:
	case <-time.After(5 * time.Second):
		t.Fatal("the cached held transfer never started")
	}
	done = make(chan struct{}, 1)
	group.Add(1)
	go func() {
		defer group.Done()
		tar := newSystemTar(home)
		if err := uploadLegacyProfileArchive(tar, &ssh, first, filepath.Join(home, "cached-parallel"), plugins); err != nil {
			t.Errorf("cached parallel upload: %v", err)
			return
		}
		done <- struct{}{}
	}()
	overlapped = false
	select {
	case <-done:
		overlapped = true
	case <-time.After(10 * time.Second):
	}
	release <- struct{}{}
	group.Wait()
	if !overlapped {
		t.Fatal("cached private archives queued behind each other")
	}
	if got, want := profileSnapshot(t, filepath.Join(home, "same")), profileSnapshot(t, first); !reflect.DeepEqual(got, want) {
		t.Fatalf("the same profile arrived changed: %v", got)
	}
}

// Hard-won: efbd5c927: interrupted profile receivers left private staging outside collection.
func TestProfileCollectionReclaimsFailedAndAbandonedStagingButPreservesOwners(t *testing.T) {
	temp := t.TempDir()
	source := filepath.Join(temp, "source")
	if err := os.Mkdir(source, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "settings.json"), bytes.Repeat([]byte{'x'}, 100_000), 0o666); err != nil {
		t.Fatal(err)
	}
	executor := newSystemTar(temp)
	executor.disconnectAfterReceive = true
	failure := legacyProfileUpload(executor, source, filepath.Join(temp, "disconnected-profile"))
	if failure == nil || !strings.Contains(failure.Error(), "simulated SSH connection loss") {
		t.Fatalf("%v", failure)
	}
	stages, err := executor.stagingDirectories()
	if err != nil || len(stages) == 0 {
		t.Fatalf("SSH status 255 deleted a surviving receiver's files: %v", err)
	}
	interrupted := stages[0]
	if _, err := os.Stat(filepath.Join(interrupted, "profile/settings.json")); err != nil {
		t.Fatal(err)
	}
	// Finish the simulated receiver by restoring its completed real owner.
	copyTestFile(t, filepath.Join(interrupted, ".quiescent"), filepath.Join(interrupted, ".owner"))
	executor.collectBeforePublish = 1
	before := len(executor.uploads)
	recovered := filepath.Join(temp, "automatically-recovered")
	if err := legacyProfileUpload(executor, source, recovered); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(profileSnapshot(t, recovered), profileSnapshot(t, source)) {
		t.Fatal("the recovered profile differs from its source")
	}
	if len(executor.uploads)-before != 2 {
		t.Fatal("collector loss did not restart from transfer")
	}
	executor.collectBeforePublish = 2
	before = len(executor.uploads)
	repeated := legacyProfileUpload(executor, source, filepath.Join(temp, "repeated-loss"))
	if repeated == nil || !strings.Contains(repeated.Error(), "stage removed by a collector") {
		t.Fatalf("%v", repeated)
	}
	if len(executor.uploads)-before != 2 {
		t.Fatal("collector loss changed the existing retry count")
	}
	entries, err := profileArchiveEntries(source, nil)
	if err != nil {
		t.Fatal(err)
	}
	compressed, err := buildProfileArchive(source, entries, map[string]struct{}{})
	if err != nil {
		t.Fatal(err)
	}
	decompressed, err := gzip.NewReader(compressed)
	if err != nil {
		t.Fatal(err)
	}
	var stored bytes.Buffer
	encoder, _ := gzip.NewWriterLevel(&stored, gzip.NoCompression)
	if _, err := io.Copy(encoder, decompressed); err != nil {
		t.Fatal(err)
	}
	if err := encoder.Close(); err != nil {
		t.Fatal(err)
	}
	compressed.Close()
	archive := stored.Bytes()
	if len(archive) <= 64*1024 {
		t.Fatalf("the archive is only %d bytes", len(archive))
	}
	ssh := unusedSSH
	stage := func(token string, sent []byte) core.CommandOutput {
		work := profileContentStore + "/.stage-" + token
		command := core.SSHCommand(core.SshTrafficClassBulk, &ssh, []string{
			"sh", "-c", profileScript(stageProfileContent), "mj-profile-stage", work,
		}).WithPurpose("send profile content")
		output, err := executor.ExecuteWithStdin(&command, bytes.NewReader(sent))
		if err != nil {
			t.Fatal(err)
		}
		return output
	}
	exists := func(path string) bool {
		_, err := os.Lstat(path)
		return err == nil
	}
	if stage("failed", archive[:len(archive)-1024]).Status == 0 {
		t.Fatal("a truncated archive was received")
	}
	if exists(filepath.Join(executor.store(), ".stage-failed")) {
		t.Fatal("receiver failure left private files")
	}
	if stage("collected-between-phases", archive).Status != 0 {
		t.Fatal("staging failed")
	}
	if err := os.Rename(filepath.Join(executor.store(), ".stage-collected-between-phases"),
		filepath.Join(executor.store(), ".retired-between-phases")); err != nil {
		t.Fatal(err)
	}
	destination := filepath.Join(temp, "must-not-publish")
	publish := core.SSHCommand(core.SshTrafficClassBulk, &ssh, []string{
		"sh", "-c", profileScript(installProfileContent), "mj-profile-archive",
		destination, profileContentStore, profileContentStore + "/.stage-collected-between-phases",
	}).WithPurpose("publish profile content")
	removed, err := executor.Execute(&publish)
	if err != nil {
		t.Fatal(err)
	}
	if removed.Status != profileStageMissing {
		t.Fatalf("status %d", removed.Status)
	}
	if !strings.Contains(lossyText(removed.Stderr), "stage removed by a collector") {
		t.Fatalf("%s", removed.Stderr)
	}
	if exists(destination) {
		t.Fatal("a missing receive was published as an empty profile")
	}
	if exists(filepath.Join(executor.store(), ".stage-collected-between-phases")) {
		t.Fatal("the publication recreated a collected stage")
	}
	if !exists(filepath.Join(executor.store(), ".retired-between-phases/profile/settings.json")) {
		t.Fatal("the retirement lost its files")
	}
	for _, token := range []string{"abandoned", "registered", "live", "reused", "unknown", "raced", "unquiescent"} {
		if stage(token, archive).Status != 0 {
			t.Fatalf("staging %s failed", token)
		}
	}
	pid := os.Getpid()
	birth, err := core.ProcessBirthIdentity(uint32(pid))
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(executor.store(), ".stage-live/.owner"), []byte(fmt.Sprintf("%d\n%s\n", pid, birth)), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(executor.store(), ".stage-reused/.owner"), []byte(fmt.Sprintf("%d\n%s-another-process\n", pid, birth)), 0o666); err != nil {
		t.Fatal(err)
	}
	copyTestFile(t, filepath.Join(executor.store(), ".stage-reused/.owner"), filepath.Join(executor.store(), ".stage-reused/.quiescent"))
	if err := os.Remove(filepath.Join(executor.store(), ".stage-unquiescent/.quiescent")); err != nil {
		t.Fatal(err)
	}
	abruptOwner, err := os.ReadFile(filepath.Join(executor.store(), ".stage-unquiescent/.owner"))
	if err != nil {
		t.Fatal(err)
	}
	abrupt := strings.ReplaceAll(string(abruptOwner), "\n", ":")
	if err := os.Remove(filepath.Join(executor.store(), ".stage-unknown/.owner")); err != nil {
		t.Fatal(err)
	}
	host := core.TargetsTargetLocator{Kind: core.TargetsTargetLocatorSshBare, SSH: &ssh}
	key := StoreKey(&host, profileContentStore)
	gate := StoreGateFor(&host, profileContentStore)
	storeGuard, err := LockGate(executor, gate, "test", "register test profile staging")
	if err != nil {
		t.Fatal(err)
	}
	active := registerProfileUpload(key, "registered")
	storeGuard.Release()
	bin := filepath.Join(temp, "bin")
	if err := os.Mkdir(bin, 0o777); err != nil {
		t.Fatal(err)
	}
	which := core.NewCommandSpec("sh", []string{"-c", "command -v mv"})
	resolvedMv, err := core.ProcessExecutor{}.Execute(&which)
	if err != nil {
		t.Fatal(err)
	}
	// Claim first, after the collector's initial dead-owner snapshot but
	// before its rename. The changed marker must force restoration.
	claim := profileScript("work=$1; mj_profile_claim; mj_profile_quiescent")
	core.InstallFakeCommand(bin, "mv", fmt.Sprintf(
		"#!/bin/sh\n[ \"$1\" != -- ] || shift\nif [ \"${1##*/}\" = .stage-raced ]; then\n sh -c %s mj-test-owner \"$1\" || exit\nfi\nexec %s -- \"$@\"\n",
		core.PosixQuote(claim), core.PosixQuote(strings.TrimSpace(string(resolvedMv.Stdout)))))
	executor.bin = &bin
	if err := legacyProfileUpload(executor, source, filepath.Join(temp, "profile")); err != nil {
		t.Fatal(err)
	}
	if exists(interrupted) {
		t.Fatal("a completed disconnected receiver was never reclaimed")
	}
	for _, token := range []string{"abandoned", "reused"} {
		if exists(filepath.Join(executor.store(), ".stage-"+token)) {
			t.Fatalf(".stage-%s survived", token)
		}
	}
	for _, token := range []string{"registered", "live", "unknown", "raced", "unquiescent"} {
		if !exists(filepath.Join(executor.store(), ".stage-"+token)) {
			t.Fatalf(".stage-%s was collected", token)
		}
	}
	warned := func(match func(string) bool) bool {
		for _, warning := range executor.warnings {
			if match(warning) {
				return true
			}
		}
		return false
	}
	if !warned(func(w string) bool {
		return strings.Contains(w, "ownership unknown") && strings.Contains(w, ".stage-unknown")
	}) {
		t.Fatalf("no warning names the unknown owner: %q", executor.warnings)
	}
	if !warned(func(w string) bool { return strings.Contains(w, ".stage-unquiescent") && strings.Contains(w, abrupt) }) {
		t.Fatalf("no warning names the unquiescent owner: %q", executor.warnings)
	}
	active.drop()
	executor.bin = nil
	if err := legacyProfileUpload(executor, source, filepath.Join(temp, "next-profile")); err != nil {
		t.Fatal(err)
	}
	if exists(filepath.Join(executor.store(), ".stage-raced")) {
		t.Fatal("a restored dead owner was never reclaimed")
	}
	if exists(filepath.Join(executor.store(), ".retired-between-phases")) {
		t.Fatal("an abandoned retirement was never reclaimed")
	}
	if exists(filepath.Join(executor.store(), ".stage-registered")) {
		t.Fatal("finished local owner was never reclaimed")
	}
}

func copyTestFile(t *testing.T, source, destination string) {
	t.Helper()
	if err := installCopyFile(source, destination); err != nil {
		t.Fatal(err)
	}
}

func installPrivateProfile(t *testing.T, staging, destination string) {
	t.Helper()
	if err := os.Mkdir(destination, 0o777); err != nil {
		t.Fatal(err)
	}
	if _, err := targets.ExecuteChecked(core.ProcessExecutor{},
		core.NewCommandSpec("cp", []string{"-R", staging + "/.", destination})); err != nil {
		t.Fatal(err)
	}
	if err := os.RemoveAll(staging); err != nil {
		t.Fatal(err)
	}
	if _, err := targets.ExecuteChecked(core.ProcessExecutor{},
		core.NewCommandSpec("chmod", []string{"-R", "--", "go-rwx", destination})); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: #1483: recursive scp spent 162 seconds on a 17 MB profile.
// Hard-won: #1187: BSD chmod treats -- after the mode as a filename.
func TestStreamedProfileRoundtripsThroughSystemTarWithoutPerFileCommands(t *testing.T) {
	temp := t.TempDir()
	source := filepath.Join(temp, "staged profile")
	nested := filepath.Join(source, "plugins/日本語")
	if err := os.MkdirAll(nested, 0o777); err != nil {
		t.Fatal(err)
	}
	// The production archive is over 8 MiB: exercise pipe backpressure too.
	payload := make([]byte, 8*1024*1024+70_000)
	if _, err := rand.Read(payload); err != nil {
		t.Fatal(err)
	}
	for _, file := range []struct {
		name string
		mode os.FileMode
	}{{"tool data", 0o755}, {"line\nbreak", 0o644}, {"-leading dash", 0o644}} {
		if err := os.WriteFile(filepath.Join(nested, file.name), payload, 0o666); err != nil {
			t.Fatal(err)
		}
		if err := os.Chmod(filepath.Join(nested, file.name), file.mode); err != nil {
			t.Fatal(err)
		}
	}
	for _, directory := range []string{filepath.Join(source, "skills/empty"), filepath.Join(source, "empty")} {
		if err := os.MkdirAll(directory, 0o777); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.WriteFile(filepath.Join(source, ".credentials.json"), []byte("synthetic credentials"), 0o666); err != nil {
		t.Fatal(err)
	}
	// Hard-won: review of 7954fde2e: profile files named like the
	// archive's own manifest and content directory must survive.
	if err := os.WriteFile(filepath.Join(source, ".mj-keys"), []byte("a profile file"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, ".mj-content"), []byte("another profile file"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("plugins/日本語/tool data", filepath.Join(source, "linked plugin")); err != nil {
		t.Fatal(err)
	}
	executor := newSystemTar(temp)
	expected := profileSnapshot(t, source)
	for index := range 2 {
		staging := filepath.Join(temp, fmt.Sprintf("remote staging %d", index))
		destination := filepath.Join(temp, fmt.Sprintf("private profile %d", index))
		abandoned := filepath.Join(executor.store(), strings.Repeat("0", 64))
		abandonedUpload := filepath.Join(executor.store(), ".upload-abandoned")
		if index == 1 {
			if err := os.WriteFile(abandoned, []byte("unused content"), 0o666); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(abandonedUpload, []byte("interrupted publication"), 0o666); err != nil {
				t.Fatal(err)
			}
			args := []string{"-t", "200001010000"}
			entries, err := os.ReadDir(executor.store())
			if err != nil {
				t.Fatal(err)
			}
			for _, entry := range entries {
				args = append(args, filepath.Join(executor.store(), entry.Name()))
			}
			if _, err := targets.ExecuteChecked(core.ProcessExecutor{}, core.NewCommandSpec("touch", args)); err != nil {
				t.Fatal(err)
			}
		}
		if err := legacyProfileUpload(executor, source, staging); err != nil {
			t.Fatal(err)
		}
		if index == 1 {
			for _, path := range []string{abandoned, abandonedUpload} {
				if _, err := os.Stat(path); err == nil {
					t.Fatalf("%s survived collection", path)
				}
			}
		}
		for path, entry := range expected {
			info, err := os.Stat(filepath.Join(staging, path))
			if err != nil {
				t.Fatal(err)
			}
			if info.Mode().Perm() != entry.mode {
				t.Fatalf("staging permissions: %s: %o", path, info.Mode().Perm())
			}
		}
		installPrivateProfile(t, staging, destination)
		if !reflect.DeepEqual(profileSnapshot(t, destination), expected) {
			t.Fatal("the installed profile differs from its source")
		}
		for path, entry := range expected {
			metadata, err := os.Lstat(filepath.Join(destination, path))
			if err != nil {
				t.Fatal(err)
			}
			if metadata.Mode()&os.ModeSymlink != 0 {
				t.Fatalf("%s is a link", path)
			}
			if metadata.Mode().Perm() != entry.mode {
				t.Fatalf("%s: mode %o", path, metadata.Mode().Perm())
			}
			for _, stored := range executor.storedFiles() {
				storedInfo, err := os.Stat(stored)
				if err != nil {
					t.Fatal(err)
				}
				if metadata.Sys().(*syscall.Stat_t).Ino == storedInfo.Sys().(*syscall.Stat_t).Ino {
					t.Fatalf("%s shares an inode with the store", path)
				}
			}
		}
		// A write to a private session copy cannot corrupt later installs.
		if err := os.WriteFile(filepath.Join(destination, "plugins/日本語/tool data"), []byte("session mutation"), 0o666); err != nil {
			t.Fatal(err)
		}
	}
	if len(executor.uploads) != 2 {
		t.Fatalf("%d uploads", len(executor.uploads))
	}
	for index, sent := range executor.uploads {
		shared := 0
		for _, name := range archiveEntryNames(t, sent) {
			if underMjContent(name) {
				shared++
			}
		}
		want := 0
		if index == 0 {
			want = 2
		}
		if shared != want {
			t.Fatalf("upload %d carried %d content files", index, shared)
		}
	}
	store := executor.storedFiles()
	if len(store) != 2 {
		t.Fatalf("store holds %d files", len(store))
	}
	for _, path := range store {
		contents, err := os.ReadFile(path)
		if err != nil {
			t.Fatal(err)
		}
		if !bytes.Equal(contents, payload) {
			t.Fatalf("%s holds other bytes", path)
		}
		info, err := os.Stat(path)
		if err != nil {
			t.Fatal(err)
		}
		want := os.FileMode(0o600)
		if filepath.Ext(path) == ".x" {
			want = 0o700
		}
		if info.Mode().Perm() != want {
			t.Fatalf("%s: mode %o", path, info.Mode().Perm())
		}
	}
}

func TestProfileContentDisappearingAfterQueryRetriesOnceWithTheMissingBytes(t *testing.T) {
	temp := t.TempDir()
	source := filepath.Join(temp, "source")
	if err := os.MkdirAll(filepath.Join(source, "plugins"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "plugins/content"), bytes.Repeat([]byte{'x'}, 100_000), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "settings.json"), []byte("private settings"), 0o666); err != nil {
		t.Fatal(err)
	}
	executor := newSystemTar(temp)
	if err := legacyProfileUpload(executor, source, filepath.Join(temp, "initial")); err != nil {
		t.Fatal(err)
	}
	executor.deleteBeforeUpload = 1
	recovered := filepath.Join(temp, "recovered")
	if err := legacyProfileUpload(executor, source, recovered); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(profileSnapshot(t, recovered), profileSnapshot(t, source)) {
		t.Fatal("the recovered profile differs from its source")
	}
	if len(executor.uploads) != 3 {
		t.Fatalf("%d uploads", len(executor.uploads))
	}
	// The real link script reports exactly which content disappeared.
	stored := executor.storedFiles()
	key := filepath.Base(stored[len(stored)-1])
	entries, err := profileArchiveEntries(source, []string{"plugins"})
	if err != nil {
		t.Fatal(err)
	}
	archive, err := buildProfileArchive(source, entries, map[string]struct{}{})
	if err != nil {
		t.Fatal(err)
	}
	defer archive.Close()
	if err := os.Remove(filepath.Join(executor.store(), key)); err != nil {
		t.Fatal(err)
	}
	destination := filepath.Join(temp, "missing")
	work := "missing-work"
	ssh := unusedSSH
	send := core.SSHCommand(core.SshTrafficClassBulk, &ssh, []string{
		"sh", "-c", profileScript(stageProfileContent), "mj-profile-stage", work,
	}).WithPurpose("send profile content")
	if _, err := executor.ExecuteWithStdin(&send, archive); err != nil {
		t.Fatal(err)
	}
	command := core.SSHCommand(core.SshTrafficClassControl, &ssh, []string{
		"sh", "-c", profileScript(installProfileContent), "mj-profile-archive", destination, profileContentStore, work,
	}).WithPurpose("publish profile content")
	output, err := executor.Execute(&command)
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != profileContentMissing {
		t.Fatalf("status %d: %s", output.Status, output.Stderr)
	}
	if got := strings.TrimSpace(string(output.Stdout)); got != key {
		t.Fatalf("reported %q, want %q", got, key)
	}
}

// Hard-won: review of 09769fc85: a stored file that exists but cannot be
// copied must fail the upload instead of leaving a profile with gaps.
func TestStoredProfileContentThatCannotBeCopiedFailsTheUpload(t *testing.T) {
	temp := t.TempDir()
	source := filepath.Join(temp, "source")
	if err := os.MkdirAll(filepath.Join(source, "plugins"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(source, "plugins/content"), bytes.Repeat([]byte{'x'}, 100_000), 0o666); err != nil {
		t.Fatal(err)
	}
	executor := newSystemTar(temp)
	if err := legacyProfileUpload(executor, source, filepath.Join(temp, "initial")); err != nil {
		t.Fatal(err)
	}
	files := executor.storedFiles()
	stored := files[len(files)-1]
	if err := os.Chmod(stored, 0o000); err != nil {
		t.Fatal(err)
	}
	err := legacyProfileUpload(executor, source, filepath.Join(temp, "unreadable"))
	if chmodErr := os.Chmod(stored, 0o600); chmodErr != nil {
		t.Fatal(chmodErr)
	}
	if err == nil || !strings.Contains(err.Error(), "could not copy stored profile content") {
		t.Fatalf("%v", err)
	}
}
