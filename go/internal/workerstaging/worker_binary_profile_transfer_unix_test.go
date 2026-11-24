// Port of the tests of mj-worker-staging/src/worker_binary/profile_transfer.rs
// (`#[cfg(all(test, unix))] mod tests`).

//go:build unix

package workerstaging

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"maps"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/targets"
)

// profileHold pauses a receiver's input once its offer is complete, until
// release (Rust's Hold).
type profileHold struct {
	reached chan<- struct{}
	release <-chan struct{}
}

// profileTestReceiver is Rust's LocalReceiver: it runs the receiver an SSH
// command names as a local process with HOME at home, and records each
// batch it was sent.
type profileTestReceiver struct {
	core.CommandExecutorDefaults
	home    string
	mu      sync.Mutex
	batches [][]byte
	hold    *profileHold
	// pid is where the receiver records its process id; empty for none.
	pid string
	// nofile is the receiver's soft descriptor limit; 0 for the inherited
	// one.
	nofile uint32
}

func newProfileTestReceiver(home string) *profileTestReceiver {
	return &profileTestReceiver{home: home}
}

func (r *profileTestReceiver) recordedBatches() [][]byte {
	r.mu.Lock()
	defer r.mu.Unlock()
	return slices.Clone(r.batches)
}

// claimedObjects is the objects each batch carried, by key.
func (r *profileTestReceiver) claimedObjects(t *testing.T) []map[string]struct{} {
	t.Helper()
	var claimed []map[string]struct{}
	for _, batch := range r.recordedBatches() {
		objects := map[string]struct{}{}
		data := bytes.Index(batch, []byte("\n\n")) + 2
		if data == len(batch) {
			claimed = append(claimed, objects)
			continue
		}
		decompressed, err := gzip.NewReader(bytes.NewReader(batch[data:]))
		storeMust(t, err)
		archive := tar.NewReader(decompressed)
		for {
			header, err := archive.Next()
			if errors.Is(err, io.EOF) {
				break
			}
			storeMust(t, err)
			if key, ok := pathStripPrefix(header.Name, "objects"); ok {
				objects[key] = struct{}{}
			}
		}
		claimed = append(claimed, objects)
	}
	return claimed
}

func (r *profileTestReceiver) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

func (r *profileTestReceiver) CancellationRequestedWithoutDeadline() bool { return false }

func (r *profileTestReceiver) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	return r.ExecuteWithStdin(command, bytes.NewReader(nil))
}

func (r *profileTestReceiver) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return r.Execute(command)
}

func (r *profileTestReceiver) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return r.ExecuteCleanup(command)
}

func (r *profileTestReceiver) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return r.Execute(command)
}

func (r *profileTestReceiver) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return r.ExecuteWithStdin(command, input)
}

// profileRecord records what a receiver read, holding it once the offer is
// complete.
type profileRecord struct {
	input io.Reader
	bytes []byte
	hold  *profileHold
}

func (r *profileRecord) Read(buffer []byte) (int, error) {
	count, err := r.input.Read(buffer)
	if bytes.HasSuffix(r.bytes, []byte("\n\n")) && count > 0 && r.hold != nil {
		hold := r.hold
		r.hold = nil
		hold.reached <- struct{}{}
		select {
		case <-hold.release:
		case <-time.After(15 * time.Second):
			panic("the held receiver was never released")
		}
	}
	r.bytes = append(r.bytes, buffer[:count]...)
	return count, err
}

func (r *profileTestReceiver) ExecuteWithStdin(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	local := *command
	script := command.Args[len(command.Args)-1]
	var args []string
	if r.pid != "" {
		script = fmt.Sprintf("printf '%%s\\n' \"$$\" > \"$1\"; exec %s", script)
		args = append(args, "test-receiver", r.pid)
	}
	if r.nofile != 0 {
		script = fmt.Sprintf("ulimit -Sn %d || exit; %s", r.nofile, script)
	}
	local.Args = append([]string{"-c", script}, args...)
	local.Program = "sh"
	local.SSHSession = nil
	local.SSHDestination = nil
	cwd := r.home
	local.Cwd = &cwd
	local.Env = maps.Clone(command.Env)
	if local.Env == nil {
		local.Env = map[string]string{}
	}
	local.Env["HOME"] = r.home
	r.mu.Lock()
	hold := r.hold
	r.hold = nil
	r.mu.Unlock()
	record := &profileRecord{input: input, hold: hold}
	output, err := core.ProcessExecutor{}.ExecuteWithStdin(&local, record)
	r.mu.Lock()
	r.batches = append(r.batches, record.bytes)
	r.mu.Unlock()
	return output, err
}

