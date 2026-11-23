// Port of the tests in mj-checkpoint/src/snapshot.rs.

package checkpoint

import (
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"net/url"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// fileGit is the system Git with file-protocol submodules allowed and a
// fixed test identity.
type fileGit struct{}

func prepareFileGit(command *GitCommand) *GitCommand {
	prepared := GitCommand{
		Arguments: append([]string{"-c", "protocol.file.allow=always"}, command.Arguments...),
		Stdin:     command.Stdin,
		Env: append(slices.Clone(command.Env),
			[2]string{"GIT_AUTHOR_NAME", "Snapshot Test"},
			[2]string{"GIT_AUTHOR_EMAIL", "snapshot@example.test"},
			[2]string{"GIT_COMMITTER_NAME", "Snapshot Test"},
			[2]string{"GIT_COMMITTER_EMAIL", "snapshot@example.test"},
		),
	}
	return &prepared
}

func (fileGit) Run(path string, command *GitCommand) (GitOutput, error) {
	return SystemGit{}.Run(path, prepareFileGit(command))
}

func (fileGit) RunToFile(path string, command *GitCommand, output *os.File) (GitOutput, error) {
	return runToFile(SystemGit{}, path, prepareFileGit(command), output)
}

// snapGit runs Git through fileGit and returns its output without trailing
// newlines; it panics when Git fails.
func snapGit(path string, args ...string) string {
	output, err := fileGit{}.Run(path, &GitCommand{Arguments: args})
	if err != nil {
		panic(err)
	}
	if output.Status != 0 {
		panic(fmt.Sprintf("git %q: %s", args, checkpointLossy(output.Stderr)))
	}
	return strings.TrimRight(string(output.Stdout), "\n")
}

func snapMust[T any](value T, err error) T {
	if err != nil {
		panic(err)
	}
	return value
}

func snapMustDo(err error) {
	if err != nil {
		panic(err)
	}
}

type snapRepo struct {
	temp     string
	origin   string
	checkout string
}

func newSnapRepo(t *testing.T) *snapRepo { return newSnapRepoWithFormat(t, "sha1") }

func newSnapRepoWithFormat(t *testing.T, format string) *snapRepo {
	temp := t.TempDir()
	origin := filepath.Join(temp, "origin.git")
	checkout := filepath.Join(temp, "source")
	snapGit(temp, "init", "--bare", "--initial-branch=main", "--object-format="+format, origin)
	snapGit(temp, "clone", origin, checkout)
	snapMustDo(os.WriteFile(filepath.Join(checkout, "file"), []byte("first\nsecond\nthird\n"), 0o644))
	snapGit(checkout, "add", "file")
	snapGit(checkout, "commit", "-m", "base")
	snapGit(checkout, "push", "origin", "main")
	return &snapRepo{temp: temp, origin: origin, checkout: checkout}
}

func anyConsent() CaptureConsent {
	return CaptureConsent{
		Untracked:  UntrackedConsent{Kind: core.ConsentAny},
		Operations: OperationConsent{Kind: core.ConsentAny},
	}
}

func (r *snapRepo) capture(ownership CheckoutOwnership) WorkspaceSnapshot {
	return snapMust(CaptureWorkspaceSnapshot(fileGit{}, r.checkout, ownership, filepath.Join(r.temp, "snapshot"), anyConsent(), nil))
}

func (r *snapRepo) provision(name string) string {
	path := filepath.Join(r.temp, name)
	snapGit(r.temp, "clone", "--no-checkout", r.origin, path)
	return path
}

func (r *snapRepo) restore(snapshot WorkspaceSnapshot) string {
	restored, _ := r.restoreWithBaseline(snapshot)
	return restored
}

func (r *snapRepo) restoreWithBaseline(snapshot WorkspaceSnapshot) (string, map[string]string) {
	staging := r.provision("staging")
	baseline := snapshotState(staging, nil).refs
	notices := snapMust(RestoreWorkspaceSnapshot(fileGit{}, snapshot, staging))
	if len(notices) != 0 {
		panic(fmt.Sprintf("%q", notices))
	}
	published := filepath.Join(r.temp, "restored")
	snapMustDo(os.Rename(staging, published))
	snapMustDo(RepairWorkspaceSnapshot(fileGit{}, snapshot, published))
	return published, baseline
}

func (r *snapRepo) commit(text string) {
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "file"), []byte(text), 0o644))
	snapGit(r.checkout, "commit", "-am", "change")
}

func (r *snapRepo) conflict() {
	snapGit(r.checkout, "branch", "side")
	r.commit("main version\n")
	snapGit(r.checkout, "checkout", "side")
	r.commit("side version\n")
	snapGit(r.checkout, "checkout", "main")
}

func (r *snapRepo) startConflict(kind string) {
	output := snapMust(runGit(fileGit{}, r.checkout, []string{kind, "side"}, nil))
	if output.Status == 0 {
		panic(kind + " side unexpectedly succeeded")
	}
}

func addSubmodule(root *snapRepo, module *snapRepo) string {
	snapGit(root.checkout, "submodule", "add", module.origin, "sub")
	snapGit(root.checkout, "commit", "-am", "registered submodule")
	snapGit(root.checkout, "push", "origin", "main")
	return filepath.Join(root.checkout, "sub")
}

type checkoutState struct {
	head    SnapshotHead
	index   string
	tracked string
}

type snapState struct {
	refs      map[string]string
	stash     [][3]string
	checkouts map[string]checkoutState
}

func trackedFiles(path string) string {
	// Compare canonical tracked file data through a private index. Module
	// commit positions and missing/uninitialized directory placeholders
	// follow native update/checkout and are declared topology losses.
	links := snapMust(snapshotIndexGitlinks(fileGit{}, path))
	index := snapMust(privateIndex(fileGit{}, path))
	defer index.remove()
	env := [][2]string{{"GIT_INDEX_FILE", index.path}}
	run := func(arguments []string, stdin []byte) []byte {
		output := snapMust(fileGit{}.Run(path, &GitCommand{Arguments: arguments, Env: env, Stdin: stdin}))
		if output.Status != 0 {
			panic(checkpointLossy(output.Stderr))
		}
		return output.Stdout
	}
	run([]string{"add", "-u"}, nil)
	var normalized []byte
	for _, link := range links {
		topology := true
		metadata, err := os.Lstat(filepath.Join(path, link.Path))
		switch {
		case err == nil:
			topology = metadata.IsDir()
		case errors.Is(err, os.ErrNotExist):
		default:
			panic(err)
		}
		if topology {
			normalized = append(normalized, fmt.Sprintf("160000 %s\t%s\x00", link.Commit, snapMust(slashPath(link.Path)))...)
		}
	}
	if len(normalized) > 0 {
		run([]string{"update-index", "-z", "--index-info"}, normalized)
	}
	return strings.TrimRight(string(run([]string{"write-tree"}, nil)), "\n")
}

func snapshotState(path string, checkoutPaths []string) snapState {
	refs := map[string]string{}
	for _, line := range gitLines(snapGit(path, "for-each-ref", "--format=%(refname) %(objectname)")) {
		name, oid, _ := strings.Cut(line, " ")
		if name != "refs/stash" {
			refs[name] = oid
		}
	}
	stash := [][3]string{}
	for _, entry := range snapMust(readStash(fileGit{}, path)) {
		stash = append(stash, [3]string{
			snapGit(path, "rev-parse", entry.Commit+"^{tree}"),
			snapGit(path, "rev-parse", entry.Commit+"^2^{tree}"),
			entry.Message,
		})
	}
	checkouts := map[string]checkoutState{}
	for _, relative := range checkoutPaths {
		directory := gitJoin(path, relative)
		checkouts[relative] = checkoutState{
			head:    snapMust(readHead(fileGit{}, directory)),
			index:   snapGit(directory, "write-tree"),
			tracked: trackedFiles(directory),
		}
	}
	return snapState{refs: refs, stash: stash, checkouts: checkouts}
}

type rawSnapState struct {
	refs      []byte
	indexes   [][]byte
	stash     []SnapshotStashEntry
	fetchHead []byte
}

func readOptional(path string) []byte {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil
	}
	return data
}

func rawState(root string, paths []string) rawSnapState {
	state := rawSnapState{
		refs:  snapMust(runGit(fileGit{}, root, []string{"for-each-ref", "--format=%(refname) %(objectname) %(symref)"}, nil)).Stdout,
		stash: snapMust(readStash(fileGit{}, root)),
	}
	for _, path := range paths {
		state.indexes = append(state.indexes, readOptional(snapMust(gitPath(fileGit{}, gitJoin(root, path), "index"))))
	}
	state.fetchHead = readOptional(snapMust(gitPath(fileGit{}, root, "FETCH_HEAD")))
	return state
}

type snapGenerator uint64

func (g *snapGenerator) next() uint64 {
	*g ^= *g << 13
	*g ^= *g >> 7
	*g ^= *g << 17
	return uint64(*g)
}

func generatedCommands(root string, paths []string, seed uint64) {
	generator := snapGenerator(seed)
	for step := 0; step < 18; step++ {
		directory := gitJoin(root, paths[generator.next()%uint64(len(paths))])
		switch generator.next() % 11 {
		case 0, 1:
			snapMustDo(os.WriteFile(filepath.Join(directory, "file"), fmt.Appendf(nil, "seed %d step %d\nsecond\nthird\n", seed, step), 0o644))
			snapGit(directory, "add", "file")
			if step%2 == 0 {
				snapMustDo(os.WriteFile(filepath.Join(directory, "file"), fmt.Appendf(nil, "seed %d step %d\nsecond\nunstaged\n", seed, step), 0o644))
			}
		case 2:
			snapGit(directory, "add", "-u")
			snapGit(directory, "commit", "--allow-empty", "-m", fmt.Sprintf("generated %d", step))
		case 3:
			message := fmt.Sprintf("seed %d stash %d", seed, step)
			if step%3 == 0 {
				snapGit(directory, "stash", "push", "-u", "-m", message)
			} else {
				snapGit(directory, "stash", "push", "-m", message)
			}
		case 4:
			snapGit(directory, "tag", "-a", fmt.Sprintf("tag-%d", step), "-m", fmt.Sprintf("seed %d", seed))
		case 5:
			snapGit(directory, "branch", fmt.Sprintf("branch-%d", step))
		case 6:
			snapGit(directory, "notes", "add", "-f", "-m", fmt.Sprintf("note %d %d", seed, step))
		case 7:
			snapMustDo(os.WriteFile(filepath.Join(directory, fmt.Sprintf("untracked-%d", step)), []byte("declared loss\n"), 0o644))
		case 8:
			snapGit(directory, "push", "--force", "origin", "HEAD:refs/heads/published")
		case 9:
			name := fmt.Sprintf("added-%d", step)
			snapMustDo(os.WriteFile(filepath.Join(directory, name), fmt.Appendf(nil, "added %d %d\n", seed, step), 0o644))
			snapGit(directory, "add", name)
			snapMustDo(os.Chmod(filepath.Join(directory, name), 0o755))
		default:
			snapGit(directory, "rm", "-f", "--ignore-unmatch", "file")
		}
	}
	snapGit(root, "remote", "add", "discarded", "https://example.invalid/extra.git")
	snapGit(root, "config", "url.https://example.invalid/.insteadOf", "never-matches:")
	snapGit(root, "config", "remote.origin.pushurl", "https://example.invalid/publication.git")
	if seed%11 == 0 {
		snapGit(root, "config", "--unset-all", "branch.main.merge")
	}
}

// assertProjectedState compares a restored checkout with its source.
// Allowed differences are deliberately closed: untracked/ignored files and
// stash third parents never travel; raw launch-baseline refs/stashes do not
// travel; active raw operations omit tracked edits; symbolic ref aliases are
// materialized at their resolved OID. Initialized modules retain checkout
// gitlinks; native update selects the parent's pin. Module local refs,
// branch attachment, configuration other than origin/upstreams, extra
// remotes and rewrite rules are declared losses. Submodule update failure
// is a notice and an uninitialized path. Gitlink working positions and
// missing/uninitialized directory placeholders follow Git; regular-file
// data and the exact index remain compared. The provisioner contributes
// origin tracking refs, origin tags, and its initial branch only when that
// branch is absent from the projected source. No other extras pass.
func assertProjectedState(t *testing.T, source snapState, restored snapState, launch *LaunchRecord, baseline map[string]string) {
	t.Helper()
	maps.DeleteFunc(source.refs, func(name, _ string) bool { return strings.HasPrefix(name, "refs/remotes/") })
	if launch != nil {
		maps.DeleteFunc(source.refs, func(name, oid string) bool {
			head := false
			for _, tree := range source.checkouts {
				if tree.head.Kind != core.SnapshotHeadDetached && tree.head.Name == name {
					head = true
				}
			}
			launched, ok := launch.Refs[name]
			return !(head || !ok || launched != oid)
		})
		source.stash = slices.DeleteFunc(source.stash, func(entry [3]string) bool {
			return slices.ContainsFunc(launch.Stash, func(old SnapshotStashEntry) bool { return old.Message == entry[2] })
		})
	}
	actual := maps.Clone(restored.refs)
	for name, oid := range baseline {
		if _, ok := source.refs[name]; !ok &&
			(strings.HasPrefix(name, "refs/remotes/origin/") || strings.HasPrefix(name, "refs/tags/") || name == "refs/heads/main") {
			got, present := actual[name]
			if !present || got != oid {
				t.Fatalf("provisioner baseline %s: got %q (present %v), want %q", name, got, present, oid)
			}
			delete(actual, name)
		}
	}
	if !reflect.DeepEqual(actual, source.refs) {
		t.Fatalf("saved refs and all unexpected extras:\n got %v\nwant %v", actual, source.refs)
	}
	if !reflect.DeepEqual(restored.stash, source.stash) {
		t.Fatalf("stash trees, index trees and messages:\n got %v\nwant %v", restored.stash, source.stash)
	}
	if !reflect.DeepEqual(restored.checkouts, source.checkouts) {
		t.Fatalf("HEAD, index and tracked worktree:\n got %+v\nwant %+v", restored.checkouts, source.checkouts)
	}
}

