// Port of the tests in mj-import/src/project_picker.rs.

package mjimport

import (
	"encoding/json"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// pickerExecutor is a hand-written CommandExecutor whose Execute and
// CancellationRequested are functions, standing in for the Rust tests'
// one-off executor types.
type pickerExecutor struct {
	core.CommandExecutorDefaults
	execute   func(command *core.CommandSpec) (core.CommandOutput, error)
	cancelled func() bool
}

func (e *pickerExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.execute(command)
}

func (e *pickerExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *pickerExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *pickerExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *pickerExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e *pickerExecutor) CancellationRequested() bool {
	if e.cancelled == nil {
		return false
	}
	return e.cancelled()
}

func (e *pickerExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e *pickerExecutor) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

// fakeGh answers each command with the next queued output and records the
// commands it ran.
type fakeGh struct {
	pickerExecutor
	outputs  []fakeGhOutput
	commands []core.CommandSpec
}

type fakeGhOutput struct {
	output core.CommandOutput
	err    error
}

func newFakeGh(outputs ...fakeGhOutput) *fakeGh {
	gh := &fakeGh{outputs: outputs}
	gh.execute = func(command *core.CommandSpec) (core.CommandOutput, error) {
		gh.commands = append(gh.commands, *command)
		next := gh.outputs[0]
		gh.outputs = gh.outputs[1:]
		return next.output, next.err
	}
	return gh
}

func fakeGhJSON(body any, next bool) *fakeGh {
	headers := ""
	if next {
		headers = "Link: <https://api.github.com/user/repos?page=2>; rel=\"next\"\r\n"
	}
	encoded, err := json.Marshal(body)
	if err != nil {
		panic(err)
	}
	return newFakeGh(fakeGhOutput{output: core.CommandOutput{
		Status: 0,
		Stdout: []byte(fmt.Sprintf("HTTP/2.0 200 OK\r\n%s\r\n%s", headers, encoded)),
	}})
}

// noCommands refuses to spawn anything.
func noCommands() *pickerExecutor {
	return &pickerExecutor{execute: func(*core.CommandSpec) (core.CommandOutput, error) {
		panic("directory discovery must not spawn a command")
	}}
}

func pickerPathText(t *testing.T, path string) string {
	t.Helper()
	text, err := pathText(path)
	if err != nil {
		t.Fatal(err)
	}
	return text
}

func pickerCanonical(t *testing.T, path string) string {
	t.Helper()
	canonical, err := pickerCanonicalize(path)
	if err != nil {
		t.Fatal(err)
	}
	return canonical
}

func browse(t *testing.T, path string) core.ProjectDiscovery {
	t.Helper()
	answer, err := Discover(core.ProjectDiscoveryRequestDirectory{Path: pickerPathText(t, path)}, noCommands())
	if err != nil {
		t.Fatal(err)
	}
	return answer
}

func mustMkdirAll(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o755); err != nil {
		t.Fatal(err)
	}
}