func profileTestUpload(executor *profileTestReceiver, ssh *core.SshTarget, source, destination string, sharedPaths []string) error {
	manifest, err := ProfileContentManifestFromStaged(source, sharedPaths)
	if err != nil {
		return err
	}
	return transfer(executor, ssh, manifest, &destination)
}

func profileTestSSH() *core.SshTarget {
	return &core.SshTarget{Destination: "unused", SSHArgs: []string{}}
}

func profileTestDigest(body []byte) string {
	sum := sha256.Sum256(body)
	return core.LowerHex(sum[:])
}

// profileTestSet is a set of object keys.
func profileTestSet(keys ...string) map[string]struct{} {
	set := map[string]struct{}{}
	for _, key := range keys {
		set[key] = struct{}{}
	}
	return set
}

func profileTestSetsEqual(left, right []map[string]struct{}) bool {
	return slices.EqualFunc(left, right, func(a, b map[string]struct{}) bool { return maps.Equal(a, b) })
}

// profileTestFlatten is every key of every batch, batch by batch in key
// order.
func profileTestFlatten(sets []map[string]struct{}) []string {
	var keys []string
	for _, set := range sets {
		keys = append(keys, slices.Sorted(maps.Keys(set))...)
	}
	return keys
}

func profileTestWaitFor(condition func() bool) bool {
	start := time.Now()
	for !condition() && time.Since(start) < 10*time.Second {
		time.Sleep(10 * time.Millisecond)
	}
	return condition()
}