func refusedAs[T SnapshotRefused](err error) (T, bool) {
	var refused SnapshotRefused
	var zero T
	if !errors.As(err, &refused) {
		return zero, false
	}
	typed, ok := refused.(T)
	return typed, ok
}

func TestSeededGitRoundTripAfterSourceDeletionAndRawImmutability(t *testing.T) {
	for seed := uint64(1); seed <= 32; seed++ {
		t.Logf("snapshot property seed=%d", seed)
		format := "sha1"
		if seed%8 == 0 {
			format = "sha256"
		}
		r := newSnapRepoWithFormat(t, format)
		snapGit(r.checkout, "fetch", "origin")
		snapGit(r.checkout, "branch", "user-before-launch")
		snapMustDo(os.WriteFile(filepath.Join(r.checkout, "file"), []byte("baseline stash\n"), 0o644))
		snapGit(r.checkout, "stash", "push", "-m", "baseline stash")
		snapMustDo(RecordLaunch(fileGit{}, r.checkout, "property"))
		launch := snapMust(ReadLaunchRecord(fileGit{}, r.checkout, "property"))
		if launch == nil {
			t.Fatal("launch record missing")
		}
		raw := seed%2 == 0
		ownership := CheckoutOwnershipManaged()
		if raw {
			copied := *launch
			ownership = CheckoutOwnershipRaw(&copied)
		}
		paths := []string{""}
		if seed%3 != 0 {
			relative := "lanes/nested lane"
			snapGit(r.checkout, "worktree", "add", "--detach", filepath.Join(r.checkout, relative))
			paths = append(paths, relative)
		}
		if seed%5 == 0 && len(paths) > 1 {
			relative := gitJoin(paths[1], "inner")
			snapGit(r.checkout, "worktree", "add", "--detach", filepath.Join(r.checkout, relative))
			paths = append(paths, relative)
		}
		generatedCommands(r.checkout, paths, seed)
		var overrideOrigin *string
		if seed%7 == 0 {
			snapGit(r.checkout, "remote", "remove", "origin")
			origin := r.origin
			overrideOrigin = &origin
		}
		expected := snapshotState(r.checkout, paths)
		originUpstream := snapMust(configValues(fileGit{}, r.checkout, "branch.main.merge"))
		invariant := rawState(r.checkout, paths)
		var snapshot WorkspaceSnapshot
		if raw || overrideOrigin != nil {
			untracked := UntrackedConsent{Kind: core.ConsentAny}
			if raw {
				untracked = UntrackedConsent{}
			}
			snapshot = snapMust(CaptureWorkspaceSnapshot(fileGit{}, r.checkout, ownership, filepath.Join(r.temp, "raw.pack"),
				CaptureConsent{Untracked: untracked}, overrideOrigin))
		} else {
			snapshot = r.capture(ownership)
		}
		if raw && !reflect.DeepEqual(rawState(r.checkout, paths), invariant) {
			t.Fatalf("seed %d: capture mutated raw state", seed)
		}
		encoded := snapMust(json.Marshal(snapshot))
		var decoded WorkspaceSnapshot
		snapMustDo(json.Unmarshal(encoded, &decoded))
		snapshot = decoded
		snapMustDo(os.RemoveAll(r.checkout))
		staging := r.provision("staging")
		if overrideOrigin != nil {
			snapGit(staging, "remote", "remove", "origin")
		}
		baseline := snapshotState(staging, nil).refs
		snapMust(RestoreWorkspaceSnapshot(fileGit{}, snapshot, staging))
		restored := filepath.Join(r.temp, "published")
		snapMustDo(os.Rename(staging, restored))
		snapMustDo(RepairWorkspaceSnapshot(fileGit{}, snapshot, restored))
		if remotes := snapGit(restored, "remote"); remotes != "origin" {
			t.Fatalf("seed %d: remotes %q", seed, remotes)
		}
		if got := snapMust(configValues(fileGit{}, restored, "branch.main.merge")); !reflect.DeepEqual(got, originUpstream) {
			t.Fatalf("seed %d: origin upstream must replace the provisioner's value: %q != %q", seed, got, originUpstream)
		}
		if len(snapMust(configValues(fileGit{}, restored, "remote.origin.pushurl"))) != 0 {
			t.Fatalf("seed %d: push URL survived", seed)
		}
		if status := snapMust(runGit(fileGit{}, restored, []string{"config", "--local", "--get-regexp", `^url\.`}, nil)).Status; status != 1 {
			t.Fatalf("seed %d: url rewrite survived (status %d)", seed, status)
		}
		var launchRecord *LaunchRecord
		if raw {
			launchRecord = launch
		}
		assertProjectedState(t, expected, snapshotState(restored, paths), launchRecord, baseline)
		for _, relative := range paths {
			for step := 0; step < 18; step++ {
				if checkpointExists(filepath.Join(gitJoin(restored, relative), fmt.Sprintf("untracked-%d", step))) {
					t.Fatalf("untracked file survived seed %d, step %d", seed, step)
				}
			}
		}
	}
}

func TestSeededFullyPushedSessionsHaveEmptyPacks(t *testing.T) {
	for seed := uint64(33); seed <= 40; seed++ {
		r := newSnapRepo(t)
		generatedCommands(r.checkout, []string{""}, seed)
		snapGit(r.checkout, "config", "--unset-all", "remote.origin.pushurl")
		snapGit(r.checkout, "add", "-u")
		snapGit(r.checkout, "commit", "--allow-empty", "-m", "publish tracked state")
		for index, entry := range snapMust(readStash(fileGit{}, r.checkout)) {
			snapGit(r.checkout, "tag", fmt.Sprintf("published-stash-%d", index), snapMust(trackedStash(fileGit{}, r.checkout, entry)).Commit)
		}
		snapGit(r.checkout, "push", "origin", "--all")
		if snapMust(runGit(fileGit{}, r.checkout, []string{"show-ref", "--verify", "refs/notes/commits"}, nil)).Status == 0 {
			snapGit(r.checkout, "tag", "published-notes", "refs/notes/commits")
		}
		snapGit(r.checkout, "push", "origin", "--tags")
		notes := snapMust(runGit(fileGit{}, r.checkout, []string{"show-ref", "--verify", "refs/notes/commits"}, nil))
		if notes.Status == 0 {
			snapGit(r.checkout, "push", "origin", "refs/notes/commits")
		}
		expected := snapshotState(r.checkout, []string{""})
		snapshot := r.capture(CheckoutOwnershipManaged())
		if snapshot.Pack != nil {
			t.Fatalf("fully pushed seed %d has a pack", seed)
		}
		snapMustDo(os.RemoveAll(r.checkout))
		restored, baseline := r.restoreWithBaseline(snapshot)
		assertProjectedState(t, expected, snapshotState(restored, []string{""}), nil, baseline)
	}
}

// hostGit is the system Git with empty global and system configuration.
type hostGit struct {
	global string
}

func (g hostGit) prepareCommand(command *GitCommand) *GitCommand {
	prepared := GitCommand{
		Arguments: command.Arguments,
		Stdin:     command.Stdin,
		Env: append(slices.Clone(command.Env),
			[2]string{"GIT_CONFIG_GLOBAL", g.global},
			[2]string{"GIT_CONFIG_SYSTEM", strings.TrimSuffix(g.global, filepath.Ext(g.global)) + ".system"},
			[2]string{"GIT_CONFIG_NOSYSTEM", "1"},
		),
	}
	return &prepared
}

func (g hostGit) Run(repository string, command *GitCommand) (GitOutput, error) {
	return SystemGit{}.Run(repository, g.prepareCommand(command))
}

func (g hostGit) RunToFile(repository string, command *GitCommand, output *os.File) (GitOutput, error) {
	return runToFile(SystemGit{}, repository, g.prepareCommand(command), output)
}

func newHostGit(path string, name string) hostGit {
	global := filepath.Join(path, name)
	snapMustDo(os.WriteFile(global, nil, 0o644))
	snapMustDo(os.WriteFile(strings.TrimSuffix(global, filepath.Ext(global))+".system", nil, 0o644))
	return hostGit{global: global}
}

func scopedItemPath(root string, checkout CheckoutKey, path string) UntrackedItem {
	identity := snapMust(FileIdentityOf(snapMust(os.Lstat(filepath.Join(root, path)))))
	return core.UntrackedItem{Kind: core.UntrackedItemPath, Checkout: checkout, Path: path, Identity: identity}
}

func itemStash(commit string, message string) UntrackedItem {
	return core.UntrackedItem{Kind: core.UntrackedItemStash, Checkout: core.DefaultCheckoutKey(), Commit: commit, Message: message}
}

// Hard-won: ffd652c27: reflog expiration hid the still-applicable root stash ref.
func TestExpiredRootStashRefSurvivesWithoutReflogEntries(t *testing.T) {
	root := newSnapRepo(t)
	snapMustDo(os.WriteFile(filepath.Join(root.checkout, "file"), []byte("stash retained by ref\n"), 0o644))
	snapGit(root.checkout, "stash", "push", "-m", "retained stash")
	stash := snapGit(root.checkout, "rev-parse", "refs/stash")
	snapGit(root.checkout, "reflog", "expire", "--expire=all", "refs/stash")
	if snapGit(root.checkout, "stash", "list") != "" {
		t.Fatal("stash list is not empty")
	}
	snapshot := root.capture(CheckoutOwnershipManaged())
	if snapshot.Metadata.Stash[0].Commit != stash {
		t.Fatalf("stash %s != %s", snapshot.Metadata.Stash[0].Commit, stash)
	}
	restored := root.restore(snapshot)
	snapGit(restored, "stash", "apply")
	if got := string(snapMust(os.ReadFile(filepath.Join(restored, "file")))); got != "stash retained by ref\n" {
		t.Fatalf("file %q", got)
	}
}

// Hard-won: 227e874ea: retained source/configuration regression.
func TestUnreachableOriginUsesStaleRemoteTrackingRefs(t *testing.T) {
	r := newSnapRepo(t)
	r.commit("unpushed\n")
	away := filepath.Join(r.temp, "away.git")
	snapMustDo(os.Rename(r.origin, away))
	snapshot := r.capture(CheckoutOwnershipManaged())
	if !snapshot.Metadata.RemoteRefsStale {
		t.Fatal("remote refs not stale")
	}
	snapMustDo(os.Mkdir(r.origin, 0o755))
	unresolvable := r.capture(CheckoutOwnershipManaged())
	if !unresolvable.Metadata.RemoteRefsStale {
		t.Fatal("unresolvable remote refs not stale")
	}
	if !slices.ContainsFunc(unresolvable.Metadata.Notices, func(notice string) bool {
		return strings.Contains(notice, "could not inspect local origin object storage")
	}) {
		t.Fatalf("notices %q", unresolvable.Metadata.Notices)
	}
	snapMustDo(os.Remove(r.origin))
	snapMustDo(os.Rename(away, r.origin))
	restored := r.restore(snapshot)
	if snapGit(restored, "rev-parse", "HEAD") != snapGit(r.checkout, "rev-parse", "HEAD") {
		t.Fatal("restored HEAD differs")
	}
}

// Hard-won: d7c636759: intent-to-add could fail capture after an already-approved managed abort.
func TestIntentToAddRefusesBeforeAbortingAManagedMerge(t *testing.T) {
	root := newSnapRepo(t)
	root.conflict()
	root.startConflict("merge")
	snapMustDo(os.WriteFile(filepath.Join(root.checkout, "draft"), []byte("intent payload\n"), 0o644))
	snapGit(root.checkout, "add", "-N", "draft")
	index := snapMust(os.ReadFile(filepath.Join(root.checkout, ".git/index")))
	merge := snapMust(os.ReadFile(filepath.Join(root.checkout, ".git/MERGE_HEAD")))
	_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused.pack"), anyConsent(), nil)
	if err == nil {
		t.Fatal("capture succeeded")
	}
	if _, ok := refusedAs[SnapshotRefusedIntentToAdd](err); !ok {
		t.Fatalf("%v", err)
	}
	for _, step := range []string{"draft", "git add", "git reset --"} {
		if !strings.Contains(err.Error(), step) {
			t.Fatalf("%q missing from %v", step, err)
		}
	}
	if !reflect.DeepEqual(snapMust(os.ReadFile(filepath.Join(root.checkout, ".git/index"))), index) {
		t.Fatal("index changed")
	}
	if !reflect.DeepEqual(snapMust(os.ReadFile(filepath.Join(root.checkout, ".git/MERGE_HEAD"))), merge) {
		t.Fatal("MERGE_HEAD changed")
	}
	if got := string(snapMust(os.ReadFile(filepath.Join(root.checkout, "draft")))); got != "intent payload\n" {
		t.Fatalf("draft %q", got)
	}
}