func mustWriteFile(t *testing.T, path, content string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestBrowsingListsImmediateFoldersAndIdentifiesGitDirectoriesAndFiles(t *testing.T) {
	temporary := t.TempDir()
	root := filepath.Join(temporary, "projects with spaces")
	mustMkdirAll(t, filepath.Join(root, "zeta checkout", ".git"))
	mustMkdirAll(t, filepath.Join(root, "Alpha folder", "nested repository", ".git"))
	mustMkdirAll(t, filepath.Join(root, "beta worktree"))
	mustWriteFile(t, filepath.Join(root, "beta worktree", ".git"), "gitdir: elsewhere")
	mustWriteFile(t, filepath.Join(root, "regular file"), "not a folder")
	answer := browse(t, root)
	if answer.Directory == nil || *answer.Directory != pickerPathText(t, pickerCanonical(t, root)) {
		t.Fatalf("directory = %v", answer.Directory)
	}
	if answer.Parent == nil || *answer.Parent != pickerPathText(t, pickerCanonical(t, temporary)) {
		t.Fatalf("parent = %v", answer.Parent)
	}
	if answer.Truncated {
		t.Fatal("truncated")
	}
	type entry struct {
		name string
		kind core.ProjectEntryKind
	}
	var entries []entry
	for _, listed := range answer.Entries {
		entries = append(entries, entry{listed.Name, listed.Kind})
	}
	want := []entry{
		{"Alpha folder", core.ProjectEntryKindDirectory},
		{"beta worktree", core.ProjectEntryKindRepository},
		{"zeta checkout", core.ProjectEntryKindRepository},
	}
	if !slices.Equal(entries, want) {
		t.Fatalf("entries = %+v", entries)
	}
	if answer.Entries[1].Source != pickerPathText(t, filepath.Join(pickerCanonical(t, root), "beta worktree")) {
		t.Fatalf("source = %q", answer.Entries[1].Source)
	}
	if name := browse(t, answer.Entries[0].Source).Entries[0].Name; name != "nested repository" {
		t.Fatalf("nested = %q", name)
	}
}

func TestBrowsingSortsBeforeTruncationAndReservesTheCurrentRepository(t *testing.T) {
	root := t.TempDir()
	mustMkdirAll(t, filepath.Join(root, ".git"))
	for index := maxEntries; index >= 0; index-- {
		mustMkdirAll(t, filepath.Join(root, fmt.Sprintf("folder-%03d", index)))
	}
	answer := browse(t, root)
	if !answer.Truncated {
		t.Fatal("not truncated")
	}
	if len(answer.Entries) != maxEntries {
		t.Fatalf("entries = %d", len(answer.Entries))
	}
	if answer.Entries[1].Name != "folder-000" {
		t.Fatalf("first = %q", answer.Entries[1].Name)
	}
	if last := answer.Entries[len(answer.Entries)-1].Name; last != "folder-098" {
		t.Fatalf("last = %q", last)
	}
}

func TestFilteringReachesOmittedFoldersAndKeepsTheCurrentRepository(t *testing.T) {
	root := t.TempDir()
	mustMkdirAll(t, filepath.Join(root, ".git"))
	for index := range maxEntries {
		mustMkdirAll(t, filepath.Join(root, fmt.Sprintf("folder-%d", index)))
	}
	mustMkdirAll(t, filepath.Join(root, "ZZ matching project"))
	if !browse(t, root).Truncated {
		t.Fatal("not truncated")
	}
	request := core.ProjectDiscoveryRequestDirectory{Path: pickerPathText(t, root), Filter: "MATCHING"}
	answer, err := Discover(request, noCommands())
	if err != nil {
		t.Fatal(err)
	}
	if answer.Truncated {
		t.Fatal("truncated")
	}
	if len(answer.Entries) != 2 {
		t.Fatalf("entries = %+v", answer.Entries)
	}
	if !strings.HasPrefix(answer.Entries[0].Name, "Use ") {
		t.Fatalf("first = %q", answer.Entries[0].Name)
	}
	if answer.Entries[1].Name != "ZZ matching project" {
		t.Fatalf("second = %q", answer.Entries[1].Name)
	}
	legacy, err := core.UnmarshalProjectDiscoveryRequestJSON([]byte(`{"kind":"directory","path":"/work"}`))
	if err != nil {
		t.Fatal(err)
	}
	if legacy != (core.ProjectDiscoveryRequestDirectory{Path: "/work", Filter: ""}) {
		t.Fatalf("legacy = %#v", legacy)
	}
}

func TestDirectoryScanObservesTheCallersCancellation(t *testing.T) {
	checks := 0
	executor := &pickerExecutor{
		execute: func(*core.CommandSpec) (core.CommandOutput, error) { panic("unreachable") },
		cancelled: func() bool {
			current := checks
			checks++
			return current > 2
		},
	}
	root := t.TempDir()
	for index := range 10 {
		mustMkdirAll(t, filepath.Join(root, fmt.Sprint(index)))
	}
	_, err := Discover(core.ProjectDiscoveryRequestDirectory{Path: pickerPathText(t, root)}, executor)
	if err == nil || !strings.Contains(err.Error(), "cancelled or timed out") {
		t.Fatalf("error = %v", err)
	}
}

func pickerHasArgs(command core.CommandSpec, first, second string) bool {
	for index := 0; index+1 < len(command.Args); index++ {
		if command.Args[index] == first && command.Args[index+1] == second {
			return true
		}
	}
	return false
}

func TestGithubSearchPassesQueriesLiterallyAndReportsMoreOrIncompleteResults(t *testing.T) {
	for _, row := range []struct {
		totalCount        int
		incompleteResults bool
		truncated         bool
	}{{1, false, false}, {101, false, true}, {1, true, true}} {
		executor := fakeGhJSON(map[string]any{
			"total_count": row.totalCount, "incomplete_results": row.incompleteResults,
			"items": []any{map[string]any{"full_name": "team/private", "description": nil, "private": true}},
		}, false)
		query := "@private {owner} in:name org:team --jq secret"
		answer, err := Discover(core.ProjectDiscoveryRequestGithub{Query: query}, executor)
		if err != nil {
			t.Fatal(err)
		}
		if answer.Entries[0].Source != "team/private" {
			t.Fatalf("source = %q", answer.Entries[0].Source)
		}
		if answer.Truncated != row.truncated {
			t.Fatalf("truncated = %v for %+v", answer.Truncated, row)
		}
		command := executor.commands[0]
		if !slices.Contains(command.Args, "search/repositories") {
			t.Fatalf("args = %q", command.Args)
		}
		if !pickerHasArgs(command, "--raw-field", "q="+query+" fork:true") {
			t.Fatalf("args = %q", command.Args)
		}
	}
}

func searchOutput(names ...string) fakeGhOutput {
	items := []any{}
	for _, name := range names {
		items = append(items, map[string]any{"full_name": name, "description": nil})
	}
	encoded, err := json.Marshal(map[string]any{"total_count": len(names), "incomplete_results": false, "items": items})
	if err != nil {
		panic(err)
	}
	return fakeGhOutput{output: core.CommandOutput{Status: 0, Stdout: []byte("HTTP/2.0 200 OK\r\n\r\n" + string(encoded))}}
}

func TestOwnerScopedGithubQueriesSearchForksAndPromoteExactMatch(t *testing.T) {
	for _, row := range []struct{ query, expected, first string }{
		{"jbellis/sessionwiki", "user:jbellis sessionwiki in:name fork:true", "JBELLIS/SESSIONWIKI"},
		{"jbellis/session", "user:jbellis session in:name fork:true", "jbellis/sessionwiki-copy"},
		{"jbellis/", "user:jbellis fork:true", "jbellis/sessionwiki-copy"},
		{"https://github.com/jbellis/sessionwiki.git", "user:jbellis sessionwiki in:name fork:true", "JBELLIS/SESSIONWIKI"},
	} {
		executor := fakeGhJSON(map[string]any{
			"total_count": 2, "incomplete_results": false,
			"items": []any{
				map[string]any{"full_name": "jbellis/sessionwiki-copy", "description": nil},
				map[string]any{"full_name": "JBELLIS/SESSIONWIKI", "description": nil},
			},
		}, false)
		answer, err := Discover(core.ProjectDiscoveryRequestGithub{Query: row.query}, executor)
		if err != nil {
			t.Fatal(err)
		}
		if answer.Entries[0].Source != row.first {
			t.Fatalf("%s: first = %q", row.query, answer.Entries[0].Source)
		}
		if !pickerHasArgs(executor.commands[0], "--raw-field", "q="+row.expected) {
			t.Fatalf("%s: args = %q", row.query, executor.commands[0].Args)
		}
	}

	missingOwner := newFakeGh(fakeGhOutput{output: core.CommandOutput{
		Status: 1,
		Stderr: []byte("gh: Validation Failed (HTTP 422)"),
	}})
	answer, err := Discover(core.ProjectDiscoveryRequestGithub{Query: "missing-owner/sessionwiki"}, missingOwner)
	if err != nil {
		t.Fatal(err)
	}
	if len(answer.Entries) != 0 || answer.Truncated {
		t.Fatalf("answer = %+v", answer)
	}
}

func TestBareOwnerMergesSearchesAndHandlesOwnerLookupOutcomes(t *testing.T) {
	executor := newFakeGh(searchOutput("jbellis/a"), searchOutput("JBELLIS/A", "other/b"))
	answer, err := Discover(core.ProjectDiscoveryRequestGithub{Query: "jbellis"}, executor)
	if err != nil {
		t.Fatal(err)
	}
	var sources []string
	for _, entry := range answer.Entries {
		sources = append(sources, entry.Source)
	}
	if !slices.Equal(sources, []string{"jbellis/a", "other/b"}) {
		t.Fatalf("sources = %q", sources)
	}
	if len(executor.commands) != 2 {
		t.Fatalf("commands = %d", len(executor.commands))
	}

	// Non-user single word: synthesized owner 422 falls through to general
	// search.
	notAUser := newFakeGh(
		fakeGhOutput{output: core.CommandOutput{Status: 1, Stderr: []byte("gh: Validation Failed (HTTP 422)")}},
		searchOutput("jbellis/sessionwiki"),
	)
	answer, err = Discover(core.ProjectDiscoveryRequestGithub{Query: "sessionwiki"}, notAUser)
	if err != nil {
		t.Fatal(err)
	}
	if len(answer.Entries) != 1 || answer.Entries[0].Source != "jbellis/sessionwiki" {
		t.Fatalf("entries = %+v", answer.Entries)
	}
	if len(notAUser.commands) != 2 {
		t.Fatalf("commands = %d", len(notAUser.commands))
	}

	// Non-422 errors (e.g. rate limit / auth) propagate without running
	// general search.
	authError := newFakeGh(fakeGhOutput{output: core.CommandOutput{Status: 1, Stderr: []byte("Bad credentials (HTTP 401)")}})
	_, err = Discover(core.ProjectDiscoveryRequestGithub{Query: "jbellis"}, authError)
	if err == nil || !strings.Contains(err.Error(), "gh auth login") {
		t.Fatalf("error = %v", err)
	}
	if len(authError.commands) != 1 {
		t.Fatalf("commands = %d", len(authError.commands))
	}

	// Full page of owner matches skips the general search and marks
	// truncated.
	var fullPage []string
	for index := range maxEntries {
		fullPage = append(fullPage, fmt.Sprintf("jbellis/repo-%d", index))
	}
	fullOwner := newFakeGh(searchOutput(fullPage...))
	answer, err = Discover(core.ProjectDiscoveryRequestGithub{Query: "jbellis"}, fullOwner)
	if err != nil {
		t.Fatal(err)
	}
	if len(answer.Entries) != maxEntries || !answer.Truncated {
		t.Fatalf("entries = %d truncated = %v", len(answer.Entries), answer.Truncated)
	}
	if len(fullOwner.commands) != 1 {
		t.Fatalf("commands = %d", len(fullOwner.commands))
	}
}

func TestExplicitForkQualifiersAreNotDuplicated(t *testing.T) {
	for _, query := range []string{"foo fork:false", "foo -fork:only"} {
		executor := fakeGhJSON(map[string]any{"total_count": 0, "incomplete_results": false, "items": []any{}}, false)
		if _, err := Discover(core.ProjectDiscoveryRequestGithub{Query: query}, executor); err != nil {
			t.Fatal(err)
		}
		if !slices.Contains(executor.commands[0].Args, "q="+query) {
			t.Fatalf("args = %q", executor.commands[0].Args)
		}
	}
}

func TestGithubUsesPaginationHeadersAndCapsTheReturnedEntries(t *testing.T) {
	for _, row := range []struct {
		count     int
		next      bool
		truncated bool
	}{{100, false, false}, {100, true, true}, {101, false, true}} {
		repos := []any{}
		for index := range row.count {
			repos = append(repos, map[string]any{"full_name": fmt.Sprintf("owner/repo-%d", index), "description": nil})
		}
		executor := fakeGhJSON(repos, row.next)
		answer, err := Discover(core.ProjectDiscoveryRequestGithub{Query: ""}, executor)
		if err != nil {
			t.Fatal(err)
		}
		if len(answer.Entries) != min(row.count, maxEntries) {
			t.Fatalf("entries = %d", len(answer.Entries))
		}
		if answer.Truncated != row.truncated {
			t.Fatalf("truncated = %v for %+v", answer.Truncated, row)
		}
	}
}

func TestGithubFailuresAreActionableAndNeverExposeSubprocessOutput(t *testing.T) {
	request := core.ProjectDiscoveryRequestGithub{Query: ""}
	for _, row := range []struct {
		status   int
		stderr   string
		expected string
	}{
		{4, "gh auth login secret-token", "gh auth login"},
		{1, "Bad credentials (HTTP 401) secret-token", "gh auth login"},
		{1, "failed connection https://secret-token@example.invalid", "network connection"},
		{1, "API rate limit exceeded secret-token", "rate limit"},
		{1, "HTTP 403 secret-token", "permissions"},
		{1, "HTTP 422 secret-token", "qualifiers"},
	} {
		executor := newFakeGh(fakeGhOutput{output: core.CommandOutput{
			Status: row.status,
			Stdout: []byte("secret-token"),
			Stderr: []byte(row.stderr),
		}})
		_, err := Discover(request, executor)
		if err == nil || !strings.Contains(err.Error(), row.expected) {
			t.Fatalf("error = %v", err)
		}
		if strings.Contains(fmt.Sprintf("%#v", err), "secret-token") {
			t.Fatalf("debug error leaks: %#v", err)
		}
		if strings.Contains(ErrorMessage(err), "secret-token") {
			t.Fatalf("message leaks: %s", ErrorMessage(err))
		}
	}
	executor := newFakeGh(fakeGhOutput{err: &fs.PathError{Op: "exec", Path: "secret-token", Err: fs.ErrNotExist}})
	_, err := Discover(request, executor)
	if err == nil || !strings.Contains(err.Error(), "Install it") {
		t.Fatalf("error = %v", err)
	}
	if strings.Contains(fmt.Sprintf("%#v", err), "secret-token") {
		t.Fatalf("debug error leaks: %#v", err)
	}
	executor = newFakeGh(fakeGhOutput{output: core.CommandOutput{Status: 0, Stdout: []byte("secret-token")}})
	if _, err := Discover(request, executor); err == nil || !strings.Contains(err.Error(), "unreadable") {
		t.Fatalf("error = %v", err)
	}
	if message := ErrorMessage(fmt.Errorf("secret-token")); message != "Project discovery failed. Retry the request." {
		t.Fatalf("message = %q", message)
	}
}

func TestWireContractRoundTripsAndRejectsUnboundedInputBeforeWork(t *testing.T) {
	request, err := core.UnmarshalProjectDiscoveryRequestJSON([]byte(`{"kind":"github","query":"private org:team"}`))
	if err != nil {
		t.Fatal(err)
	}
	if request != (core.ProjectDiscoveryRequestGithub{Query: "private org:team"}) {
		t.Fatalf("request = %#v", request)
	}
	for kind, want := range map[core.ProjectEntryKind]string{
		core.ProjectEntryKindRepository: `"repository"`,
		core.ProjectEntryKindDirectory:  `"directory"`,
	} {
		encoded, err := json.Marshal(kind)
		if err != nil || string(encoded) != want {
			t.Fatalf("%s = %s, %v", kind, encoded, err)
		}
	}
	for _, input := range []string{strings.Repeat("x", maxInputBytes+1), "a\x00b"} {
		if _, err := Discover(core.ProjectDiscoveryRequestGithub{Query: input}, noCommands()); err == nil {
			t.Fatal("a GitHub query was accepted")
		}
		if _, err := Discover(core.ProjectDiscoveryRequestDirectory{Path: input}, noCommands()); err == nil {
			t.Fatal("a directory was accepted")
		}
	}
}