func TestOverlappingProfileOffersShareClaimsAndRecoverADeadSender(t *testing.T) {
	home := t.TempDir()
	first := filepath.Join(home, "first")
	second := filepath.Join(home, "second")
	storeMust(t, os.Mkdir(first, 0o777))
	storeMust(t, os.Mkdir(second, 0o777))
	for _, profile := range []struct{ path, unique string }{{first, "only first"}, {second, "only second"}} {
		storeMust(t, os.WriteFile(filepath.Join(profile.path, "common"), []byte("both profiles"), 0o666))
		storeMust(t, os.WriteFile(filepath.Join(profile.path, "unique"), []byte(profile.unique), 0o666))
	}
	roots := []string{"common", "unique", "recover"}
	var firstDone, secondDone atomic.Bool
	owner := newProfileTestReceiver(home)
	contender := newProfileTestReceiver(home)
	reached := make(chan struct{}, 1)
	release := make(chan struct{}, 1)
	owner.hold = &profileHold{reached: reached, release: release}
	var group sync.WaitGroup
	group.Go(func() {
		if err := profileTestUpload(owner, profileTestSSH(), first, "first-out", roots); err != nil {
			t.Error(err)
		}
		firstDone.Store(true)
	})
	select {
	case <-reached:
	case <-time.After(10 * time.Second):
		t.Fatal("the owner never sent its batch")
	}
	group.Go(func() {
		if err := profileTestUpload(contender, profileTestSSH(), second, "second-out", roots); err != nil {
			t.Error(err)
		}
		secondDone.Store(true)
	})
	disjoint := filepath.Join(home, profileContentStore, profileTestDigest([]byte("only second")))
	if !profileTestWaitFor(func() bool { return storeExists(disjoint) }) {
		t.Error("the contender did not publish its own object")
	}
	if firstDone.Load() {
		t.Error("the held owner finished")
	}
	if secondDone.Load() {
		t.Error("the contender finished before the shared object was published")
	}
	release <- struct{}{}
	group.Wait()
	if t.Failed() {
		t.FailNow()
	}
	if got := storeRead(t, filepath.Join(home, "second-out/common")); string(got) != "both profiles" {
		t.Fatalf("second-out/common = %q", got)
	}
	if got := owner.claimedObjects(t); !profileTestSetsEqual(got, []map[string]struct{}{
		profileTestSet(profileTestDigest([]byte("both profiles")), profileTestDigest([]byte("only first"))),
	}) {
		t.Fatalf("owner claimed %v", got)
	}
	if got := profileTestFlatten(contender.claimedObjects(t)); !slices.Equal(got, []string{profileTestDigest([]byte("only second"))}) {
		t.Fatalf("contender claimed %v", got)
	}

	storeMust(t, os.WriteFile(filepath.Join(first, "recover"), []byte("dead owner bytes"), 0o666))
	pid := filepath.Join(home, "receiver.pid")
	failed := newProfileTestReceiver(home)
	failed.pid = pid
	reached = make(chan struct{}, 1)
	release = make(chan struct{}, 1)
	failed.hold = &profileHold{reached: reached, release: release}
	original := make(chan error, 1)
	go func() { original <- profileTestUpload(failed, profileTestSSH(), first, "dead-out", roots) }()
	select {
	case <-reached:
	case <-time.After(10 * time.Second):
		t.Fatal("the doomed sender never sent its batch")
	}
	recorded := strings.TrimSpace(string(storeRead(t, pid)))
	_, err := targets.ExecuteChecked(core.ProcessExecutor{}, core.NewCommandSpec("kill", []string{"-KILL", recorded}))
	storeMust(t, err)
	release <- struct{}{}
	if err := <-original; err == nil {
		t.Fatal("the killed receiver's transfer succeeded")
	}
	manifest, err := ProfileContentManifestFromStaged(first, roots)
	storeMust(t, err)
	recoverKey := profileTestDigest([]byte("dead owner bytes"))
	storePath := filepath.Join(home, profileContentStore)
	abandonedClaim := filepath.Join(storePath, ".stage-negotiated/claims", recoverKey)
	abandoned := storeInode(t, abandonedClaim)

	// Start two receivers against the same dead claim and hold whichever
	// receives it before any bytes are published. The other receiver must
	// observe the fresh link as busy while the winner still owns it.
	contenders := []*profileTestReceiver{newProfileTestReceiver(home), newProfileTestReceiver(home)}
	reached = make(chan struct{}, len(contenders))
	var releases []chan struct{}
	for _, receiver := range contenders {
		release := make(chan struct{}, 1)
		receiver.hold = &profileHold{reached: reached, release: release}
		releases = append(releases, release)
	}
	for _, receiver := range contenders {
		group.Go(func() {
			if err := transfer(receiver, profileTestSSH(), manifest, nil); err != nil {
				t.Error(err)
			}
		})
	}
	select {
	case <-reached:
	case <-time.After(10 * time.Second):
		t.Fatal("no contender received the dead claim")
	}
	anyBatch := func() bool {
		return slices.ContainsFunc(contenders, func(receiver *profileTestReceiver) bool {
			return len(receiver.recordedBatches()) != 0
		})
	}
	if !profileTestWaitFor(anyBatch) {
		t.Error("the busy contender never finished a negotiation")
	}
	if storeExists(filepath.Join(storePath, recoverKey)) {
		t.Error("the object was published while its owner was held")
	}
	if storeInode(t, abandonedClaim) == abandoned {
		t.Error("the dead claim was not taken over")
	}
	for _, release := range releases {
		// The receiver that got only a busy reply dropped its hold.
		release <- struct{}{}
	}
	group.Wait()
	if t.Failed() {
		t.FailNow()
	}
	var recovered []string
	for _, receiver := range contenders {
		recovered = append(recovered, profileTestFlatten(receiver.claimedObjects(t))...)
	}
	if !slices.Equal(recovered, []string{recoverKey}) {
		t.Fatalf("recovered %v", recovered)
	}
	if got := storeRead(t, filepath.Join(storePath, recoverKey)); string(got) != "dead owner bytes" {
		t.Fatalf("recovered object = %q", got)
	}
	if storeExists(abandonedClaim) {
		t.Fatal("the dead claim survived its recovery")
	}
}

// Hard-won: c4831a676: cold EC2 receivers exhausted 1,024 file descriptors holding one lock per object.
func TestColdProfileClaimsMoreObjectsThanTheReceiverDescriptorLimit(t *testing.T) {
	home := t.TempDir()
	source := filepath.Join(home, "source")
	objects := filepath.Join(source, "rules")
	storeMust(t, os.MkdirAll(objects, 0o777))
	const limit = uint32(1024)
	expected := map[string]struct{}{}
	for index := range limit + 128 {
		body := binary.LittleEndian.AppendUint32(nil, index)
		storeMust(t, os.WriteFile(filepath.Join(objects, strconv.FormatUint(uint64(index), 10)), body, 0o666))
		expected[profileTestDigest(body)] = struct{}{}
	}
	receiver := newProfileTestReceiver(home)
	receiver.nofile = limit
	roots := SharedProfilePaths(core.HarnessKindCodex)
	manifest, err := ProfileContentManifestFromStaged(source, roots)
	storeMust(t, err)

	storeMust(t, transfer(receiver, profileTestSSH(), manifest, nil))
	storeMust(t, transfer(receiver, profileTestSSH(), manifest, nil))

	storePath := filepath.Join(home, profileContentStore)
	if got := receiver.claimedObjects(t); !profileTestSetsEqual(got, []map[string]struct{}{expected, {}}) {
		t.Fatalf("claimed %d batches, want every object once", len(got))
	}
	claims, err := os.ReadDir(filepath.Join(storePath, ".stage-negotiated/claims"))
	storeMust(t, err)
	if len(claims) != 0 {
		t.Fatalf("%d claims remain", len(claims))
	}
}