// Hard-won: 227e874ea: retained source/configuration regression.
func TestStashUntrackedParentRequiresConsentAndNeverRestores(t *testing.T) {
	r := newSnapRepo(t)
	ownership := CheckoutOwnershipManaged()
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "file"), []byte("tracked stash\n"), 0o644))
	state := uint32(0xabcdef01)
	scratch := make([]byte, 262_144)
	for index := range scratch {
		state ^= state << 13
		state ^= state >> 17
		state ^= state << 5
		scratch[index] = byte(state)
	}
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "secret"), scratch, 0o644))
	snapGit(r.checkout, "stash", "push", "-u", "-m", "mixed stash")
	original := snapMust(readStash(fileGit{}, r.checkout))
	estimate := snapMust(EstimateDelta(fileGit{}, r.checkout, ownership, nil))
	if *estimate.TotalBytes >= 65_536 {
		t.Fatalf("scratch must not inflate the real transfer pack: %d", *estimate.TotalBytes)
	}
	_, err := CaptureWorkspaceSnapshot(fileGit{}, r.checkout, ownership, filepath.Join(r.temp, "snapshot.pack"), CaptureConsent{}, nil)
	items, ok := refusedAs[SnapshotRefusedUntracked](err)
	if !ok || !reflect.DeepEqual([]UntrackedItem(items), []UntrackedItem{itemStash(original[0].Commit, original[0].Message)}) {
		t.Fatalf("%v", err)
	}
	snapshot := snapMust(CaptureWorkspaceSnapshot(fileGit{}, r.checkout, ownership, filepath.Join(r.temp, "snapshot.pack"), CaptureConsent{
		Untracked: UntrackedConsent{Kind: core.ConsentShown, Shown: []UntrackedItem{itemStash(original[0].Commit, original[0].Message)}},
	}, nil))
	packInfo := snapMust(os.Stat(*snapshot.Pack))
	if *estimate.TotalBytes != uint64(packInfo.Size()) {
		t.Fatalf("estimate %d != pack %d", *estimate.TotalBytes, packInfo.Size())
	}
	if snapshot.Metadata.Stash[0].Commit == original[0].Commit {
		t.Fatal("stash was not rebuilt")
	}
	if !reflect.DeepEqual(snapMust(readStash(fileGit{}, r.checkout)), original) {
		t.Fatal("source stash changed")
	}
	restored := r.restore(snapshot)
	if snapMust(stashHasUntracked(fileGit{}, restored, snapshot.Metadata.Stash[0].Commit)) {
		t.Fatal("restored stash has untracked parent")
	}
	missing := snapMust(runGit(fileGit{}, restored, []string{"cat-file", "-e", original[0].Commit}, nil))
	if missing.Status == 0 {
		t.Fatal("original untracked stash must not travel")
	}
	snapGit(restored, "stash", "apply")
	if got := string(snapMust(os.ReadFile(filepath.Join(restored, "file")))); got != "tracked stash\n" {
		t.Fatalf("file %q", got)
	}
	if checkpointExists(filepath.Join(restored, "secret")) {
		t.Fatal("secret restored")
	}
}

// Hard-won: 227e874ea: retained source/configuration regression.
func TestEmbeddedHTTPCredentialsNeverEnterSnapshot(t *testing.T) {
	r := newSnapRepo(t)
	credentialed := "http://token:secret@127.0.0.1:1/unreachable.git"
	snapGit(r.checkout, "remote", "set-url", "origin", credentialed)
	snapGit(r.checkout, "config", "remote.origin.pushurl", credentialed)
	snapshot := r.capture(CheckoutOwnershipManaged())
	if snapshot.Metadata.OriginURL == nil || *snapshot.Metadata.OriginURL != "http://127.0.0.1:1/unreachable.git" {
		t.Fatalf("origin %v", snapshot.Metadata.OriginURL)
	}
	encoded := string(snapMust(json.Marshal(snapshot)))
	if strings.Contains(encoded, "secret") || strings.Contains(encoded, "token") {
		t.Fatalf("credentials in %s", encoded)
	}
}

// Hard-won: 227e874ea: retained source/configuration regression.
func TestRawCaptureNeverPrunesUserRemoteTrackingRefs(t *testing.T) {
	r := newSnapRepo(t)
	snapGit(r.checkout, "branch", "deleted")
	snapGit(r.checkout, "push", "origin", "deleted")
	tracked := snapGit(r.checkout, "rev-parse", "refs/remotes/origin/deleted")
	snapGit(r.origin, "update-ref", "-d", "refs/heads/deleted")
	r.capture(CheckoutOwnershipRaw(nil))
	if snapGit(r.checkout, "rev-parse", "refs/remotes/origin/deleted") != tracked {
		t.Fatal("tracking ref changed")
	}
}

// Hard-won: 227e874ea: retained source/configuration regression.
func TestRawReachableOriginDoesNotExcludeForcePushedTrackingTip(t *testing.T) {
	r := newSnapRepo(t)
	r.commit("old published branch\n")
	snapGit(r.checkout, "push", "origin", "main")
	base := snapGit(r.checkout, "rev-parse", "HEAD~")
	snapGit(r.origin, "update-ref", "refs/heads/main", base)
	snapGit(r.origin, "gc", "--prune=now")
	snapshot := r.capture(CheckoutOwnershipRaw(nil))
	if snapshot.Pack == nil {
		t.Fatal("no pack")
	}
	restored := r.restore(snapshot)
	if snapGit(restored, "rev-parse", "HEAD") != snapGit(r.checkout, "rev-parse", "HEAD") {
		t.Fatal("restored HEAD differs")
	}
}

// Hard-won: 227e874ea: retained source/configuration regression.
func TestSessionConsentChecksAllRepositoriesBeforeAbortingAny(t *testing.T) {
	a := newSnapRepo(t)
	b := newSnapRepo(t)
	a.conflict()
	a.startConflict("merge")
	snapMustDo(os.WriteFile(filepath.Join(a.checkout, "notes.txt"), []byte("approved only in a"), 0o644))
	snapMustDo(os.WriteFile(filepath.Join(b.checkout, "notes.txt"), []byte("not approved in b"), 0o644))
	var repositories []RepositoryInput
	for _, entry := range []struct {
		id   string
		repo *snapRepo
	}{{"a", a}, {"b", b}} {
		repositories = append(repositories, RepositoryInput{
			ID:        entry.id,
			Checkout:  entry.repo.checkout,
			Ownership: CheckoutOwnershipManaged(),
			PackPath:  filepath.Join(entry.repo.temp, "snapshot.pack"),
		})
	}
	shownA := UntrackedConsent{Kind: core.ConsentShown, Shown: []UntrackedItem{scopedItemPath(a.checkout, repositoryKey("a"), "notes.txt")}}
	consent := SessionCaptureConsent{Repositories: map[string]CaptureConsent{
		"a": {Untracked: shownA, Operations: OperationConsent{Kind: core.ConsentAny}},
		"b": {Untracked: shownA, Operations: OperationConsent{Kind: core.ConsentAny}},
	}}
	_, err := CaptureSessionSnapshot(fileGit{}, repositories, consent)
	items, ok := refusedAs[SnapshotRefusedUntracked](err)
	if !ok || !reflect.DeepEqual([]UntrackedItem(items), []UntrackedItem{scopedItemPath(b.checkout, repositoryKey("b"), "notes.txt")}) {
		t.Fatalf("%v", err)
	}
	if !checkpointExists(filepath.Join(a.checkout, ".git/MERGE_HEAD")) {
		t.Fatal("merge was aborted")
	}
	c := newSnapRepo(t)
	c.commit("shallow blocker\n")
	snapGit(c.checkout, "push", "origin", "main")
	snapGit(c.checkout, "fetch", "--depth=1", "origin")
	repositories = append(repositories, RepositoryInput{
		ID:        "c",
		Checkout:  c.checkout,
		Ownership: CheckoutOwnershipManaged(),
		PackPath:  filepath.Join(c.temp, "blocked.pack"),
	})
	inventory := snapMust(SessionInventory(fileGit{}, repositories))
	if !slices.ContainsFunc(inventory["c"].Blockers, func(blocker InventoryBlocker) bool {
		_, ok := blocker.Kind.(SnapshotRefusedUnrepresentedGitState)
		return ok
	}) {
		t.Fatalf("blockers %+v", inventory["c"].Blockers)
	}
	_, err = CaptureSessionSnapshot(fileGit{}, repositories, SessionCaptureConsent{})
	state, ok := refusedAs[SnapshotRefusedUnrepresentedGitState](err)
	if !ok || !slices.ContainsFunc(state.Directories, func(row Scoped[string]) bool {
		return row.Checkout.RepositoryID == "c" && checkpointPathEndsWith(row.Value, "shallow")
	}) {
		t.Fatalf("%v", err)
	}
	if !checkpointExists(filepath.Join(a.checkout, ".git/MERGE_HEAD")) {
		t.Fatal("merge was aborted")
	}
}

// fileDirectoryURL is url::Url::from_directory_path(path).to_string().
func fileDirectoryURL(path string) string {
	return (&url.URL{Scheme: "file", Path: filepath.ToSlash(path) + "/"}).String()
}

// Hard-won: 3a4aef552: Git's configured pack splitting produced multiple snapshot artifacts.
func TestSnapshotAndEstimateOverrideConfiguredPackSplitting(t *testing.T) {
	root := newSnapRepo(t)
	snapGit(root.checkout, "config", "pack.packSizeLimit", "1m")
	state := uint64(0x34ab_9812_76ff_1229)
	for _, name := range []string{"a", "b", "c", "d"} {
		data := make([]byte, 600_000)
		for index := range data {
			state ^= state << 13
			state ^= state >> 7
			state ^= state << 17
			data[index] = byte(state)
		}
		snapMustDo(os.WriteFile(filepath.Join(root.checkout, name), data, 0o644))
	}
	snapGit(root.checkout, "add", "a", "b", "c", "d")
	snapGit(root.checkout, "commit", "-m", "incompressible delta")
	split := snapGit(root.checkout, "pack-objects", "--all", filepath.Join(root.temp, "split"))
	if len(gitLines(split)) <= 1 {
		t.Fatal("fixture did not exercise splitting")
	}
	estimate := snapMust(EstimateDelta(fileGit{}, root.checkout, CheckoutOwnershipManaged(), nil))
	snapshot := root.capture(CheckoutOwnershipManaged())
	if *estimate.TotalBytes != uint64(snapMust(os.Stat(*snapshot.Pack)).Size()) {
		t.Fatalf("estimate %d differs from pack", *estimate.TotalBytes)
	}
	restored := root.restore(snapshot)
	if snapGit(restored, "rev-parse", "HEAD^{tree}") != snapGit(root.checkout, "rev-parse", "HEAD^{tree}") {
		t.Fatal("restored tree differs")
	}
	snapGit(restored, "fsck", "--full")
}

// Hard-won: 227e874ea: retained source/configuration regression.
func TestIntegrationSnapshotFetchHonorsRepositorySSHCommand(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only")
	}
	root := newSnapRepo(t)
	staging := root.provision("staging")
	root.commit("published after the seed was cloned\n")
	snapGit(root.checkout, "push", "origin", "main")
	ssh := filepath.Join(root.temp, "ssh.sh")
	snapMustDo(os.WriteFile(ssh, fmt.Appendf(nil, "exec git-upload-pack '%s'\n", root.origin), 0o644))
	for _, checkout := range []string{root.checkout, staging} {
		snapGit(checkout, "config", "core.sshCommand", fmt.Sprintf("sh '%s'", ssh))
	}
	snapGit(root.checkout, "remote", "set-url", "origin", "ssh://snapshot.invalid/repo")
	snapshot := root.capture(CheckoutOwnershipManaged())
	if snapshot.Metadata.RemoteRefsStale {
		t.Fatal("remote refs stale")
	}
	if snapshot.Pack != nil {
		t.Fatal("unexpected pack")
	}
	notices := snapMust(RestoreWorkspaceSnapshot(fileGit{}, snapshot, staging))
	if len(notices) != 0 {
		t.Fatalf("notices %q", notices)
	}
	if snapGit(staging, "rev-parse", "HEAD") != snapGit(root.checkout, "rev-parse", "HEAD") {
		t.Fatal("restored HEAD differs")
	}
}

// Hard-won: 227e874ea: retained source/configuration regression.
func TestIntegrationReviewLegacyGitPreservesQuotedNestedWorktreePaths(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only")
	}
	root := newSnapRepo(t)
	relative := "lanes/tab\tquote\"slash\\café"
	nested := filepath.Join(root.checkout, relative)
	snapGit(root.checkout, "worktree", "add", "--detach", nested)
	snapMustDo(os.WriteFile(filepath.Join(nested, "file"), []byte("unpublished nested commit\n"), 0o644))
	snapGit(nested, "commit", "-am", "nested commit")
	snapMustDo(os.WriteFile(filepath.Join(nested, "file"), []byte("nested working edits\n"), 0o644))
	snapshot := snapMust(CaptureWorkspaceSnapshot(git231{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "quoted.pack"), anyConsent(), nil))
	if len(snapshot.Metadata.Worktrees) != 1 {
		t.Fatalf("worktrees %+v", snapshot.Metadata.Worktrees)
	}
	if !checkpointPathEqual(snapshot.Metadata.Worktrees[0].RelativePath, relative) {
		t.Fatalf("relative path %q", snapshot.Metadata.Worktrees[0].RelativePath)
	}
	restored := gitJoin(root.restore(snapshot), relative)
	if snapGit(restored, "rev-parse", "HEAD") != snapGit(nested, "rev-parse", "HEAD") {
		t.Fatal("nested HEAD differs")
	}
	if snapGit(restored, "diff") != snapGit(nested, "diff") {
		t.Fatal("nested diff differs")
	}
}

// Hard-won: 1d7cc60a7: consecutive identical stash OIDs did not create consecutive reflog entries.
func TestRestoreEdgeAdjacentStashEntriesPreserveEveryOIDAndMessage(t *testing.T) {
	root := newSnapRepo(t)
	snapMustDo(os.WriteFile(filepath.Join(root.checkout, "file"), []byte("stash A\n"), 0o644))
	a := snapGit(root.checkout, "stash", "create")
	snapMustDo(os.WriteFile(filepath.Join(root.checkout, "file"), []byte("stash B\n"), 0o644))
	b := snapGit(root.checkout, "stash", "create")
	snapGit(root.checkout, "stash", "store", "-m", "oldest A", a)
	for _, message := range []string{"middle A", "newest A"} {
		snapGit(root.checkout, "stash", "store", "-m", "temporary B", b)
		snapGit(root.checkout, "stash", "store", "-m", message, a)
		snapGit(root.checkout, "stash", "drop", "stash@{1}")
	}
	snapGit(root.checkout, "reset", "--hard", "HEAD")
	before := snapMust(readStash(fileGit{}, root.checkout))
	if len(before) != 3 {
		t.Fatalf("stash %+v", before)
	}
	for _, entry := range before {
		if entry.Commit != a {
			t.Fatalf("stash %+v", before)
		}
	}
	snapshot := root.capture(CheckoutOwnershipManaged())
	restored := root.restore(snapshot)
	if got := snapMust(readStash(fileGit{}, restored)); !reflect.DeepEqual(got, before) {
		t.Fatalf("restored stash %+v != %+v", got, before)
	}
}

// Hard-won: 1548f4aa4: Unicode whitespace was stripped from symbolic HEAD ref names.
func TestReviewEighthSymbolicHeadPreservesUnicodeWhitespace(t *testing.T) {
	root := newSnapRepo(t)
	snapGit(root.checkout, "branch", "topic")
	name := "topic "
	snapGit(root.checkout, "checkout", "-b", name)
	root.commit("distinct branch with Unicode whitespace\n")
	snapshot := root.capture(CheckoutOwnershipManaged())
	if want := (SnapshotHead{Kind: core.SnapshotHeadBranch, Name: "refs/heads/" + name, Commit: snapGit(root.checkout, "rev-parse", "HEAD")}); snapshot.Metadata.Head != want {
		t.Fatalf("head %+v != %+v", snapshot.Metadata.Head, want)
	}
	restored := root.restore(snapshot)
	if got := snapGit(restored, "symbolic-ref", "HEAD"); got != "refs/heads/"+name {
		t.Fatalf("symbolic HEAD %q", got)
	}
	if snapGit(restored, "rev-parse", "HEAD") != snapshot.Metadata.Head.Commit {
		t.Fatal("restored HEAD differs")
	}
	if snapGit(restored, "rev-parse", "topic") == snapshot.Metadata.Head.Commit {
		t.Fatal("topic moved")
	}
}

// Hard-won: 58c5ca099: name-only ITA subtraction missed tracked nonempty and empty HEAD paths.
func TestReviewTenthExistingHeadPathsWithIntentToAddRefuseBeforeAbort(t *testing.T) {
	for _, empty := range []bool{false, true} {
		root := newSnapRepo(t)
		if empty {
			snapMustDo(os.WriteFile(filepath.Join(root.checkout, "file"), nil, 0o644))
			snapGit(root.checkout, "commit", "-am", "empty HEAD path")
		}
		snapGit(root.checkout, "checkout", "-b", "side")
		snapMustDo(os.WriteFile(filepath.Join(root.checkout, "other"), []byte("side\n"), 0o644))
		snapGit(root.checkout, "add", "other")
		snapGit(root.checkout, "commit", "-m", "side path")
		snapGit(root.checkout, "checkout", "main")
		snapMustDo(os.WriteFile(filepath.Join(root.checkout, "other"), []byte("main\n"), 0o644))
		snapGit(root.checkout, "add", "other")
		snapGit(root.checkout, "commit", "-m", "main path")
		root.startConflict("merge")
		snapGit(root.checkout, "rm", "--cached", "file")
		snapGit(root.checkout, "add", "-N", "file")
		index := snapMust(os.ReadFile(filepath.Join(root.checkout, ".git/index")))
		inventory := snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipManaged()))
		if !slices.ContainsFunc(inventory.IntentToAdd, func(entry Scoped[string]) bool { return entry.Value == "file" }) {
			t.Fatalf("intent to add %+v", inventory.IntentToAdd)
		}
		_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused.pack"), anyConsent(), nil)
		if _, ok := refusedAs[SnapshotRefusedIntentToAdd](err); !ok {
			t.Fatalf("%v", err)
		}
		if !reflect.DeepEqual(snapMust(os.ReadFile(filepath.Join(root.checkout, ".git/index"))), index) {
			t.Fatal("index changed")
		}
		if !checkpointExists(filepath.Join(root.checkout, ".git/MERGE_HEAD")) {
			t.Fatal("merge aborted")
		}
	}
}

// Hard-won: 58c5ca099: primary-scope reads dropped upstreams defined by the branch's linked worktree.
func TestReviewTenthNestedBranchUpstreamsComeFromTheOwningWorktree(t *testing.T) {
	root := newSnapRepo(t)
	snapGit(root.checkout, "config", "extensions.worktreeConfig", "true")
	nested := filepath.Join(root.checkout, "lanes/upstream")
	snapGit(root.checkout, "worktree", "add", "-b", "lane", nested)
	snapGit(nested, "config", "--worktree", "branch.lane.remote", "origin")
	snapGit(nested, "config", "--worktree", "branch.lane.merge", "refs/heads/main")
	snapshot := root.capture(CheckoutOwnershipManaged())
	origin := "origin"
	if got := snapshot.Metadata.BranchTracking["lane"]; !reflect.DeepEqual(got, BranchTracking{Remote: &origin, Merge: []string{"refs/heads/main"}}) {
		t.Fatalf("tracking %+v", got)
	}
	restored := root.restore(snapshot)
	restoredLane := filepath.Join(restored, "lanes/upstream")
	if got := snapGit(restoredLane, "rev-parse", "--abbrev-ref", "@{upstream}"); got != "origin/main" {
		t.Fatalf("upstream %q", got)
	}
	snapGit(restoredLane, "pull", "--ff-only")
	if got := snapGit(restoredLane, "symbolic-ref", "HEAD"); got != "refs/heads/lane" {
		t.Fatalf("HEAD %q", got)
	}
}

// Hard-won: ecc543144: an origin pull mapping could contaminate its own ordinary tracking namespace.
func TestReviewEleventhOfflineOriginMixedSourcesKeepPullOnlyHistoryInPack(t *testing.T) {
	root := newSnapRepo(t)
	root.commit("pull-only unpublished branch history\n")
	tip := snapGit(root.checkout, "rev-parse", "HEAD")
	snapGit(root.checkout, "push", "origin", "HEAD:refs/pull/1/head")
	snapGit(root.checkout, "config", "--add", "remote.origin.fetch", "+refs/pull/1/head:refs/remotes/origin/pull-cache")
	snapGit(root.checkout, "fetch", "origin")
	evidence := snapMust(originTrackingObjects(fileGit{}, root.checkout, snapMust(readAllRefs(fileGit{}, root.checkout))))
	if !evidence.ambiguous {
		t.Fatal("evidence not ambiguous")
	}
	if slices.Contains(evidence.objects, tip) {
		t.Fatal("evidence contains the pull-only tip")
	}
	away := filepath.Join(root.temp, "offline.git")
	snapMustDo(os.Rename(root.origin, away))
	snapshot := root.capture(CheckoutOwnershipManaged())
	if snapshot.Pack == nil || !snapshot.Metadata.RemoteRefsStale {
		t.Fatalf("pack %v stale %v", snapshot.Pack, snapshot.Metadata.RemoteRefsStale)
	}
	snapMustDo(os.Rename(away, root.origin))
	snapGit(root.origin, "update-ref", "-d", "refs/pull/1/head")
	snapGit(root.origin, "reflog", "expire", "--expire=now", "--all")
	snapGit(root.origin, "gc", "--prune=now")
	if snapMust(runGit(fileGit{}, root.origin, []string{"cat-file", "-e", tip}, nil)).Status == 0 {
		t.Fatal("origin still has the tip")
	}
	snapMustDo(os.RemoveAll(root.checkout))
	restored := root.restore(snapshot)
	if snapGit(restored, "rev-parse", "HEAD") != tip {
		t.Fatal("restored HEAD differs")
	}
	if got := string(snapMust(os.ReadFile(filepath.Join(restored, "file")))); got != "pull-only unpublished branch history\n" {
		t.Fatalf("file %q", got)
	}
}

// Hard-won: e0cc04156: preserving inode and mtime let changed file sizes reuse discard consent.
func TestReviewTwelfthShownFileSizeChangesRefuseEvenWithPreservedMtime(t *testing.T) {
	for _, nested := range []bool{false, true} {
		root := newSnapRepo(t)
		var path string
		if nested {
			embedded := filepath.Join(root.checkout, "embedded")
			snapGit(root.temp, "clone", root.origin, embedded)
			path = filepath.Join(embedded, "file")
		} else {
			path = filepath.Join(root.checkout, "untracked")
			snapMustDo(os.WriteFile(path, []byte("before"), 0o644))
		}
		inventory := snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipManaged()))
		var shown []UntrackedItem
		for _, entry := range inventory.Untracked {
			shown = append(shown, core.UntrackedItem{Kind: core.UntrackedItemPath, Checkout: entry.Checkout, Path: entry.Path, Identity: entry.Identity})
		}
		before := snapMust(os.Stat(path))
		snapMustDo(os.Truncate(path, before.Size()+123))
		snapMustDo(os.Chtimes(path, time.Time{}, before.ModTime()))
		if !snapMust(os.Stat(path)).ModTime().Equal(before.ModTime()) {
			t.Fatal("modification time changed")
		}
		_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused.pack"), CaptureConsent{
			Untracked: UntrackedConsent{Kind: core.ConsentShown, Shown: shown},
		}, nil)
		if _, ok := refusedAs[SnapshotRefusedUntracked](err); !ok {
			t.Fatalf("nested %v: %v", nested, err)
		}
	}
}

func TestSeededPublishedSubmodulePositionsRoundTripAfterSourceDeletion(t *testing.T) {
	for seed := uint64(41); seed <= 48; seed++ {
		t.Logf("snapshot module property seed=%d", seed)
		r := newSnapRepo(t)
		module := newSnapRepo(t)
		directory := addSubmodule(r, module)
		modulePath := "sub"
		if seed == 44 {
			modulePath = "sub[0]"
			snapGit(r.checkout, "mv", "sub", modulePath)
			snapGit(r.checkout, "submodule", "add", module.origin, "sub0")
			snapGit(r.checkout, "commit", "-am", "literal module path and uninitialized sibling")
			snapGit(r.checkout, "push", "origin", "main")
			snapGit(r.checkout, "submodule", "deinit", "-f", "sub0")
			snapMustDo(os.RemoveAll(filepath.Join(r.checkout, ".git/modules/sub0")))
			directory = filepath.Join(r.checkout, modulePath)
		}
		old := snapGit(directory, "rev-parse", "HEAD")
		module.commit(fmt.Sprintf("published module %d\n", seed))
		snapGit(module.checkout, "push", "origin", "main")
		snapGit(directory, "fetch", "origin")
		updated := snapGit(module.checkout, "rev-parse", "HEAD")
		position := updated
		if seed%2 == 0 {
			position = old
		}
		snapGit(directory, "checkout", "--detach", position)
		if seed%3 == 0 {
			snapGit(r.checkout, "update-index", "--cacheinfo", fmt.Sprintf("160000,%s,%s", updated, modulePath))
		}
		generatedCommands(r.checkout, []string{""}, seed)
		paths := []string{"", modulePath}
		snapshot := r.capture(CheckoutOwnershipManaged())
		// Native update selects the parent's saved gitlink, not the module's
		// previous branch attachment or checkout position.
		savedPin := snapGit(r.checkout, "rev-parse", ":"+modulePath)
		snapGit(directory, "checkout", "--detach", savedPin)
		expected := snapshotState(r.checkout, paths)
		snapMustDo(os.RemoveAll(r.checkout))
		restored, baseline := r.restoreWithBaseline(snapshot)
		assertProjectedState(t, expected, snapshotState(restored, paths), nil, baseline)
		if strings.HasPrefix(snapGit(restored, "--literal-pathspecs", "submodule", "status", "--", modulePath), "-") {
			t.Fatalf("seed %d: module not initialized", seed)
		}
		if seed == 44 && checkpointExists(filepath.Join(restored, "sub0/.git")) {
			t.Fatal("literal path initialized an unrelated module")
		}
	}
}

func TestRawOperationSavesLiteralHeadAndRefsWithoutMutatingTheCheckout(t *testing.T) {
	for _, kind := range []string{"merge", "rebase", "cherry-pick"} {
		r := newSnapRepo(t)
		r.conflict()
		r.startConflict(kind)
		paths := []string{""}
		before := rawState(r.checkout, paths)
		head := snapMust(readHead(fileGit{}, r.checkout))
		refs := snapMust(readRefs(fileGit{}, r.checkout))
		snapshot := r.capture(CheckoutOwnershipRaw(nil))
		if !reflect.DeepEqual(rawState(r.checkout, paths), before) {
			t.Fatalf("%s: capture mutated raw state", kind)
		}
		if snapshot.Metadata.Head != head {
			t.Fatalf("%s: head %+v != %+v", kind, snapshot.Metadata.Head, head)
		}
		if !snapshot.Metadata.WorkingStateSkipped || snapshot.Metadata.WorkingState != nil {
			t.Fatalf("%s: working state %v skipped %v", kind, snapshot.Metadata.WorkingState, snapshot.Metadata.WorkingStateSkipped)
		}
		count := 0
		for _, notice := range snapshot.Metadata.Notices {
			if notice == rawNotice {
				count++
			}
		}
		if count != 1 {
			t.Fatalf("%s: raw notices %q", kind, snapshot.Metadata.Notices)
		}
		snapMustDo(os.RemoveAll(r.checkout))
		restored := r.restore(snapshot)
		if got := snapMust(readHead(fileGit{}, restored)); got != head {
			t.Fatalf("%s: restored head %+v", kind, got)
		}
		for name, oid := range snapshot.Metadata.Refs {
			if refs[name] != oid {
				t.Fatalf("%s: ref %s %s != %s", kind, name, refs[name], oid)
			}
		}
		if snapGit(restored, "status", "--porcelain") != "" {
			t.Fatalf("%s: restored checkout is dirty", kind)
		}
	}
	r := newSnapRepo(t)
	r.conflict()
	r.startConflict("merge")
	snapshot := r.capture(CheckoutOwnershipManaged())
	if _, ok, err := operation(fileGit{}, r.checkout); err != nil || ok {
		t.Fatalf("operation still in progress: %v", err)
	}
	expected := snapshotState(r.checkout, []string{""})
	snapMustDo(os.RemoveAll(r.checkout))
	restored, baseline := r.restoreWithBaseline(snapshot)
	assertProjectedState(t, expected, snapshotState(restored, []string{""}), nil, baseline)
}