func TestNegotiatedProfileStreamSendsLargeReplicaOnceAndKeepsTokensPrivate(t *testing.T) {
	home := t.TempDir()
	source := filepath.Join(home, "source")
	storeMust(t, os.MkdirAll(filepath.Join(source, "replica"), 0o777))
	storeMust(t, os.MkdirAll(filepath.Join(source, "baseline"), 0o777))
	body := make([]byte, 96*1024+17)
	_, err := rand.Read(body)
	storeMust(t, err)
	storeMust(t, os.WriteFile(filepath.Join(source, "replica/topic"), body, 0o666))
	storeMust(t, os.WriteFile(filepath.Join(source, "baseline/topic"), body, 0o666))
	storeMust(t, os.WriteFile(filepath.Join(source, "auth.json"), []byte("a private login"), 0o666))
	storeMust(t, os.WriteFile(filepath.Join(source, "settings.json"), []byte(`{"enabled":true}`), 0o666))
	storeMust(t, os.WriteFile(filepath.Join(source, "config.toml"),
		[]byte("experimental_bearer_token = 'a literal secret'"), 0o666))
	receiver := newProfileTestReceiver(home)
	roots := []string{"replica", "baseline"}
	manifest, err := ProfileContentManifestFromStaged(source, roots)
	storeMust(t, err)
	storeMust(t, transfer(receiver, profileTestSSH(), manifest, nil))
	if storeExists(filepath.Join(home, "warm")) {
		t.Fatal("a prewarm materialized the profile")
	}
	storeMust(t, profileTestUpload(receiver, profileTestSSH(), source, "warm", roots))
	if got := receiver.claimedObjects(t); !profileTestSetsEqual(got, []map[string]struct{}{
		profileTestSet(profileTestDigest(body)), {},
	}) {
		t.Fatalf("claimed %v", got)
	}
	if size := len(receiver.recordedBatches()[0]); size <= 64*1024 {
		t.Fatalf("the first batch carried only %d bytes", size)
	}
	for _, file := range []string{"baseline/topic", "replica/topic", "auth.json", "config.toml", "settings.json"} {
		if !bytes.Equal(storeRead(t, filepath.Join(home, "warm", file)), storeRead(t, filepath.Join(source, file))) {
			t.Fatalf("warm/%s differs from the stage", file)
		}
	}
	storePath := filepath.Join(home, profileContentStore)
	if storeExists(filepath.Join(storePath, profileTestDigest([]byte("a private login")))) {
		t.Fatal("the login entered the shared store")
	}
	if storeExists(filepath.Join(storePath, profileTestDigest([]byte("experimental_bearer_token = 'a literal secret'")))) {
		t.Fatal("the configuration entered the shared store")
	}
}

// Hard-won: 75f2a04dd: opaque configuration arguments could enter the shared store.
func TestUnclassifiedProfileEntriesStayPrivate(t *testing.T) {
	home := t.TempDir()
	source := filepath.Join(home, "source")
	storeMust(t, os.MkdirAll(filepath.Join(source, "plugins"), 0o777))
	plugin := []byte("declared plugin asset")
	storeMust(t, os.WriteFile(filepath.Join(source, "plugins/asset"), plugin, 0o666))
	entries := []struct {
		name string
		body []byte
	}{
		{"future-harness-entry", []byte("unknown staged entry")},
		{"mcp.json", []byte(`{"mcpServers":{"example":{"command":"example","args":["--token","synthetic-argument-secret"]}}}`)},
	}
	for _, entry := range entries {
		storeMust(t, os.WriteFile(filepath.Join(source, entry.name), entry.body, 0o666))
	}
	receiver := newProfileTestReceiver(home)
	roots := SharedProfilePaths(core.HarnessKindKimi)
	storeMust(t, profileTestUpload(receiver, profileTestSSH(), source, "installed", roots))
	if got := receiver.claimedObjects(t); !profileTestSetsEqual(got, []map[string]struct{}{profileTestSet(profileTestDigest(plugin))}) {
		t.Fatalf("claimed %v", got)
	}
	for _, entry := range entries {
		if !bytes.Equal(storeRead(t, filepath.Join(home, "installed", entry.name)), entry.body) {
			t.Fatalf("installed/%s differs", entry.name)
		}
		if storeExists(filepath.Join(home, profileContentStore, profileTestDigest(entry.body))) {
			t.Fatalf("%s entered the shared store", entry.name)
		}
	}
}