func TestBoundaryPrerequisitesRecoverBySHAAndNameRewrittenHistory(t *testing.T) {
	for _, discard := range []bool{false, true} {
		r := newSnapRepo(t)
		r.commit("published boundary\n")
		snapGit(r.checkout, "push", "origin", "main")
		boundary := snapGit(r.checkout, "rev-parse", "HEAD")
		r.commit("local child\n")
		snapshot := r.capture(CheckoutOwnershipManaged())
		if !slices.Contains(snapshot.Metadata.Prerequisites, boundary) {
			t.Fatalf("prerequisites %q", snapshot.Metadata.Prerequisites)
		}
		base := snapGit(r.checkout, "rev-parse", "HEAD~2")
		snapGit(r.origin, "update-ref", "refs/heads/main", base)
		if discard {
			snapGit(r.origin, "reflog", "expire", "--expire=now", "--all")
			snapGit(r.origin, "gc", "--prune=now")
		}
		snapMustDo(os.RemoveAll(r.checkout))
		staging := r.provision("staging")
		_, err := RestoreWorkspaceSnapshot(git231{}, snapshot, staging)
		if discard {
			if err == nil {
				t.Fatal("restore succeeded")
			}
			message := err.Error()
			if !strings.Contains(message, boundary) || !strings.Contains(message, *snapshot.Metadata.OriginURL) || !strings.Contains(message, "origin may have rewritten history") {
				t.Fatal(message)
			}
		} else {
			if err != nil {
				t.Fatal(err)
			}
			if snapGit(staging, "rev-parse", "HEAD") != snapshot.Metadata.Head.Commit {
				t.Fatal("restored HEAD differs")
			}
		}
	}
}

func TestLegacyGitRepairsNestedWorktreesAfterTheStagingRename(t *testing.T) {
	r := newSnapRepo(t)
	relative := "lanes/legacy"
	snapGit(r.checkout, "worktree", "add", "--detach", filepath.Join(r.checkout, relative))
	snapshot := r.capture(CheckoutOwnershipManaged())
	snapMustDo(os.RemoveAll(r.checkout))
	staging := r.provision("staging")
	snapMust(RestoreWorkspaceSnapshot(git231{}, snapshot, staging))
	finalPath := filepath.Join(r.temp, "final")
	snapMustDo(os.Rename(staging, finalPath))
	snapMustDo(RepairWorkspaceSnapshot(git231{}, snapshot, finalPath))
	if snapMust(archiveCanonicalize(snapGit(filepath.Join(finalPath, relative), "rev-parse", "--show-toplevel"))) != snapMust(archiveCanonicalize(filepath.Join(finalPath, relative))) {
		t.Fatal("nested worktree was not repaired")
	}
}

// Hard-won: 227e874ea: retained Git fidelity regression.
func TestRawOriginRefreshFetchesObjectsWithoutChangingRefsOrFetchHead(t *testing.T) {
	r := newSnapRepo(t)
	snapMustDo(RecordLaunch(fileGit{}, r.checkout, "raw-refresh"))
	launch := snapMust(ReadLaunchRecord(fileGit{}, r.checkout, "raw-refresh"))
	before := snapMust(readAllRefs(fileGit{}, r.checkout))
	fetchHead := filepath.Join(r.checkout, ".git/FETCH_HEAD")
	snapMustDo(os.WriteFile(fetchHead, []byte("unchanged sentinel\n"), 0o644))
	publisher := filepath.Join(r.temp, "publisher")
	snapGit(r.temp, "clone", r.origin, publisher)
	snapMustDo(os.WriteFile(filepath.Join(publisher, "published"), []byte("published after raw launch\n"), 0o644))
	snapGit(publisher, "add", "published")
	snapGit(publisher, "commit", "-m", "published")
	snapGit(publisher, "push", "origin", "main")
	published := snapGit(publisher, "rev-parse", "HEAD")
	r.commit("raw session\n")
	expectedRefs := snapMust(readAllRefs(fileGit{}, r.checkout))
	snapshot := r.capture(CheckoutOwnershipRaw(launch))
	if !reflect.DeepEqual(snapMust(readAllRefs(fileGit{}, r.checkout)), expectedRefs) {
		t.Fatal("refs changed")
	}
	if got := string(snapMust(os.ReadFile(fetchHead))); got != "unchanged sentinel\n" {
		t.Fatalf("FETCH_HEAD %q", got)
	}
	for name := range snapshot.Metadata.Refs {
		if strings.HasPrefix(name, "refs/remotes/") {
			t.Fatalf("snapshot ref %s", name)
		}
	}
	if snapshot.Metadata.RemoteRefsStale {
		t.Fatal("remote refs stale")
	}
	snapGit(r.checkout, "cat-file", "-e", published)
	if before["refs/remotes/origin/main"] != expectedRefs["refs/remotes/origin/main"] {
		t.Fatal("tracking ref moved")
	}
	restored := r.restore(snapshot)
	if snapGit(restored, "rev-parse", "origin/main") != published {
		t.Fatal("restored origin/main differs")
	}
}

// Hard-won: a2932eae2: failed tip downloads reused force-pushed-away tracking refs.
func TestAdvertisedButUnservableTipsNeverUseStaleTrackingEvidence(t *testing.T) {
	r := newSnapRepo(t)
	unavailable := "1234567890123456789012345678901234567890"
	// ls-remote advertises a loose ref without reading its object;
	// upload-pack cannot serve it. This exercises a successful query and a
	// real fetch failure.
	snapMustDo(os.WriteFile(filepath.Join(r.origin, "refs/heads/main"), []byte(unavailable+"\n"), 0o644))
	if !strings.Contains(snapGit(r.checkout, "ls-remote", "origin"), unavailable) {
		t.Fatal("origin does not advertise the unavailable tip")
	}
	snapshot := r.capture(CheckoutOwnershipManaged())
	if snapshot.Metadata.RemoteRefsStale {
		t.Fatal("remote refs stale")
	}
	if snapshot.Pack == nil {
		t.Fatal("no pack")
	}
	// Restore receives a fresh provisioned clone, after origin is usable.
	snapGit(r.origin, "update-ref", "refs/heads/main", snapshot.Metadata.Head.Commit)
	restored := filepath.Join(r.temp, "fresh")
	snapMust(RestoreWorkspaceSnapshot(fileGit{}, snapshot, r.provision("fresh")))
	if snapGit(restored, "rev-parse", "HEAD") != snapshot.Metadata.Head.Commit {
		t.Fatal("restored HEAD differs")
	}
	snapGit(restored, "fsck", "--no-reflogs")

	root := newSnapRepo(t)
	module := newSnapRepo(t)
	directory := addSubmodule(root, module)
	snapMustDo(os.WriteFile(filepath.Join(module.origin, "refs/heads/main"), []byte(unavailable+"\n"), 0o644))
	if !strings.Contains(snapGit(directory, "ls-remote", "origin"), unavailable) {
		t.Fatal("module origin does not advertise the unavailable tip")
	}
	_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused.pack"), anyConsent(), nil)
	if _, ok := refusedAs[SnapshotRefusedDirtySubmodule](err); !ok {
		t.Fatalf("%v", err)
	}
}

// Hard-won: bae1ae4d3: custom origin refs excluded trees absent from a normal clone.
func TestCustomOriginRefsDoNotExcludeSharedTrees(t *testing.T) {
	r := newSnapRepo(t)
	r.commit("only in custom origin ref\n")
	snapGit(r.checkout, "push", "origin", "HEAD:refs/pull/1/head")
	snapGit(r.checkout, "commit", "--amend", "-m", "distinct commit sharing tree")
	fileOrigin := "file://" + r.origin
	snapGit(r.checkout, "remote", "set-url", "origin", fileOrigin)
	snapshot := r.capture(CheckoutOwnershipManaged())
	restored := r.restore(snapshot)
	if got := string(snapMust(os.ReadFile(filepath.Join(restored, "file")))); got != "only in custom origin ref\n" {
		t.Fatalf("file %q", got)
	}
	snapGit(restored, "fsck", "--no-reflogs")
}

// Hard-won: bae1ae4d3: raw stash filtering discarded a newly stored old object.
func TestRawNewStashMessageKeepsAReusedLaunchObject(t *testing.T) {
	r := newSnapRepo(t)
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "file"), []byte("old stash\n"), 0o644))
	snapGit(r.checkout, "stash", "push", "-m", "old")
	old := snapGit(r.checkout, "rev-parse", "refs/stash")
	snapMustDo(RecordLaunch(fileGit{}, r.checkout, "stash-session"))
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "file"), []byte("new stash\n"), 0o644))
	snapGit(r.checkout, "stash", "push", "-m", "new")
	snapGit(r.checkout, "stash", "store", "-m", "reused object", old)
	snapshot := r.capture(CheckoutOwnershipRaw(snapMust(ReadLaunchRecord(fileGit{}, r.checkout, "stash-session"))))
	if len(snapshot.Metadata.Stash) != 2 || snapshot.Metadata.Stash[0].Message != "reused object" {
		t.Fatalf("stash %+v", snapshot.Metadata.Stash)
	}
	restored := r.restore(snapshot)
	if got := snapMust(readStash(fileGit{}, restored)); !reflect.DeepEqual(got, snapshot.Metadata.Stash) {
		t.Fatalf("restored stash %+v", got)
	}
}

// Hard-won: d00d140bd: even fully published leftover state is not represented by a snapshot.
func TestStructuralGitStateRefusesPublishedLeftoversAndCleanModuleWorktreesAndWarnsRaw(t *testing.T) {
	root := newSnapRepo(t)
	module := newSnapRepo(t)
	addSubmodule(root, module)
	snapGit(root.checkout, "-c", "protocol.file.allow=always", "submodule", "add", module.origin, "retired")
	snapGit(root.checkout, "commit", "-am", "published retired module")
	snapGit(root.checkout, "push", "origin", "main")
	snapGit(root.checkout, "submodule", "deinit", "-f", "retired")
	leftover := filepath.Join(root.checkout, ".git/modules/retired")
	source := filepath.Join(root.checkout, "sub")
	linked := filepath.Join(source, "local-lane")
	snapGit(source, "worktree", "add", "--detach", linked, "HEAD")
	snapMustDo(os.WriteFile(snapMust(gitPath(fileGit{}, source, "info/exclude")), []byte("local-lane/\n"), 0o644))
	snapGit(source, "worktree", "lock", linked)
	outside := filepath.Join(root.temp, "outside-root-with-module")
	snapGit(root.checkout, "worktree", "add", "--detach", outside)
	snapGit(outside, "-c", "protocol.file.allow=always", "submodule", "update", "--init", "sub")
	_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "structure.pack"), anyConsent(), nil)
	if _, ok := refusedAs[SnapshotRefusedUnrepresentedGitState](err); !ok {
		t.Fatalf("%v", err)
	}
	message := err.Error()
	for _, path := range []string{leftover, linked, outside} {
		if !strings.Contains(message, path) {
			t.Fatalf("%s missing from %s", path, message)
		}
	}
	if !strings.Contains(message, "worktree remove --force") {
		t.Fatal(message)
	}
	if !strings.Contains(message, "locked") || !strings.Contains(message, "worktree unlock") || !strings.Contains(message, "once it is available") {
		t.Fatal(message)
	}
	if strings.Contains(message, "move it inside") {
		t.Fatal(message)
	}
	raw := root.capture(CheckoutOwnershipRaw(nil))
	if !slices.Contains(raw.Metadata.Notices, rawNotice) {
		t.Fatalf("notices %q", raw.Metadata.Notices)
	}
	if !checkpointExists(filepath.Join(linked, ".git")) || !checkpointExists(leftover) {
		t.Fatal("raw capture removed structural state")
	}
}

// Hard-won: 8251715a1: inactive submodule branches and stashes escaped snapshot loss checks.
func TestInactiveSubmoduleWorkRefusesManagedCaptureAndWarnsRaw(t *testing.T) {
	root := newSnapRepo(t)
	module := newSnapRepo(t)
	directory := addSubmodule(root, module)
	published := snapGit(directory, "rev-parse", "HEAD")
	snapGit(directory, "switch", "-c", "private")
	snapMustDo(os.WriteFile(filepath.Join(directory, "file"), []byte("inactive unpublished work\n"), 0o644))
	snapGit(directory, "commit", "-am", "private work")
	snapGit(directory, "checkout", "--detach", published)
	snapMustDo(os.WriteFile(filepath.Join(directory, "file"), []byte("stashed work\n"), 0o644))
	snapGit(directory, "stash", "push", "-m", "private stash")
	capture := func(ownership CheckoutOwnership) (WorkspaceSnapshot, error) {
		return CaptureWorkspaceSnapshot(fileGit{}, root.checkout, ownership, filepath.Join(root.temp, "work.pack"), anyConsent(), nil)
	}
	_, refused := capture(CheckoutOwnershipManaged())
	if _, ok := refusedAs[SnapshotRefusedDirtySubmodule](refused); !ok {
		t.Fatalf("%v", refused)
	}
	if !strings.Contains(refused.Error(), "submodule workspace::sub") {
		t.Fatal(refused)
	}
	raw := snapMust(capture(CheckoutOwnershipRaw(nil)))
	if !slices.Contains(raw.Metadata.Notices, rawNotice) {
		t.Fatalf("notices %q", raw.Metadata.Notices)
	}
	snapGit(directory, "push", "origin", "private")
	if _, err := capture(CheckoutOwnershipManaged()); err == nil {
		t.Fatal("capture with a module stash succeeded")
	}
	snapGit(directory, "stash", "drop")
	snapMust(capture(CheckoutOwnershipManaged()))
}

// Hard-won: 58c5ca099: an external shared origin lost its only objects during source teardown.
func TestReviewTenthSharedOriginAndRecursiveAlternatesMustBeIndependent(t *testing.T) {
	root := newSnapRepo(t)
	name := "checkout-owned"
	if runtime.GOOS != "windows" {
		name = "checkout\towned"
	}
	moved := filepath.Join(root.temp, name)
	snapMustDo(os.Rename(root.checkout, moved))
	root.checkout = moved
	root.commit("only source owns this commit\n")
	first := filepath.Join(root.temp, "shared-first.git")
	last := filepath.Join(root.temp, "shared-recursive.git")
	snapGit(root.temp, "clone", "--shared", "--bare", root.checkout, first)
	snapGit(root.temp, "clone", "--shared", "--bare", first, last)
	snapGit(root.checkout, "remote", "set-url", "origin", last)
	inventory := snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipManaged()))
	if !slices.ContainsFunc(inventory.OriginsRemovedByTeardown, func(origin Scoped[OriginDependency]) bool {
		return origin.Value.ObjectProvider != nil && checkpointPathStartsWith(*origin.Value.ObjectProvider, root.checkout)
	}) {
		t.Fatalf("origins %+v", inventory.OriginsRemovedByTeardown)
	}
	_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused.pack"), anyConsent(), nil)
	if _, ok := refusedAs[SnapshotRefusedOriginRemovedByTeardown](err); !ok {
		t.Fatalf("%v", err)
	}
	if !strings.Contains(err.Error(), "repack -a -d") || !strings.Contains(err.Error(), "objects/info/alternates") || !strings.Contains(err.Error(), last) {
		t.Fatal(err)
	}
	if len(snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipRaw(nil))).OriginsRemovedByTeardown) != 0 {
		t.Fatal("raw inventory reports teardown dependencies")
	}
	alternates := filepath.Join(first, "objects/info/alternates")
	original := snapMust(os.ReadFile(alternates))
	malformed := append(slices.Clone(original), []byte("\"unterminated\n")...)
	snapMustDo(os.WriteFile(alternates, malformed, 0o644))
	_, err = CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "invalid-provider.pack"), anyConsent(), nil)
	if err == nil {
		t.Fatal("capture succeeded")
	}
	message := err.Error()
	if !strings.Contains(message, "cannot inspect local origin object storage") || !strings.Contains(message, last) || !strings.Contains(message, "repair its objects/info/alternates") {
		t.Fatal(message)
	}
	snapMustDo(os.WriteFile(alternates, original, 0o644))
	snapGit(last, "repack", "-a", "-d")
	snapMustDo(os.Remove(filepath.Join(last, "objects/info/alternates")))
	if runtime.GOOS != "windows" {
		// Native provider names are canonical. The literal route can still
		// cross a deleted symlink even when its target survives.
		route := filepath.Join(root.checkout, "borrowed-objects")
		snapMustDo(os.Symlink(filepath.Join(root.origin, "objects"), route))
		snapMustDo(os.WriteFile(filepath.Join(last, "objects/info/alternates"), []byte(route+"\n"), 0o644))
		inventory := snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipManaged()))
		if !slices.ContainsFunc(inventory.OriginsRemovedByTeardown, func(origin Scoped[OriginDependency]) bool {
			return origin.Value.ObjectProvider != nil && checkpointPathEqual(*origin.Value.ObjectProvider, route)
		}) {
			t.Fatalf("origins %+v", inventory.OriginsRemovedByTeardown)
		}
		snapMustDo(os.Remove(filepath.Join(last, "objects/info/alternates")))
		snapMustDo(os.Remove(route))
	}
	snapshot := root.capture(CheckoutOwnershipManaged())
	if snapshot.Pack != nil {
		t.Fatal("unexpected pack")
	}
	saved := snapshot.Metadata.Head.Commit
	snapMustDo(os.RemoveAll(root.checkout))
	restored := root.restore(snapshot)
	if snapGit(restored, "rev-parse", "HEAD") != saved {
		t.Fatal("restored HEAD differs")
	}
	if got := string(snapMust(os.ReadFile(filepath.Join(restored, "file")))); got != "only source owns this commit\n" {
		t.Fatalf("file %q", got)
	}
}

func TestSeededSubmoduleFailuresAreNoticesAndConfigurationIsADeclaredLoss(t *testing.T) {
	for seed := uint64(49); seed <= 56; seed++ {
		t.Logf("snapshot notice property seed=%d", seed)
		r := newSnapRepo(t)
		module := newSnapRepo(t)
		addSubmodule(r, module)
		generatedCommands(r.checkout, []string{""}, seed)
		if seed%2 == 0 {
			// A root-owned gitlink can lie below an old initialized module.
			// Its path prefix must never move it to that module's index.
			pin := snapGit(module.checkout, "rev-parse", "HEAD")
			snapGit(r.checkout, "update-index", "--force-remove", "sub")
			snapGit(r.checkout, "update-index", "--add", "--cacheinfo", fmt.Sprintf("160000,%s,sub/child", pin))
		}
		expected := snapshotState(r.checkout, []string{""})
		snapshot := r.capture(CheckoutOwnershipManaged())
		snapMustDo(os.RemoveAll(r.checkout))
		snapMustDo(os.Rename(module.origin, filepath.Join(module.temp, "offline.git")))
		staging := r.provision("staging")
		baseline := snapshotState(staging, nil).refs
		notices := snapMust(RestoreWorkspaceSnapshot(fileGit{}, snapshot, staging))
		if !slices.ContainsFunc(notices, func(notice string) bool {
			return strings.Contains(notice, "submodule sub") && strings.Contains(notice, "not initialized")
		}) {
			t.Fatalf("seed %d: %q", seed, notices)
		}
		if snapGit(staging, "remote") != "origin" {
			t.Fatal("remotes differ")
		}
		if len(snapMust(configValues(fileGit{}, staging, "remote.origin.pushurl"))) != 0 {
			t.Fatal("push URL survived")
		}
		if snapMust(runGit(fileGit{}, staging, []string{"config", "--local", "--get-regexp", `^url\.`}, nil)).Status != 1 {
			t.Fatal("url rewrite survived")
		}
		assertProjectedState(t, expected, snapshotState(staging, []string{""}), nil, baseline)
	}
}

// Hard-won: 227e874ea: managed origins must survive the complete teardown.
func TestReviewFourteenthLocalShallowOriginsRefuseBeforeManagedAbort(t *testing.T) {
	for _, operation := range []bool{false, true} {
		root := newSnapRepo(t)
		root.commit("history below the shallow origin tip\n")
		snapGit(root.checkout, "push", "origin", "main")
		uri := fileDirectoryURL(root.origin)
		shallow := filepath.Join(root.temp, "shallow.git")
		snapGit(root.temp, "clone", "--bare", "--depth", "1", uri, shallow)
		snapGit(root.checkout, "remote", "set-url", "origin", shallow)
		if snapGit(root.checkout, "rev-parse", "--is-shallow-repository") != "false" {
			t.Fatal("source is shallow")
		}
		if operation {
			root.conflict()
			root.startConflict("merge")
		}
		index := snapMust(os.ReadFile(filepath.Join(root.checkout, ".git/index")))
		file := snapMust(os.ReadFile(filepath.Join(root.checkout, "file")))
		_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused.pack"), anyConsent(), nil)
		state, ok := refusedAs[SnapshotRefusedUnrepresentedGitState](err)
		if !ok || !slices.ContainsFunc(state.Directories, func(row Scoped[string]) bool { return checkpointPathEndsWith(row.Value, "shallow") }) {
			t.Fatalf("%v", err)
		}
		if !reflect.DeepEqual(snapMust(os.ReadFile(filepath.Join(root.checkout, ".git/index"))), index) {
			t.Fatal("index changed")
		}
		if !reflect.DeepEqual(snapMust(os.ReadFile(filepath.Join(root.checkout, "file"))), file) {
			t.Fatal("file changed")
		}
		if checkpointExists(filepath.Join(root.checkout, ".git/MERGE_HEAD")) != operation {
			t.Fatal("merge state changed")
		}
	}
	root := newSnapRepo(t)
	module := newSnapRepo(t)
	module.commit("module ancestry\n")
	snapGit(module.checkout, "push", "origin", "main")
	directory := addSubmodule(root, module)
	shallow := filepath.Join(module.temp, "shallow.git")
	uri := fileDirectoryURL(module.origin)
	snapGit(module.temp, "clone", "--bare", "--depth", "1", uri, shallow)
	snapGit(directory, "remote", "set-url", "origin", shallow)
	inventory := snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipManaged()))
	if !slices.ContainsFunc(inventory.Presence, func(row Scoped[string]) bool { return checkpointPathStartsWith(row.Value, shallow) }) {
		t.Fatalf("%+v", inventory)
	}
}

// Hard-won: 227e874ea: managed origins must survive the complete teardown.
func TestReviewSeventhUninitializedModuleMirrorsMustSurviveTeardown(t *testing.T) {
	root := newSnapRepo(t)
	module := newSnapRepo(t)
	addSubmodule(root, module)
	snapGit(root.checkout, "submodule", "deinit", "-f", "sub")
	snapMustDo(os.RemoveAll(filepath.Join(root.checkout, ".git/modules")))
	mirror := filepath.Join(root.checkout, "mirror.git")
	snapGit(root.temp, "clone", "--bare", module.origin, mirror)
	snapMustDo(os.WriteFile(filepath.Join(root.checkout, ".git/info/exclude"), []byte("mirror.git\n"), 0o644))
	host := newHostGit(root.temp, "module-routes")
	snapGit(root.temp, "config", "--file", host.global, fmt.Sprintf("url.%s.insteadOf", mirror), "module-mirror:")
	snapGit(root.checkout, "config", fmt.Sprintf("url.%s.insteadOf", module.origin), "module-mirror:")
	snapGit(root.checkout, "config", "-f", ".gitmodules", "submodule.sub.url", "module-mirror:")
	for _, changedPin := range []bool{false, true} {
		if changedPin {
			module.commit("new pin\n")
			snapGit(module.checkout, "push", mirror, "main")
			snapGit(root.checkout, "update-index", "--cacheinfo", fmt.Sprintf("160000,%s,sub", snapGit(module.checkout, "rev-parse", "HEAD")))
		}
		inventory := snapMust(WorkspaceInventoryOf(host, root.checkout, CheckoutOwnershipManaged()))
		if len(inventory.OriginsRemovedByTeardown) != 1 {
			t.Fatalf("origins %+v", inventory.OriginsRemovedByTeardown)
		}
		dependency := inventory.OriginsRemovedByTeardown[0]
		if dependency.Value.CanonicalPath != snapMust(archiveCanonicalize(mirror)) {
			t.Fatalf("canonical %q", dependency.Value.CanonicalPath)
		}
		if dependency.Value.ModuleURLKey == nil || *dependency.Value.ModuleURLKey != "submodule.sub.url" {
			t.Fatalf("module key %v", dependency.Value.ModuleURLKey)
		}
		_, err := CaptureWorkspaceSnapshot(host, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused.pack"), anyConsent(), nil)
		if _, ok := refusedAs[SnapshotRefusedOriginRemovedByTeardown](err); !ok {
			t.Fatalf("%v", err)
		}
		if !strings.Contains(err.Error(), "config -f .gitmodules") {
			t.Fatal(err)
		}
		if len(snapMust(WorkspaceInventoryOf(host, root.checkout, CheckoutOwnershipRaw(nil))).OriginsRemovedByTeardown) != 0 {
			t.Fatal("raw inventory reports teardown dependencies")
		}
	}
	// Updating .gitmodules to an independent origin removes the dependency.
	snapGit(root.checkout, "config", "-f", ".gitmodules", "submodule.sub.url", mirror)
	snapGit(root.checkout, "config", "-f", ".gitmodules", "submodule.sub.url", module.origin)
	if len(snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipManaged())).OriginsRemovedByTeardown) != 0 {
		t.Fatal("independent origin still reported")
	}
}

// Hard-won: 227e874ea: managed origins must survive the complete teardown.
func TestReviewTenthModuleOriginsCannotBorrowFromInitializedOrCachedAdmins(t *testing.T) {
	for _, initialized := range []bool{true, false} {
		root := newSnapRepo(t)
		module := newSnapRepo(t)
		directory := addSubmodule(root, module)
		mirror := filepath.Join(root.temp, "module-shared.git")
		snapGit(root.temp, "clone", "--shared", "--bare", directory, mirror)
		snapGit(directory, "remote", "set-url", "origin", mirror)
		if !initialized {
			snapGit(root.checkout, "config", "-f", ".gitmodules", "submodule.sub.url", mirror)
			snapGit(root.checkout, "add", ".gitmodules")
			snapGit(root.checkout, "submodule", "deinit", "-f", "sub")
		}
		inventory := snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipManaged()))
		expectedModules := snapMust(archiveCanonicalize(filepath.Join(root.checkout, ".git/modules")))
		if !slices.ContainsFunc(inventory.OriginsRemovedByTeardown, func(origin Scoped[OriginDependency]) bool {
			if origin.Value.ObjectProvider == nil {
				return false
			}
			provider := *origin.Value.ObjectProvider
			if canonical, err := archiveCanonicalize(provider); err == nil {
				provider = canonical
			}
			return checkpointPathStartsWith(provider, expectedModules)
		}) {
			t.Fatalf("initialized %v: origins %+v", initialized, inventory.OriginsRemovedByTeardown)
		}
		_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused.pack"), anyConsent(), nil)
		if _, ok := refusedAs[SnapshotRefusedOriginRemovedByTeardown](err); !ok {
			t.Fatalf("%v", err)
		}
		snapGit(mirror, "repack", "-a", "-d")
		snapMustDo(os.Remove(filepath.Join(mirror, "objects/info/alternates")))
		if !initialized {
			snapMustDo(os.RemoveAll(filepath.Join(root.checkout, ".git/modules")))
		}
		root.capture(CheckoutOwnershipManaged())
	}
}

// Hard-won: 227e874ea: managed origins must survive the complete teardown.
func TestReviewEdgeManagedOriginsMustSurviveAllSelectedTeardownRoots(t *testing.T) {
	root := newSnapRepo(t)
	mirror := filepath.Join(root.checkout, "hidden.git")
	snapMustDo(os.WriteFile(filepath.Join(root.checkout, ".git/info/exclude"), []byte("hidden.git\n"), 0o644))
	snapGit(root.temp, "clone", "--bare", root.origin, mirror)
	root.conflict()
	root.startConflict("merge")
	firstBlocker := func(checkout string) SnapshotRefused {
		inventory := snapMust(WorkspaceInventoryOf(fileGit{}, checkout, CheckoutOwnershipManaged()))
		if len(inventory.Blockers) == 0 {
			t.Fatal("no blockers")
		}
		return inventory.Blockers[0].Kind
	}
	for _, origin := range []string{mirror, fileDirectoryURL(mirror)} {
		snapGit(root.checkout, "remote", "set-url", "origin", origin)
		if _, ok := firstBlocker(root.checkout).(SnapshotRefusedOriginRemovedByTeardown); !ok {
			t.Fatalf("%s: blocker %v", origin, firstBlocker(root.checkout))
		}
		_, err := CaptureWorkspaceSnapshot(fileGit{}, root.checkout, CheckoutOwnershipManaged(), filepath.Join(root.temp, "refused"), anyConsent(), nil)
		if _, ok := refusedAs[SnapshotRefusedOriginRemovedByTeardown](err); !ok {
			t.Fatalf("%v", err)
		}
		if !strings.Contains(err.Error(), "git remote set-url origin <new path>") {
			t.Fatal(err)
		}
		if !checkpointExists(filepath.Join(root.checkout, ".git/MERGE_HEAD")) {
			t.Fatal("merge aborted")
		}
	}
	if runtime.GOOS != "windows" {
		alias := filepath.Join(root.temp, "outside-alias.git")
		snapMustDo(os.Symlink(mirror, alias))
		snapGit(root.checkout, "remote", "set-url", "origin", alias)
		if _, ok := firstBlocker(root.checkout).(SnapshotRefusedOriginRemovedByTeardown); !ok {
			t.Fatal("symlinked alias not refused")
		}
	}
	snapGit(root.checkout, "config", fmt.Sprintf("url.%s.insteadOf", mirror), "internal:")
	snapGit(root.checkout, "remote", "set-url", "origin", "internal:")
	if _, ok := firstBlocker(root.checkout).(SnapshotRefusedOriginRemovedByTeardown); !ok {
		t.Fatal("rewritten origin not refused")
	}
	if len(snapMust(WorkspaceInventoryOf(fileGit{}, root.checkout, CheckoutOwnershipRaw(nil))).OriginsRemovedByTeardown) != 0 {
		t.Fatal("raw inventory reports teardown dependencies")
	}

	parent := newSnapRepo(t)
	module := newSnapRepo(t)
	directory := addSubmodule(parent, module)
	moduleMirror := filepath.Join(parent.checkout, "module-mirror.git")
	snapMustDo(os.WriteFile(filepath.Join(parent.checkout, ".git/info/exclude"), []byte("module-mirror.git\n"), 0o644))
	snapGit(parent.temp, "clone", "--bare", module.origin, moduleMirror)
	snapGit(parent.checkout, "config", "submodule.sub.url", moduleMirror)
	snapGit(directory, "remote", "set-url", "origin", moduleMirror)
	inventory := snapMust(WorkspaceInventoryOf(fileGit{}, parent.checkout, CheckoutOwnershipManaged()))
	if len(inventory.OriginsRemovedByTeardown) == 0 || !checkpointPathEqual(inventory.OriginsRemovedByTeardown[0].Checkout.CheckoutPath, "sub") {
		t.Fatalf("origins %+v", inventory.OriginsRemovedByTeardown)
	}

	dependent := newSnapRepo(t)
	snapGit(dependent.checkout, "remote", "set-url", "origin", moduleMirror)
	var inputs []RepositoryInput
	for _, entry := range []struct {
		id   string
		repo *snapRepo
	}{{"dependent", dependent}, {"parent", parent}} {
		inputs = append(inputs, RepositoryInput{
			ID:        entry.id,
			Checkout:  entry.repo.checkout,
			Ownership: CheckoutOwnershipManaged(),
			PackPath:  filepath.Join(entry.repo.temp, "cross-root"),
		})
	}
	sessionInventory := snapMust(SessionInventory(fileGit{}, inputs))
	if blockers := sessionInventory["dependent"].Blockers; len(blockers) == 0 {
		t.Fatal("dependent has no blockers")
	} else if _, ok := blockers[0].Kind.(SnapshotRefusedOriginRemovedByTeardown); !ok {
		t.Fatalf("blocker %v", blockers[0].Kind)
	}
	_, err := CaptureSessionSnapshot(fileGit{}, inputs, SessionCaptureConsent{})
	if _, ok := refusedAs[SnapshotRefusedOriginRemovedByTeardown](err); !ok {
		t.Fatalf("%v", err)
	}
}

// Hard-won: 227e874ea: managed origins must survive the complete teardown.
func TestReviewNinthPostAbortRoutesUseTheCompleteSessionTeardownSet(t *testing.T) {
	first := newSnapRepo(t)
	second := newSnapRepo(t)
	module := newSnapRepo(t)
	addSubmodule(first, module)
	snapGit(first.checkout, "submodule", "deinit", "-f", "sub")
	snapMustDo(os.RemoveAll(filepath.Join(first.checkout, ".git/modules")))
	mirror := filepath.Join(second.checkout, "mirror.git")
	snapGit(second.temp, "clone", "--bare", module.origin, mirror)
	snapMustDo(os.WriteFile(filepath.Join(second.checkout, ".git/info/exclude"), []byte("mirror.git\n"), 0o644))
	snapGit(first.checkout, "config", "-f", ".gitmodules", "submodule.sub.url", mirror)
	snapGit(first.checkout, "commit", "-am", "unsafe module route")
	snapGit(first.checkout, "branch", "unsafe")
	snapGit(first.checkout, "config", "-f", ".gitmodules", "submodule.sub.url", module.origin)
	snapGit(first.checkout, "commit", "-am", "safe module route")
	first.commit("safe middle\n")
	first.commit("bad tip\n")
	snapGit(first.checkout, "checkout", "unsafe")
	snapGit(first.checkout, "bisect", "start", "main", "unsafe")
	if got := snapGit(first.checkout, "config", "-f", ".gitmodules", "submodule.sub.url"); got != module.origin {
		t.Fatalf("module url %q", got)
	}
	var inputs []RepositoryInput
	consent := SessionCaptureConsent{Repositories: map[string]CaptureConsent{}}
	for _, entry := range []struct {
		id   string
		repo *snapRepo
	}{{"first", first}, {"second", second}} {
		inputs = append(inputs, RepositoryInput{
			ID:        entry.id,
			Checkout:  entry.repo.checkout,
			Ownership: CheckoutOwnershipManaged(),
			PackPath:  filepath.Join(entry.repo.temp, "pending.pack"),
		})
		consent.Repositories[entry.id] = anyConsent()
	}
	_, err := CaptureSessionSnapshot(fileGit{}, inputs, consent)
	if _, ok := refusedAs[SnapshotRefusedOriginRemovedByTeardown](err); !ok {
		t.Fatalf("%v", err)
	}
	if !strings.Contains(err.Error(), mirror) {
		t.Fatal(err)
	}
	if checkpointExists(snapMust(gitPath(fileGit{}, first.checkout, "BISECT_START"))) {
		t.Fatal("bisect was not aborted")
	}
	for _, root := range []*snapRepo{first, second} {
		for _, entry := range snapMust(os.ReadDir(root.temp)) {
			if extension, ok := checkpointExtension(entry.Name()); ok && extension == "pack" {
				t.Fatalf("pending pack left behind: %s", entry.Name())
			}
		}
	}
}

// Hard-won: 3f919bd96: capture inherited the archival runner's lazy-promisor-fetch prohibition.
func TestIntegrationReviewPartialCloneHydratesRemovedOriginHistoryForPack(t *testing.T) {
	root := newSnapRepo(t)
	originalSource := root.checkout
	historical := strings.Repeat("promised historical content\n", 4000)
	snapMustDo(os.WriteFile(filepath.Join(root.checkout, "historical"), []byte(historical), 0o644))
	snapGit(root.checkout, "add", "historical")
	snapGit(root.checkout, "commit", "-m", "history retained only locally")
	snapGit(root.checkout, "push", "origin", "main")
	blob := snapGit(root.checkout, "rev-parse", "HEAD:historical")
	snapGit(root.origin, "config", "uploadpack.allowFilter", "true")
	snapGit(root.origin, "config", "uploadpack.allowAnySHA1InWant", "true")
	partial := filepath.Join(root.temp, "partial")
	snapGit(root.temp, "clone", "--filter=blob:none", "--no-checkout", fileDirectoryURL(root.origin), partial)
	snapGit(partial, "branch", "retained", "HEAD")
	snapGit(root.checkout, "checkout", "--orphan", "replacement")
	snapGit(root.checkout, "rm", "-rf", ".")
	snapMustDo(os.WriteFile(filepath.Join(root.checkout, "current"), []byte("new origin root\n"), 0o644))
	snapGit(root.checkout, "add", "current")
	snapGit(root.checkout, "commit", "-m", "replace origin history")
	snapGit(root.checkout, "push", "--force", "origin", "HEAD:main")
	snapGit(partial, "fetch", "origin")
	snapMust(gitText(newCaptureGit(fileGit{}), partial, []string{"checkout", "-B", "main", "origin/main"}))
	absent := snapMust(runGit(fileGit{}, partial, []string{"cat-file", "-e", blob}, nil))
	if absent.Status == 0 {
		t.Fatal("fixture unexpectedly hydrated the historical blob")
	}
	root.checkout = partial
	snapshot := root.capture(CheckoutOwnershipManaged())
	if snapshot.Pack == nil {
		t.Fatal("no pack")
	}
	snapMustDo(os.RemoveAll(root.checkout))
	snapMustDo(os.RemoveAll(originalSource))
	snapGit(root.origin, "reflog", "expire", "--expire=now", "--all")
	snapGit(root.origin, "gc", "--prune=now")
	restored := root.restore(snapshot)
	if got := snapGit(restored, "show", "retained:historical"); got != strings.TrimRight(historical, "\n") {
		t.Fatal("historical content differs")
	}
	snapGit(restored, "fsck", "--full")
}

func TestRawCaptureWithoutLaunchRecordKeepsAllLocalRefsAndStashes(t *testing.T) {
	r := newSnapRepo(t)
	snapGit(r.checkout, "branch", "extra-branch")
	snapGit(r.checkout, "tag", "v-local")
	snapGit(r.checkout, "notes", "add", "-m", "local note")
	head := snapGit(r.checkout, "rev-parse", "HEAD")
	snapGit(r.checkout, "update-ref", "refs/mj/durable-keep", head)
	r.commit("base tracked\n")
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "a.txt"), []byte("stashed change\n"), 0o644))
	snapGit(r.checkout, "add", "a.txt")
	snapGit(r.checkout, "stash", "push", "-m", "raw stash")

	snapshot := r.capture(CheckoutOwnershipRaw(nil))
	for _, name := range []string{"refs/heads/main", "refs/heads/extra-branch", "refs/tags/v-local", "refs/notes/commits", "refs/mj/durable-keep"} {
		if _, ok := snapshot.Metadata.Refs[name]; !ok {
			t.Fatalf("missing %s", name)
		}
	}
	if _, ok := snapshot.Metadata.Refs["refs/stash"]; ok {
		t.Fatal("refs/stash saved as a ref")
	}
	if len(snapshot.Metadata.Stash) != 1 {
		t.Fatalf("stash %+v", snapshot.Metadata.Stash)
	}
}

func TestRecordLaunchIfAbsentFromSubdirectoryIsIdempotentAndRemovable(t *testing.T) {
	r := newSnapRepo(t)
	sub := filepath.Join(r.checkout, "nested/sub")
	snapMustDo(os.MkdirAll(sub, 0o755))
	snapMustDo(RecordLaunchIfAbsent(SystemGit{}, sub, "session-a"))
	snapMustDo(RecordLaunchIfAbsent(SystemGit{}, sub, "session-b"))

	snapGit(r.checkout, "branch", "post-launch")
	snapMustDo(RecordLaunchIfAbsent(SystemGit{}, sub, "session-a"))

	a := snapMust(ReadLaunchRecord(SystemGit{}, r.checkout, "session-a"))
	if a == nil {
		t.Fatal("session-a launch record")
	}
	if _, ok := a.Refs["refs/heads/post-launch"]; ok {
		t.Fatal("launch record was rewritten")
	}

	snapMustDo(RemoveLaunchRecord(SystemGit{}, sub, "session-a"))
	if snapMust(ReadLaunchRecord(SystemGit{}, r.checkout, "session-a")) != nil {
		t.Fatal("session-a still recorded")
	}
	if snapMust(ReadLaunchRecord(SystemGit{}, r.checkout, "session-b")) == nil {
		t.Fatal("session-b removed")
	}
}

func TestHistoryCutoffCaptureAndRestoreWithoutOriginOrLaterHistory(t *testing.T) {
	r := newSnapRepo(t)
	cutoff := snapGit(r.checkout, "rev-parse", "HEAD")
	// Add a later commit to origin that must never leak into the cutoff
	// session.
	r.commit("later upstream commit\n")
	later := snapGit(r.checkout, "rev-parse", "HEAD")
	snapGit(r.checkout, "push", "origin", "main")

	// Provision a cutoff checkout with no origin and only history up to
	// cutoff.
	cutoffCheckout := filepath.Join(r.temp, "cutoff-checkout")
	snapMustDo(os.MkdirAll(cutoffCheckout, 0o755))
	snapGit(cutoffCheckout, "init", "-b", "main")
	snapGit(cutoffCheckout, "config", "user.name", "Test")
	snapGit(cutoffCheckout, "config", "user.email", "test@example.com")
	snapGit(cutoffCheckout, "fetch", r.origin, cutoff)
	snapGit(cutoffCheckout, "checkout", "-B", "main", "FETCH_HEAD")

	// Agent work beyond cutoff: commit, branch, stash, staged+unstaged
	// edits, and even a manually added origin remote pointing at the full
	// repo.
	snapGit(cutoffCheckout, "remote", "add", "origin", fileDirectoryURL(r.origin))
	snapGit(cutoffCheckout, "checkout", "-b", "work")
	snapMustDo(os.WriteFile(filepath.Join(cutoffCheckout, "a.txt"), []byte("cutoff work commit\n"), 0o644))
	snapGit(cutoffCheckout, "add", "a.txt")
	snapGit(cutoffCheckout, "commit", "-m", "cutoff work")
	workHead := snapGit(cutoffCheckout, "rev-parse", "HEAD")
	snapMustDo(os.WriteFile(filepath.Join(cutoffCheckout, "a.txt"), []byte("stashed change\n"), 0o644))
	snapGit(cutoffCheckout, "stash", "push", "-m", "cutoff stash")
	snapMustDo(os.WriteFile(filepath.Join(cutoffCheckout, "a.txt"), []byte("staged change\n"), 0o644))
	snapGit(cutoffCheckout, "add", "a.txt")
	snapMustDo(os.WriteFile(filepath.Join(cutoffCheckout, "a.txt"), []byte("unstaged change\n"), 0o644))

	input := RepositoryInput{
		ID:         "repo",
		Checkout:   cutoffCheckout,
		Ownership:  CheckoutOwnershipManaged(),
		PackPath:   filepath.Join(r.temp, "cutoff.pack"),
		Exclusions: ExclusionSource{Kind: ExclusionSourceCommits, Commits: []string{cutoff}},
	}
	snapshot := snapMust(CaptureSessionSnapshot(fileGit{}, []RepositoryInput{input}, SessionCaptureConsent{})).Repositories["repo"]

	if !snapshot.Metadata.HistoryCutoff || snapshot.Metadata.OriginURL != nil || len(snapshot.Metadata.BranchTracking) != 0 {
		t.Fatalf("metadata %+v", snapshot.Metadata)
	}
	if snapshot.Pack == nil || len(snapshot.Metadata.Stash) != 1 || snapshot.Metadata.WorkingState == nil {
		t.Fatalf("snapshot %+v", snapshot)
	}

	// Restore onto a fresh cutoff clone that has no origin remote.
	fresh := filepath.Join(r.temp, "cutoff-fresh")
	snapMustDo(os.MkdirAll(fresh, 0o755))
	snapGit(fresh, "init", "-b", "main")
	snapGit(fresh, "config", "user.name", "Test")
	snapGit(fresh, "config", "user.email", "test@example.com")
	snapGit(fresh, "fetch", r.origin, cutoff)
	snapGit(fresh, "checkout", "-B", "main", "FETCH_HEAD")

	notices := snapMust(RestoreWorkspaceSnapshot(fileGit{}, snapshot, fresh))
	if len(notices) != 0 {
		t.Fatalf("notices %q", notices)
	}
	if snapGit(fresh, "rev-parse", "HEAD") != workHead {
		t.Fatal("restored HEAD differs")
	}
	if snapGit(fresh, "branch", "--show-current") != "work" {
		t.Fatal("restored branch differs")
	}
	if got := string(snapMust(os.ReadFile(filepath.Join(fresh, "a.txt")))); got != "unstaged change\n" {
		t.Fatalf("a.txt %q", got)
	}
	if snapGit(fresh, "show", ":a.txt") != "staged change" {
		t.Fatal("staged change differs")
	}
	if len(gitLines(snapGit(fresh, "stash", "list"))) != 1 {
		t.Fatal("stash list differs")
	}
	// No origin remote was created and later upstream history was never
	// fetched.
	if snapMust(runGit(fileGit{}, fresh, []string{"remote", "get-url", "origin"}, nil)).Status == 0 {
		t.Fatal("origin was created")
	}
	if snapMust(runGit(fileGit{}, fresh, []string{"cat-file", "-e", later}, nil)).Status == 0 {
		t.Fatal("later history was fetched")
	}
}

func TestHistoryCutoffRestoreFailsVisiblyWhenPrerequisiteMissingWithoutFetching(t *testing.T) {
	r := newSnapRepo(t)
	cutoff := snapGit(r.checkout, "rev-parse", "HEAD")
	snapGit(r.checkout, "remote", "remove", "origin")
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "a.txt"), []byte("after cutoff\n"), 0o644))
	snapGit(r.checkout, "add", "a.txt")
	snapGit(r.checkout, "commit", "-m", "after cutoff")

	input := RepositoryInput{
		ID:         "repo",
		Checkout:   r.checkout,
		Ownership:  CheckoutOwnershipManaged(),
		PackPath:   filepath.Join(r.temp, "cutoff-missing-prereq.pack"),
		Exclusions: ExclusionSource{Kind: ExclusionSourceCommits, Commits: []string{cutoff}},
	}
	snapshot := snapMust(CaptureSessionSnapshot(fileGit{}, []RepositoryInput{input}, SessionCaptureConsent{})).Repositories["repo"]
	if !slices.Contains(snapshot.Metadata.Prerequisites, cutoff) {
		t.Fatalf("prerequisites %q", snapshot.Metadata.Prerequisites)
	}

	// Destination is an unrelated empty repo that DOES have origin pointing
	// to r.origin (which contains cutoff). Restore must still refuse to
	// fetch.
	dest := filepath.Join(r.temp, "dest-missing-prereq")
	snapMustDo(os.MkdirAll(dest, 0o755))
	snapGit(dest, "init", "-b", "main")
	snapGit(dest, "remote", "add", "origin", fileDirectoryURL(r.origin))

	_, err := RestoreWorkspaceSnapshot(fileGit{}, snapshot, dest)
	if err == nil || !strings.Contains(err.Error(), cutoff) || !strings.Contains(err.Error(), "refusing to fetch later history") {
		t.Fatalf("unexpected error: %v", err)
	}
	if snapMust(runGit(fileGit{}, dest, []string{"cat-file", "-e", cutoff}, nil)).Status == 0 {
		t.Fatal("restore must not fetch missing prerequisites")
	}
}

func TestHistoryCutoffRefusesInitializedSubmodulesBeforeTeardown(t *testing.T) {
	r := newSnapRepo(t)
	subOrigin := filepath.Join(r.temp, "sub.git")
	subSource := filepath.Join(r.temp, "sub-src")
	snapGit(r.temp, "init", "--bare", "-b", "main", subOrigin)
	snapGit(r.temp, "clone", subOrigin, subSource)
	snapMustDo(os.WriteFile(filepath.Join(subSource, "s.txt"), []byte("sub\n"), 0o644))
	snapGit(subSource, "add", "s.txt")
	snapGit(subSource, "commit", "-m", "sub base")
	snapGit(subSource, "push", "origin", "main")

	cutoff := snapGit(r.checkout, "rev-parse", "HEAD")
	snapGit(r.checkout, "remote", "remove", "origin")
	snapGit(r.checkout, "-c", "protocol.file.allow=always", "submodule", "add", subOrigin, "vendor/sub")
	snapGit(r.checkout, "commit", "-m", "add submodule")
	// Create a local unpushed commit inside the initialized submodule and
	// stage it.
	subPath := filepath.Join(r.checkout, "vendor/sub")
	snapMustDo(os.WriteFile(filepath.Join(subPath, "s.txt"), []byte("local sub commit\n"), 0o644))
	snapGit(subPath, "add", "s.txt")
	snapGit(subPath, "commit", "-m", "local submodule commit")
	snapGit(r.checkout, "add", "vendor/sub")

	input := RepositoryInput{
		ID:         "repo",
		Checkout:   r.checkout,
		Ownership:  CheckoutOwnershipManaged(),
		PackPath:   filepath.Join(r.temp, "cutoff-sub.pack"),
		Exclusions: ExclusionSource{Kind: ExclusionSourceCommits, Commits: []string{cutoff}},
	}
	_, err := CaptureSessionSnapshot(fileGit{}, []RepositoryInput{input}, SessionCaptureConsent{})
	if _, ok := refusedAs[SnapshotRefusedDirtySubmodule](err); !ok {
		t.Fatalf("expected DirtySubmodule refusal, got: %v", err)
	}
}

func TestConsentStalenessRefusesNewUntrackedOrKeepDirFileAfterCleanPreflight(t *testing.T) {
	r := newSnapRepo(t)
	keepDir := filepath.Join(r.checkout, ".mj/keep")
	input := RepositoryInput{
		ID:        "workspace",
		Checkout:  r.checkout,
		Ownership: CheckoutOwnershipManaged(),
		PackPath:  filepath.Join(r.temp, "stale.pack"),
		KeepDir:   &keepDir,
	}
	inputs := []RepositoryInput{input}
	// 1. Preflight sees a clean workspace (no untracked files, no keep-dir
	// files).
	inventory := snapMust(SessionInventory(fileGit{}, inputs))
	if len(inventory["workspace"].Untracked) != 0 {
		t.Fatalf("untracked %+v", inventory["workspace"].Untracked)
	}
	if entries, _, err := ScanKeepDir(inventory["workspace"].Checkout, keepDir); err != nil || len(entries) != 0 {
		t.Fatalf("keep entries %+v %v", entries, err)
	}

	// 2. A file appears before the capture cut. Capture with no untracked
	// consent refuses.
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "late-scratch.txt"), []byte("created after preflight\n"), 0o644))
	_, err := CaptureSessionSnapshot(fileGit{}, inputs, SessionCaptureConsent{})
	if _, ok := refusedAs[SnapshotRefusedUntracked](err); !ok {
		t.Fatalf("expected Untracked refusal, got: %v", err)
	}

	// 3. Even if the user approved late-scratch.txt via Shown, modifying it
	// or adding a keep-dir file after preflight refuses because the file
	// identity / keep-dir changed.
	inventory2 := snapMust(SessionInventory(fileGit{}, inputs))
	shownItem := core.UntrackedItem{Kind: core.UntrackedItemPath,
		Checkout: inventory2["workspace"].Checkout,
		Path:     inventory2["workspace"].Untracked[0].Path,
		Identity: inventory2["workspace"].Untracked[0].Identity,
	}
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "late-scratch.txt"), []byte("different content and size!\n"), 0o644))
	staleConsent := SessionCaptureConsent{Repositories: map[string]CaptureConsent{
		"workspace": {Untracked: UntrackedConsent{Kind: core.ConsentShown, Shown: []UntrackedItem{shownItem}}},
	}}
	_, err = CaptureSessionSnapshot(fileGit{}, inputs, staleConsent)
	if _, ok := refusedAs[SnapshotRefusedUntracked](err); !ok {
		t.Fatalf("expected Untracked refusal for changed identity, got: %v", err)
	}

	// 4. A file created in `.mj/keep/` after preflight is owned solely by
	// keep_dir scanning (never duplicated into inventory.untracked), and is
	// refused directly by CaptureSessionSnapshot as well as
	// CheckKeepDirConsent.
	snapMustDo(os.Remove(filepath.Join(r.checkout, "late-scratch.txt")))
	snapMustDo(os.MkdirAll(keepDir, 0o755))
	snapMustDo(os.WriteFile(filepath.Join(keepDir, "note.txt"), []byte("keep note\n"), 0o644))
	inventoryWithKeep := snapMust(SessionInventory(fileGit{}, inputs))
	if len(inventoryWithKeep["workspace"].Untracked) != 0 {
		t.Fatalf("keep_dir files must not be duplicated into inventory.untracked: %+v", inventoryWithKeep["workspace"].Untracked)
	}
	_, err = CaptureSessionSnapshot(fileGit{}, inputs, SessionCaptureConsent{})
	if _, ok := refusedAs[SnapshotRefusedUntracked](err); !ok {
		t.Fatalf("expected Untracked refusal from CaptureSessionSnapshot, got: %v", err)
	}
	err = CheckKeepDirConsent(inventory2["workspace"].Checkout, keepDir, UntrackedConsent{}, nil)
	if _, ok := refusedAs[SnapshotRefusedUntracked](err); !ok {
		t.Fatalf("expected Untracked refusal for keep-dir file, got: %v", err)
	}
	// Blanket discard consent covers the final inventory, and the resulting
	// manifest can report what was omitted after preflight.
	snapMustDo(os.WriteFile(filepath.Join(r.checkout, "late-scratch.txt"), []byte("new scratch\n"), 0o644))
	consent := SessionCaptureConsent{Repositories: map[string]CaptureConsent{
		"workspace": {Untracked: UntrackedConsent{Kind: core.ConsentAny}},
	}}
	snapshot := snapMust(CaptureSessionSnapshot(fileGit{}, inputs, consent))
	index := slices.IndexFunc(snapshot.Repositories["workspace"].Metadata.Notices, func(notice string) bool {
		return strings.HasPrefix(notice, DroppedUntrackedNoticePrefix)
	})
	if index < 0 {
		t.Fatalf("notices %q", snapshot.Repositories["workspace"].Metadata.Notices)
	}
	notice := snapshot.Repositories["workspace"].Metadata.Notices[index]
	if !strings.HasPrefix(notice, "Dropped untracked items: 2") {
		t.Fatal(notice)
	}
	if !strings.Contains(notice, "late-scratch.txt") || !strings.Contains(notice, ".mj/keep/note.txt") {
		t.Fatal(notice)
	}
}
