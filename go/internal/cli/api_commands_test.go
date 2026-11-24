// Tests of api_commands.go, ported from the tests module of
// mj-cli/src/api_commands.rs.

package cli

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/server"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
)

// mustParse parses argv as `mj` does, failing the test on a usage error.
func mustParse(t *testing.T, argv ...string) command {
	t.Helper()
	parsed, err := parseCommandLine(argv)
	if err != nil {
		t.Fatalf("%q: %v", argv, err)
	}
	return parsed.command
}

// refused is whether argv is a usage error.
func refused(argv ...string) bool {
	_, err := parseCommandLine(argv)
	return err != nil
}

func stringPointer(value string) *string { return &value }

func TestFetchLsRendersARealDirectoryListing(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("the Rust test is #[cfg(unix)]")
	}
	root := t.TempDir()
	probes := filepath.Join(root, "rust-veto-cf")
	if err := os.MkdirAll(filepath.Join(probes, "nested"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(probes, "report.md"), []byte("# Report\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(probes, "probe.log"), bytes.Repeat([]byte("x"), 120_000), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("report.md", filepath.Join(probes, "latest")); err != nil {
		t.Fatal(err)
	}
	for _, entry := range []struct {
		name    string
		seconds int64
	}{
		{"nested", 1_792_656_000},
		{"report.md", 1_792_742_130},
		{"probe.log", 1_792_742_160},
	} {
		at := time.Unix(entry.seconds, 0)
		if err := os.Chtimes(filepath.Join(probes, entry.name), at, at); err != nil {
			t.Fatal(err)
		}
	}
	list := func(path string) checkpoint.SessionListing {
		t.Helper()
		payload, err := os.CreateTemp(t.TempDir(), "payload")
		if err != nil {
			t.Fatal(err)
		}
		fetched, err := checkpoint.FetchSessionPath(path, []string{root}, false, checkpoint.FetchModeListing, payload)
		if err != nil {
			t.Fatal(err)
		}
		defer fetched.Close()
		var listing checkpoint.SessionListing
		if err := json.NewDecoder(fetched.Reader()).Decode(&listing); err != nil {
			t.Fatal(err)
		}
		return listing
	}
	directory := list(probes)
	// The standard library cannot set a symlink's own time, so the link's
	// creation time is replaced; everything else is what the target read.
	for index := range directory.Entries {
		if directory.Entries[index].Name == "latest" {
			directory.Entries[index].Modified = stringPointer("2026-10-22T09:00:00Z")
		}
	}
	file := list(filepath.Join(probes, "probe.log"))
	output := fmt.Sprintf("=== mj fetch --ls <directory> ===\n%s\n=== mj fetch --ls <file> ===\n%s",
		strings.Join(listingLines(&directory), "\n"), strings.Join(listingLines(&file), "\n"))
	core.AssertGolden(t, "testdata", "fetch-ls", output)
}

func TestGithubTokenRequiresOneOwnerOrRepositorySelector(t *testing.T) {
	owner, ok := mustParse(t, "github-token", "--owner", "acme").(commandGithubToken)
	if !ok {
		t.Fatal("expected github-token command")
	}
	if owner.args.owner == nil || *owner.args.owner != "acme" {
		t.Errorf("owner = %v", owner.args.owner)
	}
	if len(owner.args.repo) != 0 {
		t.Errorf("repo = %q", owner.args.repo)
	}

	repo, ok := mustParse(t, "github-token", "--repo", "acme/project").(commandGithubToken)
	if !ok {
		t.Fatal("expected github-token command")
	}
	if repo.args.owner != nil {
		t.Errorf("owner = %q", *repo.args.owner)
	}
	if !slices.Equal(repo.args.repo, []string{"acme/project"}) {
		t.Errorf("repo = %q", repo.args.repo)
	}
	repositories, ok := mustParse(t, "github-token", "--repo", "acme/project", "--repo", "acme/tools").(commandGithubToken)
	if !ok {
		t.Fatal("expected github-token command")
	}
	if !slices.Equal(repositories.args.repo, []string{"acme/project", "acme/tools"}) {
		t.Errorf("repo = %q", repositories.args.repo)
	}
	if !refused("github-token") {
		t.Error("github-token without a selector parsed")
	}
	if !refused("github-token", "--owner", "acme", "--repo", "acme/project") {
		t.Error("github-token with both selectors parsed")
	}
}

func TestReviewParsesStartAndStatusWithASession(t *testing.T) {
	for _, entry := range []struct {
		verb string
		json bool
	}{{"start", false}, {"status", true}} {
		argv := []string{"review", entry.verb, "--session", "s1"}
		if entry.json {
			argv = append(argv, "--json")
		}
		review, ok := mustParse(t, argv...).(commandReview)
		if !ok {
			t.Fatal("expected the review command")
		}
		var session sessionArgs
		switch command := review.args.command.(type) {
		case reviewCommandStart:
			session = command.args
		case reviewCommandStatus:
			session = command.args
		default:
			t.Fatalf("expected start or status, got %T", command)
		}
		if session.session != "s1" || session.json != entry.json {
			t.Errorf("%s: session = %+v", entry.verb, session)
		}
	}
	review, ok := mustParse(t, "review", "show", "--session", "s1", "--review", "r1", "--json").(commandReview)
	if !ok {
		t.Fatal("expected the review command")
	}
	show, ok := review.args.command.(reviewCommandShow)
	if !ok {
		t.Fatal("expected review show")
	}
	if show.args.session.session != "s1" || show.args.review == nil || *show.args.review != "r1" || !show.args.session.json {
		t.Errorf("show = %+v", show.args)
	}
	review, ok = mustParse(t, "review", "show", "--session", "s1").(commandReview)
	if !ok {
		t.Fatal("expected the review command")
	}
	show, ok = review.args.command.(reviewCommandShow)
	if !ok {
		t.Fatal("expected review show")
	}
	if show.args.review != nil {
		t.Errorf("review defaults to the latest finished review, got %q", *show.args.review)
	}
	for _, verb := range []string{"forward", "dismiss", "cancel"} {
		if !refused("review", verb, "--session", "s1") {
			t.Errorf("review %s parsed", verb)
		}
	}
	if !refused("review", "start") {
		t.Error("review start without a session parsed")
	}
}

func TestStopTaskRequiresASessionAndExactlyOneTaskSelection(t *testing.T) {
	for _, argv := range [][]string{
		{"stop-task", "--session", "s1"},
		{"stop-task", "--session", "s1", "terminal:one", "--all"},
		{"stop-task", "--all"},
	} {
		if !refused(argv...) {
			t.Errorf("%q parsed", argv)
		}
	}
	for _, selection := range []string{"terminal:one", "--all"} {
		stop, ok := mustParse(t, "stop-task", "--session", "s1", selection, "--json").(commandStopTask)
		if !ok {
			t.Fatal("expected stop-task")
		}
		if stop.args.session != "s1" || stop.args.all != (selection == "--all") || !stop.args.json {
			t.Errorf("%s: args = %+v", selection, stop.args)
		}
	}
}

// withAPIVersion answers every response with the contract version header,
// as the daemon's API layer does.
func withAPIVersion(handler http.Handler) http.Handler {
	return http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		writer.Header().Set(server.APIVersionHeader, "1")
		handler.ServeHTTP(writer, request)
	})
}

// captureStdout runs print with os.Stdout redirected to a pipe, drained while
// print writes, and returns what it wrote.
func captureStdout(t *testing.T, print func() error) string {
	t.Helper()
	reader, writer, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	var captured bytes.Buffer
	drained := make(chan error, 1)
	go func() {
		_, err := io.Copy(&captured, reader)
		drained <- err
	}()
	saved := os.Stdout
	os.Stdout = writer
	printed := func() error {
		defer func() { os.Stdout = saved }()
		return print()
	}()
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	if err := <-drained; err != nil {
		t.Fatal(err)
	}
	if printed != nil {
		t.Fatal(printed)
	}
	return captured.String()
}

// Hard-won: abab87840: `mj transcript` could not read the Rust daemon's page,
// whose execution state and item bodies are sealed types that plain
// encoding/json cannot fill. The responses were recorded from the Rust daemon
// at 3dde0c11b (an OpenCode session that wrote one file) for the queries the
// Rust mj sends, and each expected output is the Rust mj's for the same page.
// The session's checkout path reads /tmp/go-port-merge2a/repo in every file.
func TestTranscriptPrintsRecordedRustDaemonPagesAsTheRustMjDoes(t *testing.T) {
	const session = "f18d6fd12a430498505115c5c0270be6"
	const defaultQuery = "role=user&role=agent&role=message&role=thought&role=plan&role=plan_proposal&role=system"
	read := func(name string) []byte {
		t.Helper()
		data, err := os.ReadFile(filepath.Join("testdata", "rust_daemon_exchange", name))
		if err != nil {
			t.Fatal(err)
		}
		return data
	}
	for _, check := range []struct {
		argv     []string
		query    string
		response string
		output   string
	}{
		{[]string{"transcript", "--session", session}, defaultQuery,
			"transcript_default_response.json", "transcript_default_output.txt"},
		{[]string{"transcript", "--session", session, "--json"}, defaultQuery,
			"transcript_default_response.json", "transcript_default_json_output.txt"},
		{[]string{"transcript", "--session", session, "--role", "tool"}, "role=tool",
			"transcript_tool_response.json", "transcript_tool_output.txt"},
	} {
		parsed, ok := mustParse(t, check.argv...).(commandTranscript)
		if !ok {
			t.Fatalf("%q did not parse as mj transcript", check.argv)
		}
		roles, err := requestedTranscriptRoles(&parsed.args)
		if err != nil {
			t.Fatal(err)
		}
		response := read(check.response)
		mux := http.NewServeMux()
		mux.HandleFunc("GET /api/v1/sessions/"+session+"/transcript", func(writer http.ResponseWriter, request *http.Request) {
			if request.URL.RawQuery != check.query {
				t.Errorf("%q asked for %q; the recording answers %q", check.argv, request.URL.RawQuery, check.query)
				writer.WriteHeader(http.StatusBadRequest)
				return
			}
			writer.Header().Set("content-type", "application/json")
			_, _ = writer.Write(response)
		})
		daemon := httptest.NewServer(withAPIVersion(mux))
		api, err := newAPIClient(daemon.URL, "test-token")
		if err != nil {
			daemon.Close()
			t.Fatal(err)
		}
		printed := captureStdout(t, func() error {
			return printTranscript(context.Background(), api, parsed.args, roles)
		})
		daemon.Close()
		if want := string(read(check.output)); printed != want {
			t.Errorf("mj %s printed\n%s\nthe Rust mj printed\n%s", strings.Join(check.argv, " "), printed, want)
		}
	}
}

func TestStopTaskSendsOpaqueIdsAndAllReportsPartialFailures(t *testing.T) {
	session := waitResponse(t, "finished", map[string]any{}).Session
	session.BackgroundTasks = nil
	for _, task := range []struct {
		id      string
		canStop bool
	}{
		{"terminal:one / opaque", true},
		{"terminal:failed", true},
		{"native:observer", false},
		{"terminal:two", true},
	} {
		session.BackgroundTasks = append(session.BackgroundTasks, server.ViewerBackgroundTask{
			ID: task.id, Command: "sleep 60", StartedAtMs: 0, CanStop: task.canStop,
		})
	}
	var mu sync.Mutex
	var seen []string
	mux := http.NewServeMux()
	mux.HandleFunc("GET /api/v1/sessions/s1", func(writer http.ResponseWriter, _ *http.Request) {
		encoded, err := core.SerdeJSON(session)
		if err != nil {
			t.Error(err)
			return
		}
		writer.Header().Set("content-type", "application/json")
		_, _ = writer.Write(encoded)
	})
	mux.HandleFunc("POST /api/v1/sessions/s1/background-tasks/stop", func(writer http.ResponseWriter, request *http.Request) {
		if got := request.Header.Get("authorization"); got != "Bearer test-token" {
			t.Errorf("authorization = %q", got)
		}
		var stop server.StopBackgroundTaskRequest
		if err := json.NewDecoder(request.Body).Decode(&stop); err != nil {
			t.Error(err)
			return
		}
		mu.Lock()
		seen = append(seen, stop.BackgroundTaskID)
		mu.Unlock()
		if stop.BackgroundTaskID == "terminal:failed" {
			writer.Header().Set("content-type", "application/json")
			writer.WriteHeader(http.StatusConflict)
			_, _ = writer.Write([]byte(`{"error":"task already ended"}`))
			return
		}
		writer.WriteHeader(http.StatusAccepted)
	})
	daemon := httptest.NewServer(withAPIVersion(mux))
	defer daemon.Close()
	api, err := newAPIClient(daemon.URL, "test-token")
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	args := stopTaskArgs{session: "s1", taskID: stringPointer("terminal:one / opaque"), all: false, json: true}
	single, err := stopTasks(ctx, api, &args)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(single.AcceptedTaskIDs, []string{"terminal:one / opaque"}) {
		t.Errorf("accepted = %q", single.AcceptedTaskIDs)
	}
	mu.Lock()
	if !slices.Equal(seen, []string{"terminal:one / opaque"}) {
		t.Errorf("seen = %q", seen)
	}
	seen = nil
	mu.Unlock()
	args.taskID = nil
	args.all = true
	all, err := stopTasks(ctx, api, &args)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(all.AcceptedTaskIDs, []string{"terminal:one / opaque", "terminal:two"}) {
		t.Errorf("accepted = %q", all.AcceptedTaskIDs)
	}
	if !slices.Equal(all.SkippedTaskIDs, []string{"native:observer"}) {
		t.Errorf("skipped = %q", all.SkippedTaskIDs)
	}
	if !strings.Contains(all.Failures["terminal:failed"], "task already ended") {
		t.Errorf("failures = %q", all.Failures)
	}
	mu.Lock()
	requests := slices.Clone(seen)
	mu.Unlock()
	slices.Sort(requests)
	if !slices.Equal(requests, []string{"terminal:failed", "terminal:one / opaque", "terminal:two"}) {
		t.Errorf("requests = %q", requests)
	}
	encoded, err := core.SerdeJSON(all)
	if err != nil {
		t.Fatal(err)
	}
	var report map[string]any
	if err := json.Unmarshal(encoded, &report); err != nil {
		t.Fatal(err)
	}
	if report["session_id"] != "s1" {
		t.Errorf("session_id = %v", report["session_id"])
	}
	if accepted, ok := report["accepted_task_ids"].([]any); !ok || len(accepted) != 2 {
		t.Errorf("accepted_task_ids = %v", report["accepted_task_ids"])
	}
}

// Hard-won: db7d3a78: --json export with --out printed prose instead of a JSON object.
func TestExportJSONWithOutReportsOneObjectAndTextStaysASentence(t *testing.T) {
	path := filepath.Join(t.TempDir(), "work.patch")
	var printed bytes.Buffer
	if err := writeBytesTo(&printed, []byte("diff"), &path, true, "patch"); err != nil {
		t.Fatal(err)
	}
	var report map[string]any
	if err := json.Unmarshal(printed.Bytes(), &report); err != nil {
		t.Fatal(err)
	}
	if report["bytes"] != float64(4) {
		t.Errorf("bytes = %v", report["bytes"])
	}
	if report["format"] != "patch" {
		t.Errorf("format = %v", report["format"])
	}
	if report["path"] != path {
		t.Errorf("path = %v", report["path"])
	}
	written, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	if string(written) != "diff" {
		t.Errorf("written = %q", written)
	}

	printed.Reset()
	if err := writeBytesTo(&printed, []byte("diff"), &path, false, "patch"); err != nil {
		t.Fatal(err)
	}
	if !strings.HasPrefix(printed.String(), "wrote 4 bytes to ") {
		t.Errorf("printed = %q", printed.String())
	}

	printed.Reset()
	if err := writeBytesTo(&printed, []byte("diff"), nil, true, "patch"); err != nil {
		t.Fatal(err)
	}
	if printed.String() != "diff" {
		t.Errorf("printed = %q", printed.String())
	}
}

// waitResponse is a wait answer for session s1 with outcome and the extra
// fields, decoded from JSON as the client decodes the daemon's.
func waitResponse(t *testing.T, outcome string, extra map[string]any) server.WaitResponse {
	t.Helper()
	body := map[string]any{
		"outcome": outcome,
		"session": map[string]any{
			"id": "s1", "workspace_id": "w1", "title": "t",
			"harness_kind": "codex", "profile_id": "p", "target_id": "t",
			"bundle_id": "b", "state": "running", "lifecycle": "live",
			"chat_phase": "idle", "is_idle": false, "has_error": false,
			"created_at": "now", "updated_at": "now",
		},
	}
	for key, value := range extra {
		body[key] = value
	}
	encoded, err := json.Marshal(body)
	if err != nil {
		t.Fatal(err)
	}
	var response server.WaitResponse
	if err := json.Unmarshal(encoded, &response); err != nil {
		t.Fatalf("wait response: %v", err)
	}
	return response
}

// A daemon being replaced ends a wait with the handoff mark. The wait asks
// the next daemon for the same turn, with what is left of its budget, and
// reports that daemon's answer instead of failing.
// Hard-won: c87e5e88: wait ended with an error instead of following its accepted turn after daemon replacement.
func TestAWaitFollowsTheDaemonAcrossAnUpgradeHandoff(t *testing.T) {
	old := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.Method != http.MethodPost || !strings.HasSuffix(request.URL.Path, "/wait") {
			http.NotFound(writer, request)
			return
		}
		writer.Header().Set(server.UpgradeHeader, "pending")
		writer.Header().Set("retry-after", "1")
		writer.Header().Set("content-type", "application/json")
		writer.WriteHeader(http.StatusServiceUnavailable)
		encoded, err := json.Marshal(map[string]any{
			"error": "the Mjolnir daemon is being replaced by an upgrade; ask again",
			"code":  server.DaemonHandoffCode,
		})
		if err != nil {
			t.Error(err)
			return
		}
		_, _ = writer.Write(encoded)
	}))
	defer old.Close()
	var mu sync.Mutex
	var asked []sessionruntime.WaitRequest
	answer := waitResponse(t, "finished", map[string]any{})
	replacement := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.Method != http.MethodPost || !strings.HasSuffix(request.URL.Path, "/wait") {
			http.NotFound(writer, request)
			return
		}
		var wait sessionruntime.WaitRequest
		if err := json.NewDecoder(request.Body).Decode(&wait); err != nil {
			t.Error(err)
			return
		}
		mu.Lock()
		asked = append(asked, wait)
		mu.Unlock()
		encoded, err := core.SerdeJSON(answer)
		if err != nil {
			t.Error(err)
			return
		}
		writer.Header().Set(server.APIVersionHeader, "1")
		writer.Header().Set("content-type", "application/json")
		_, _ = writer.Write(encoded)
	}))
	defer replacement.Close()
	first, err := newAPIClient(old.URL, "token")
	if err != nil {
		t.Fatal(err)
	}
	reconnects := 0
	turn, timeout := uint64(4), uint64(3600)
	response, err := waitFollowingHandoffs(context.Background(), first, "s1",
		sessionruntime.WaitRequest{ReturnOnInput: true, TurnID: &turn, TimeoutSecs: &timeout},
		func(context.Context, *sessionruntime.WaitRequest) (*apiClient, error) {
			reconnects++
			return newAPIClient(replacement.URL, "token")
		})
	if err != nil {
		t.Fatal(err)
	}
	if response.Outcome != sessionruntime.WaitOutcomeFinished {
		t.Errorf("outcome = %s", response.Outcome)
	}
	if reconnects != 1 {
		t.Errorf("reconnects = %d", reconnects)
	}
	mu.Lock()
	defer mu.Unlock()
	if len(asked) != 1 {
		t.Fatalf("asked = %+v", asked)
	}
	if !asked[0].ReturnOnInput {
		t.Error("return_on_input was not kept")
	}
	if asked[0].TurnID == nil || *asked[0].TurnID != 4 {
		t.Errorf("the same turn, not a new prompt: %v", asked[0].TurnID)
	}
	if remaining := asked[0].TimeoutSecs; remaining == nil || *remaining < 3590 || *remaining > 3600 {
		t.Errorf("the next daemon gets what is left of the budget: %v", remaining)
	}
}

// A command continued under the daemon's newer build must parse there as
// the rest of the same command: same turn, time left, same filter.
// Hard-won: 4ce83357: wait and events did not continue under the replacing daemon after a protocol bump.
func TestContinuationsParseAsTheRestOfTheSameCommand(t *testing.T) {
	turn, timeout := uint64(4), uint64(1200)
	request := sessionruntime.WaitRequest{ReturnOnInput: true, TurnID: &turn, TimeoutSecs: &timeout}
	wait, ok := mustParse(t, waitContinuation("s1", &request, true)...).(commandWait)
	if !ok {
		t.Fatal("expected the wait command")
	}
	if wait.args.session != "s1" || wait.args.turn == nil || *wait.args.turn != 4 || wait.args.timeout == nil || *wait.args.timeout != 1200 {
		t.Errorf("wait = %+v", wait.args)
	}
	if !wait.args.returnOnInput || !wait.args.json {
		t.Errorf("wait = %+v", wait.args)
	}

	filter := core.ApiEventFilter{SessionID: stringPointer("s1"), WorkspaceID: stringPointer("w1")}
	after := uint64(9)
	events, ok := mustParse(t, eventsContinuation(&filter, &after)...).(commandEvents)
	if !ok {
		t.Fatal("expected the events command")
	}
	if events.args.session == nil || *events.args.session != "s1" {
		t.Errorf("session = %v", events.args.session)
	}
	if events.args.workspaceID == nil || *events.args.workspaceID != "w1" {
		t.Errorf("workspace id = %v", events.args.workspaceID)
	}
	if events.args.afterSeq == nil || *events.args.afterSeq != 9 {
		t.Errorf("after seq = %v", events.args.afterSeq)
	}
}

func TestNewOptOutFlagsAreOffUnlessNamed(t *testing.T) {
	parse := func(extra ...string) [3]bool {
		t.Helper()
		created, ok := mustParse(t, append([]string{"new", "--bundle", "product"}, extra...)...).(commandNew)
		if !ok {
			t.Fatal("expected the new command")
		}
		return [3]bool{created.args.noProjectMemory, created.args.noGitHubEvents, created.args.noGitHubCredentials}
	}
	if !refused("new", "--bundle", "product", "--no-mailbox") {
		t.Error("--no-mailbox parsed")
	}
	for _, entry := range []struct {
		extra []string
		want  [3]bool
	}{
		{nil, [3]bool{false, false, false}},
		{[]string{"--no-project-memory"}, [3]bool{true, false, false}},
		{[]string{"--no-github-events"}, [3]bool{false, true, false}},
		{[]string{"--no-github-credentials"}, [3]bool{false, false, true}},
		{[]string{"--no-github-events", "--no-project-memory"}, [3]bool{true, true, false}},
	} {
		if got := parse(entry.extra...); got != entry.want {
			t.Errorf("%q: %v, want %v", entry.extra, got, entry.want)
		}
	}
}

func TestNewDeliversNotificationsUnlessIgnored(t *testing.T) {
	parse := func(extra ...string) bool {
		t.Helper()
		created, ok := mustParse(t, append([]string{"new", "--bundle", "product"}, extra...)...).(commandNew)
		if !ok {
			t.Fatal("expected the new command")
		}
		return created.args.ignoreNotifications
	}
	if parse() {
		t.Error("notifications ignored by default")
	}
	if !parse("--ignore-notifications") {
		t.Error("--ignore-notifications not taken")
	}
}

func TestNoLaterHistoryNeedsAt(t *testing.T) {
	sha := "0123456789abcdef0123456789abcdef01234567"
	parse := func(extra ...string) (*string, bool, error) {
		parsed, err := parseCommandLine(append([]string{"new", "--bundle", "product"}, extra...))
		if err != nil {
			return nil, false, err
		}
		created, ok := parsed.command.(commandNew)
		if !ok {
			t.Fatal("expected the new command")
		}
		return created.args.at, created.args.noLaterHistory, nil
	}
	if at, later, err := parse(); err != nil || at != nil || later {
		t.Errorf("no flags: %v %v %v", at, later, err)
	}
	if at, later, err := parse("--at", sha); err != nil || at == nil || *at != sha || later {
		t.Errorf("--at: %v %v %v", at, later, err)
	}
	if at, later, err := parse("--at", sha, "--no-later-history"); err != nil || at == nil || *at != sha || !later {
		t.Errorf("--at --no-later-history: %v %v %v", at, later, err)
	}
	if _, _, err := parse("--no-later-history"); err == nil {
		t.Error("requires --at")
	}
	if refusal := nameLaunchFlags(fmt.Errorf("`no_later_history` requires `at`")); refusal.Error() != "--no-later-history requires --at" {
		t.Errorf("refusal = %q", refusal.Error())
	}
}

// Hard-won: #1615: mj new --project-directory on a container target creates a quick bundle and allows --at/--no-later-history
func TestNewProjectDirectoryOnContainerTargetResolvesQuickBundleAndAllowsAt(t *testing.T) {
	sha := "0123456789abcdef0123456789abcdef01234567"
	created, ok := mustParse(t, "new", "--project-directory", "/work/repo", "--target", "podman", "--at", sha, "--no-later-history").(commandNew)
	if !ok {
		t.Fatal("expected the new command")
	}
	if created.args.at == nil || *created.args.at != sha || !created.args.noLaterHistory {
		t.Errorf("args = %+v", created.args)
	}
	if created.args.projectDirectory == nil || *created.args.projectDirectory != "/work/repo" {
		t.Errorf("project directory = %v", created.args.projectDirectory)
	}

	options := server.LaunchOptions{
		Revision:          1,
		PrimaryConfigured: false,
		Profiles:          []server.LaunchProfile{},
		Targets: []server.LaunchTarget{
			{ID: "podman", Kind: "podman", RequiresProjectDirectory: false, Availability: server.LaunchAvailabilityReady},
			{ID: "localhost", Kind: "local-bare", RequiresProjectDirectory: true, Availability: server.LaunchAvailabilityReady},
		},
		Bundles: []server.LaunchBundle{},
		Hosts:   []server.LaunchHost{},
	}
	api, err := newAPIClient("http://127.0.0.1:1", "test-token")
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	containerOptions := options
	bundle, directory, err := resolveNewSessionProjectTarget(ctx, api, nil, stringPointer("/work/repo"), stringPointer("podman"), &containerOptions,
		func(_ context.Context, dir string) (string, error) {
			if dir != "/work/repo" {
				t.Errorf("directory = %q", dir)
			}
			return "quick-bundle", nil
		})
	if err != nil {
		t.Fatalf("container target should resolve quick bundle: %v", err)
	}
	if bundle == nil || *bundle != "quick-bundle" {
		t.Errorf("bundle = %v", bundle)
	}
	if directory != nil {
		t.Errorf("directory = %q", *directory)
	}

	bareBundle, bareDirectory, err := resolveNewSessionProjectTarget(ctx, api, nil, stringPointer("/work/repo"), stringPointer("localhost"), &options,
		func(context.Context, string) (string, error) {
			t.Fatal("bare target must not create a quick bundle")
			return "", nil
		})
	if err != nil {
		t.Fatalf("bare target should keep project_directory: %v", err)
	}
	if bareBundle != nil {
		t.Errorf("bundle = %q", *bareBundle)
	}
	if bareDirectory == nil || *bareDirectory != "/work/repo" {
		t.Errorf("directory = %v", bareDirectory)
	}
}

// Hard-won: 06cfc1cfe: reviewer-only flags preserve the global trigger.
func TestNewReviewFlagsChooseTheSessionsOwnReview(t *testing.T) {
	parse := func(extra ...string) (core.SessionReview, error, error) {
		parsed, err := parseCommandLine(append([]string{"new", "--bundle", "product"}, extra...))
		if err != nil {
			return nil, nil, err
		}
		created, ok := parsed.command.(commandNew)
		if !ok {
			t.Fatal("expected new command")
		}
		review, reviewErr := newSessionReview(&created.args)
		return review, reviewErr, nil
	}
	mustReview := func(extra ...string) core.SessionReview {
		t.Helper()
		review, reviewErr, err := parse(extra...)
		if err != nil || reviewErr != nil {
			t.Fatalf("%q: %v %v", extra, err, reviewErr)
		}
		return review
	}
	if review := mustReview(); review != nil {
		t.Errorf("no flags: %#v", review)
	}
	inherited := mustReview("--reviewer", "gpt-6-terra:xhigh", "--reviewer", "gpt-6-sol:medium")
	choices := []core.SubagentChoice{
		{Model: "gpt-6-terra", Effort: stringPointer("xhigh")},
		{Model: "gpt-6-sol", Effort: stringPointer("medium")},
	}
	if !reflect.DeepEqual(inherited, core.SessionReviewInherit{Reviewers: &choices}) {
		t.Errorf("inherited = %#v", inherited)
	}
	// Session reviewer flags enable review even without global reviewers.
	for _, global := range [][]core.SubagentChoice{{}, {choices[1]}} {
		config := core.ReviewConfig{Reviewers: global}
		resolved := config.ForSession(inherited)
		if trigger := resolved.Trigger(); trigger != core.ReviewTriggerPush {
			t.Errorf("trigger = %s", trigger)
		}
		if !reflect.DeepEqual(resolved.Reviewers, choices) {
			t.Errorf("reviewers = %#v", resolved.Reviewers)
		}
	}
	if review := mustReview("--review", "off"); !reflect.DeepEqual(review, core.SessionReviewOff{}) {
		t.Errorf("off = %#v", review)
	}
	if review := mustReview("--review", "push"); !reflect.DeepEqual(review, core.SessionReviewPush{Reviewers: nil}) {
		t.Errorf("push = %#v", review)
	}
	// Per-turn review was removed; the refusal says what to use instead.
	_, reviewErr, err := parse("--review", "turn")
	if err != nil {
		t.Fatal(err)
	}
	if reviewErr == nil || !strings.Contains(reviewErr.Error(), "--review push") {
		t.Errorf("refusal = %v", reviewErr)
	}
	if _, reviewErr, err := parse("--reviewer", "a", "--reviewer", "b", "--reviewer", "c", "--reviewer", "d"); err != nil || reviewErr == nil {
		t.Errorf("four reviewers: %v %v", err, reviewErr)
	}
	if _, reviewErr, err := parse("--reviewer", ":high"); err != nil || reviewErr == nil {
		t.Errorf("an empty model: %v %v", err, reviewErr)
	}
	for _, entry := range []struct {
		input, model string
		effort       *string
	}{
		{"openai/gpt-oss-120b:free", "openai/gpt-oss-120b:free", nil},
		{"qwen:latest", "qwen:latest", nil},
		{"x:0:high", "x:0", stringPointer("high")},
		{"foo:high:", "foo:high", nil},
	} {
		parsed, ok := mustReview("--reviewer", entry.input).(core.SessionReviewInherit)
		if !ok || parsed.Reviewers == nil {
			t.Fatalf("%s: review override expected", entry.input)
		}
		reviewers := *parsed.Reviewers
		if reviewers[0].Model != entry.model || !reflect.DeepEqual(reviewers[0].Effort, entry.effort) {
			t.Errorf("%s: %#v", entry.input, reviewers[0])
		}
	}

	parsed := mustReview("--reviewer", "anthropic.claude-haiku:0", "--reviewer", "x:0:high", "--reviewer", "m:high")
	want := []core.SubagentChoice{
		{Model: "anthropic.claude-haiku:0", Effort: nil},
		{Model: "x:0", Effort: stringPointer("high")},
		{Model: "m", Effort: stringPointer("high")},
	}
	if !reflect.DeepEqual(parsed, core.SessionReviewInherit{Reviewers: &want}) {
		t.Errorf("parsed = %#v", parsed)
	}
	if !refused("new", "--bundle", "product", "--review-count", "2") {
		t.Error("--review-count parsed")
	}
}

func TestNewAndMoveParseSubagentOverridesAndRejectConflicts(t *testing.T) {
	commandLine := func(command string, extra []string) []string {
		argv := []string{command, "--profile", "codex", "--target", "raw"}
		if command == "new" {
			argv = append(argv, "--project-directory", "/srv/project")
		} else {
			argv = append(argv, "--session", "session")
		}
		return append(argv, extra...)
	}
	parseCommand := func(command string, extra []string) (*core.SubagentPolicy, error) {
		parsed, err := parseCommandLine(commandLine(command, extra))
		if err != nil {
			return nil, err
		}
		switch parsed := parsed.command.(type) {
		case commandNew:
			return newSubagentPolicy(&parsed.args)
		case commandMove:
			args := parsed.args
			return subagentPolicy(args.subagents, args.subagentStraightforward, args.subagentStraightforwardEffort,
				args.subagentSubtle, args.subagentSubtleEffort, args.subagentModel, args.subagentEffort, args.orchestrators)
		}
		t.Fatalf("new or move command, got %T", parsed.command)
		return nil, nil
	}
	oracleOf := func(command string, extra []string) (*core.SubagentChoice, error) {
		parsed, err := parseCommandLine(commandLine(command, extra))
		if err != nil {
			return nil, err
		}
		switch parsed := parsed.command.(type) {
		case commandNew:
			return oracleOverride(parsed.args.subagentOracle, parsed.args.subagentOracleEffort, parsed.args.oracle)
		case commandMove:
			return oracleOverride(parsed.args.subagentOracle, parsed.args.subagentOracleEffort, parsed.args.oracle)
		}
		t.Fatalf("new or move command, got %T", parsed.command)
		return nil, nil
	}
	for _, command := range []string{"new", "move"} {
		parse := func(extra ...string) (*core.SubagentPolicy, error) { return parseCommand(command, extra) }
		mustPolicy := func(extra ...string) *core.SubagentPolicy {
			t.Helper()
			policy, err := parse(extra...)
			if err != nil {
				t.Fatalf("%s %q: %v", command, extra, err)
			}
			return policy
		}
		if policy := mustPolicy("--orchestrators"); policy == nil || !policy.OrchestratorsEnabled() {
			t.Errorf("%s --orchestrators: %#v", command, policy)
		}
		if policy := mustPolicy("--subagents", "mjolnir", "--orchestrators"); policy == nil || !policy.OrchestratorsEnabled() {
			t.Errorf("%s --subagents mjolnir --orchestrators: %#v", command, policy)
		}
		for _, extra := range [][]string{
			{"--subagents", "native", "--orchestrators"},
			{"--subagents", "none", "--orchestrators"},
			{"--supervisors", "enabled"},
		} {
			if _, err := parse(extra...); err == nil {
				t.Errorf("%s %q accepted", command, extra)
			}
		}
		oracle := func(extra ...string) (*core.SubagentChoice, error) { return oracleOf(command, extra) }
		choice, err := oracle("--subagents", "native", "--subagent-oracle", "advisor", "--subagent-oracle-effort", "high")
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(choice, &core.SubagentChoice{Model: "advisor", Effort: stringPointer("high")}) {
			t.Errorf("%s oracle = %#v", command, choice)
		}
		for _, extra := range [][]string{
			{"--subagent-oracle-effort", "high"},
			{"--subagent-oracle", "advisor", "--subagent-oracle-effort", "adaptive"},
			{"--oracle", "disabled", "--subagent-oracle", "advisor"},
			{"--oracle", "invalid"},
		} {
			if _, err := oracle(extra...); err == nil {
				t.Errorf("%s %q accepted", command, extra)
			}
		}
		if policy := mustPolicy(); policy != nil {
			t.Errorf("%s no flags: %#v", command, policy)
		}
		if policy := mustPolicy("--subagents", "native"); !reflect.DeepEqual(policy, &core.SubagentPolicy{Mode: core.SubagentPolicyNative}) {
			t.Errorf("%s native: %#v", command, policy)
		}
		if policy := mustPolicy("--subagents", "mjolnir"); !reflect.DeepEqual(policy, &core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir}) {
			t.Errorf("%s mjolnir: %#v", command, policy)
		}
		if policy := mustPolicy("--subagents", "none"); !reflect.DeepEqual(policy, &core.SubagentPolicy{Mode: core.SubagentPolicyNone}) {
			t.Errorf("%s none: %#v", command, policy)
		}
		for _, mode := range []*string{nil, stringPointer("mjolnir")} {
			args := []string{"--subagent-subtle", "gpt-6-luna", "--subagent-subtle-effort", "max"}
			if mode != nil {
				args = append(args, "--subagents", *mode)
			}
			want := &core.SubagentPolicy{
				Mode:          core.SubagentPolicyMjolnir,
				Orchestrators: false,
				Subtle:        &core.SubagentChoice{Model: "gpt-6-luna", Effort: stringPointer("max")},
			}
			if policy := mustPolicy(args...); !reflect.DeepEqual(policy, want) {
				t.Errorf("%s %q: %#v", command, args, policy)
			}
		}
		for _, extra := range [][]string{
			{"--subagent-straightforward-effort", "max"},
			{"--subagent-subtle", "model", "--subagent-subtle-effort", "adaptive"},
			{"--subagent-model", "model"},
			{"--mj-subagents"},
			{"--native-subagents"},
			{"--subagents", "all-models"},
			{"--subagents", "single-model"},
			{"--subagents", "native", "--subagent-straightforward", "model"},
			{"--subagents", "native", "--subagent-subtle-effort", "high"},
			{"--subagents", "none", "--subagent-straightforward", "model"},
		} {
			if _, err := parse(extra...); err == nil {
				t.Errorf("%s %q accepted", command, extra)
			}
		}
		_, retired := parse("--subagent-model", "model")
		if retired == nil {
			t.Fatalf("%s: the retired flag was accepted", command)
		}
		for _, flag := range []string{
			"--subagent-straightforward",
			"--subagent-straightforward-effort",
			"--subagent-subtle",
			"--subagent-subtle-effort",
		} {
			if !strings.Contains(retired.Error(), flag) {
				t.Errorf("%s: %v", command, retired)
			}
		}
	}
}

// A turn the worker failed for going quiet has to say why, where a script
// waiting on it can see it. Before this the reason lived only in the
// transcript and `mj wait` printed the bare word "error" (#1020). Launch
// finding J-25: `mj prompt --wait` printed the Codex quota sentence three
// times, as the wait's message, the diagnostic, and the agent's final
// message. Each distinct line is printed once.
// Hard-won: cd2f1b6d: a Codex quota reason was printed three times and surfaced as an ordinary error.
func TestAReasonRepeatedInTheFinalMessageIsPrintedOnce(t *testing.T) {
	sentence := "You’ve hit your usage limit. Try again at Sep 29th, 2026 10:20 PM."
	response := waitResponse(t, "quota_limit", map[string]any{
		"stop_reason":   "QuotaLimit",
		"turn_id":       8,
		"message":       sentence,
		"diagnostic":    map[string]any{"message": sentence, "code": "usageLimitExceeded"},
		"final_message": sentence + "\n",
	})
	lines := waitReportLines(&response)
	if lines[0] != "turn quota_limit (failed: quota limit reached)" {
		t.Errorf("first line = %q", lines[0])
	}
	count := 0
	for _, line := range lines {
		if strings.Contains(line, "usage limit") {
			count++
		}
	}
	if count != 1 {
		t.Errorf("%q", lines)
	}
}

// Hard-won: 4a875bad: wait showed only error and hid the diagnostic already recorded by the worker.
func TestAFailedTurnReportsTheReasonTheWorkerRecorded(t *testing.T) {
	response := waitResponse(t, "error", map[string]any{
		"stop_reason": "harness_inactive",
		"diagnostic": map[string]any{
			"message": "The Muse turn stopped responding: the tool call job_output-7 ran for about 241 minutes.",
			"code":    "harness_inactive",
		},
	})
	lines := waitReportLines(&response)
	if lines[0] != "turn error (failed: harness inactive)" {
		t.Errorf("first line = %q", lines[0])
	}

	// I2-5: the transcript position of the turn's prompt is not a turn
	// count, so the wait prints no number for it.
	numbered := waitResponse(t, "finished", map[string]any{"turn_id": 8, "turn_number": 1})
	if got := waitReportLines(&numbered)[0]; got != "turn finished" {
		t.Errorf("numbered = %q", got)
	}
	if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "job_output-7") }) {
		t.Errorf("the reason is printed: %q", lines)
	}

	// Hard-won: DSH campaign C-2: failed startup details were present in the session but absent from `mj wait`.
	startupFailure := waitResponse(t, "error", map[string]any{})
	startupFailure.Session.Error = stringPointer("install the harness in the target environment")
	if !slices.Contains(waitReportLines(&startupFailure), "session error: install the harness in the target environment") {
		t.Errorf("startup failure lines = %q", waitReportLines(&startupFailure))
	}

	// A turn that finished normally is not annotated with a diagnostic it
	// may still carry from an earlier attempt.
	finished := waitResponse(t, "finished", map[string]any{"diagnostic": map[string]any{"message": "an older failure"}})
	if slices.ContainsFunc(waitReportLines(&finished), func(line string) bool { return strings.Contains(line, "an older failure") }) {
		t.Errorf("a finished turn prints no failure reason: %q", waitReportLines(&finished))
	}
}

// precision-3260: sessions on a full disk read as "unreachable" with nothing
// saying why. The list marks them and the report says why.
// Hard-won: 540c9202: a real full filesystem stopped workers while users saw only unreachable.
func TestASessionOnAFullDiskSaysSoInTheListAndTheReport(t *testing.T) {
	session := waitResponse(t, "finished", map[string]any{}).Session
	if got := listedState(&session); got != "running" {
		t.Errorf("state = %q", got)
	}
	session.StorageProblem = stringPointer("disk full: precision-3260 has 0 B free on /")
	if got := listedState(&session); got != "running (disk full)" {
		t.Errorf("state = %q", got)
	}
	if got, want := sessionReportLines(&session, 0), []string{"s1  running (disk full)  t", "disk full: precision-3260 has 0 B free on /"}; !slices.Equal(got, want) {
		t.Errorf("report = %q, want %q", got, want)
	}
	// Served severity wins even when the older raw fields disagree.
	session.Attention = core.SessionAttentionImportant
	if got := listedState(&session); got != "running (important) (disk full)" {
		t.Errorf("state = %q", got)
	}
	session.Attention = core.SessionAttentionUnreachable
	if got := listedState(&session); got != "running (unreachable) (disk full)" {
		t.Errorf("state = %q", got)
	}
}

// Hard-won: #1209: session detail could not show startup progress or loss of worker contact.
func TestSessionDetailRendersLifecycleAndWorkerRelay(t *testing.T) {
	session := waitResponse(t, "finished", map[string]any{}).Session
	session.State = "launching"
	var operation server.ViewerOperation
	if err := json.Unmarshal([]byte(`{
		"id": "create-s1", "session_id": "s1", "kind": "create",
		"started_at_epoch_seconds": 100, "cancellable": true,
		"stages": [{"label": "Sync", "started_at_epoch_seconds": 110}]
	}`), &operation); err != nil {
		t.Fatal(err)
	}
	session.Operation = &operation
	session.Relay = &server.RelayHealth{State: server.RelayStateDisconnected}
	want := []string{
		"s1  launching  t",
		"create started 1970-01-01T00:01:40Z",
		"  Sync since 1970-01-01T00:01:50Z",
		"worker relay is not attached",
	}
	if got := sessionReportLines(&session, 0); !slices.Equal(got, want) {
		t.Errorf("report = %q, want %q", got, want)
	}

	session.State = "running"
	session.Operation = nil
	session.Relay.State = server.RelayStateUnreachable
	if got, want := sessionReportLines(&session, 0), []string{"s1  running  t", "worker relay is unreachable"}; !slices.Equal(got, want) {
		t.Errorf("report = %q, want %q", got, want)
	}
}

// Hard-won: #1529: refused unexpired logins were absent from session and doctor diagnostics.
func TestGoldenRefusedProfileLoginDiagnostics(t *testing.T) {
	t.Skip("needs mj-setup (not yet ported): doctor::login_checks and doctor::render_human produce half of the golden")
}

// Launch finding R11-3: `mj sessions --session` printed `last turn
// Completed { stop_reason: "EndTurn" }`, Rust's debug form. It says how the
// turn ended in words.
// Hard-won: 1018b0f7: session summaries exposed Rust debug spellings instead of user-readable turn outcomes.
func TestOneSessionNamesHowItsLastTurnEndedInWords(t *testing.T) {
	withOutcome := func(outcome string) []string {
		t.Helper()
		session := waitResponse(t, "finished", map[string]any{}).Session
		var last core.MaterializedTurnOutcome
		if err := json.Unmarshal([]byte(`{
			"command_id": "prompt-1",
			"accepted_ordinal": 16,
			"completed_ordinal": 46,
			"completed_at_ms": 0,
			"outcome": `+outcome+`
		}`), &last); err != nil {
			t.Fatal(err)
		}
		session.LastTurnOutcome = &last
		return sessionReportLines(&session, 0)
	}
	if got, want := withOutcome(`{"kind": "completed", "stop_reason": "EndTurn"}`), []string{"s1  running  t", "last turn completed, end of turn"}; !slices.Equal(got, want) {
		t.Errorf("finished = %q", got)
	}
	for _, entry := range []struct{ outcome, words string }{
		{`{"kind": "completed", "stop_reason": "end_turn"}`, "completed, end of turn"},
		{`{"kind": "completed", "stop_reason": "Cancelled"}`, "interrupted"},
		{`{"kind": "interrupted", "message": "Interrupted by the user"}`, "interrupted"},
		{`{"kind": "completed", "stop_reason": "MaxTokens"}`, "failed: max tokens"},
		{`{"kind": "completed", "stop_reason": "max_turn_requests"}`, "failed: max turn requests"},
		{`{"kind": "rejected", "message": "the session is closing\nmore detail"}`, "failed: the session is closing"},
	} {
		if got := withOutcome(entry.outcome)[1]; got != "last turn "+entry.words {
			t.Errorf("%s: %q", entry.outcome, got)
		}
	}
}

// Launch finding R12-1: `mj wait` printed the harness's stop reason as the
// API returns it, "finished (EndTurn) turn 16 in 5.3s", while `mj sessions`
// and the parent's sub-agent notice said "completed, end of turn". `mj wait`
// and `mj prompt` use the same words, placed as the notice places them;
// `--json` keeps the stop reason unchanged.
// Hard-won: 5a09877c: wait exposed raw harness stop reasons unlike the existing session summary.
func TestAWaitSaysHowTheTurnEndedInTheWordsMjSessionsUses(t *testing.T) {
	finished := waitResponse(t, "finished", map[string]any{"stop_reason": "EndTurn", "turn_id": 16, "elapsed_ms": 5300})
	if got := waitReportLines(&finished)[0]; got != "turn finished (completed, end of turn) in 5.3s" {
		t.Errorf("finished = %q", got)
	}
	worked := waitResponse(t, "finished", map[string]any{"stop_reason": "EndTurn", "turn_id": 16, "elapsed_ms": 5300, "tool_calls": 4})
	if got := waitReportLines(&worked)[0]; got != "turn finished (completed, end of turn) in 5.3s · 4 tool calls" {
		t.Errorf("worked = %q", got)
	}
	oneCall := waitResponse(t, "finished", map[string]any{"tool_calls": 1})
	if got := waitReportLines(&oneCall)[0]; got != "turn finished · 1 tool call" {
		t.Errorf("one call = %q", got)
	}
	noCalls := waitResponse(t, "finished", map[string]any{"tool_calls": 0})
	if got := waitReportLines(&noCalls)[0]; got != "turn finished" {
		t.Errorf("no calls = %q", got)
	}
	// A wait for an earlier turn answers about the later one, and says so.
	later := waitResponse(t, "finished", map[string]any{"turn_id": 87, "requested_turn_id": 82, "turn_number": 5})
	if lines := waitReportLines(&later); !strings.Contains(lines[1], "outcome of turn 87, not of turn 82") {
		t.Errorf("%q", lines)
	}
	// Hard-won: #1450 review: autonomous outcomes hid which turn answered.
	autonomous := waitResponse(t, "finished", map[string]any{"requested_turn_id": 82, "turn_number": 5})
	if got := waitReportLines(&autonomous)[1]; got != "this is the outcome of a turn the session ran without a prompt, not of turn 82; a session keeps only its latest turn's outcome and reply" {
		t.Errorf("autonomous = %q", got)
	}
	// An answer with no finished turn, such as a failed session, is not one.
	earlier := waitResponse(t, "finished", map[string]any{"turn_id": 80, "requested_turn_id": 82, "turn_number": 5})
	if lines := waitReportLines(&earlier); !strings.Contains(lines[1], "outcome of turn 80, not of turn 82") {
		t.Errorf("%q", lines)
	}
	failed := waitResponse(t, "error", map[string]any{"requested_turn_id": 82})
	if lines := waitReportLines(&failed); slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "this is the outcome") }) {
		t.Errorf("%q", lines)
	}
	same := waitResponse(t, "finished", map[string]any{"turn_id": 82, "requested_turn_id": 82})
	if lines := waitReportLines(&same); len(lines) != 1 {
		t.Errorf("%q", lines)
	}
	// What the session does after the turn, in the dashboard's words.
	busy := waitResponse(t, "finished", map[string]any{
		"stop_reason": "EndTurn",
		"next":        map[string]any{"state": "expecting", "since_ms": 1},
	})
	if got := waitReportLines(&busy)[0]; got != "turn finished (completed, end of turn); the session is not idle: expecting the agent to continue" {
		t.Errorf("busy = %q", got)
	}
	waitingForAnswer := waitResponse(t, "timeout", map[string]any{
		"pending_elicitations": []any{map[string]any{"id": "question-1", "message": "Approve?", "fields": []any{}}},
	})
	waitingForAnswer.Session.ID = "session-1"
	// Hard-won: DSH campaign E-2: a prompt timeout hid the pending permission request and its response options.
	if !slices.ContainsFunc(waitReportLines(&waitingForAnswer), func(line string) bool {
		return strings.Contains(line, "mj elicitations --session session-1") &&
			strings.Contains(line, "mj respond --session session-1") &&
			strings.Contains(line, "mj prompt --return-on-input")
	}) {
		t.Errorf("%q", waitReportLines(&waitingForAnswer))
	}
	encoded, err := core.SerdeJSON(finished)
	if err != nil {
		t.Fatal(err)
	}
	var value map[string]any
	if err := json.Unmarshal(encoded, &value); err != nil {
		t.Fatal(err)
	}
	if value["stop_reason"] != "EndTurn" {
		t.Errorf("--json prints the stop reason as the API returns it: %v", value["stop_reason"])
	}
	for _, entry := range []struct{ outcome, stopReason, words string }{
		{"finished", "end_turn", "completed, end of turn"},
		{"input_required", "awaiting_input", "completed, waiting for input"},
		{"cancelled", "Cancelled", "interrupted"},
		{"quota_limit", "QuotaLimit", "failed: quota limit reached"},
		{"error", "MaxTokens", "failed: max tokens"},
		{"error", "prompt_unanswered", "failed: prompt unanswered"},
	} {
		response := waitResponse(t, entry.outcome, map[string]any{"stop_reason": entry.stopReason, "turn_id": 2})
		if got, want := waitReportLines(&response)[0], fmt.Sprintf("turn %s (%s)", entry.outcome, entry.words); got != want {
			t.Errorf("%q, want %q", got, want)
		}
		// The same words `mj sessions --session` prints for that turn.
		session := response.Session
		var last core.MaterializedTurnOutcome
		if err := json.Unmarshal([]byte(fmt.Sprintf(`{
			"command_id": "prompt-1",
			"completed_ordinal": 3,
			"completed_at_ms": 0,
			"outcome": {"kind": "completed", "stop_reason": %q}
		}`, entry.stopReason)), &last); err != nil {
			t.Fatal(err)
		}
		session.LastTurnOutcome = &last
		if got, want := sessionReportLines(&session, 0)[1], "last turn "+entry.words; got != want {
			t.Errorf("%q, want %q", got, want)
		}
	}
}

// F-6: `mj suspend` says to watch with `mj wait`, which then failed with
// "the turn ended as stopped" once the suspension had succeeded.
// Hard-won: 2a521fc5: successful suspension made mj wait report an error and exit nonzero.
func TestAWaitThatSeesAFinishedSuspensionReportsSuccess(t *testing.T) {
	suspended := func(sessionError *string) server.WaitResponse {
		response := waitResponse(t, "stopped", map[string]any{"message": "the session is stopped or stopping"})
		response.Session.Lifecycle = sessionruntime.ViewerLifecycleCategorySuspended
		response.Session.Error = sessionError
		return response
	}

	clean := suspended(nil)
	if got := waitReportLines(&clean); !slices.Equal(got, []string{"session suspended"}) {
		t.Errorf("clean = %q", got)
	}
	if err := reportWait(&clean, false); err != nil {
		t.Errorf("clean suspension: %v", err)
	}

	// A resume that failed also leaves the session stopped, with its reason
	// recorded, and that is still a failure.
	failedResume := suspended(stringPointer("the archive was missing"))
	if err := reportWait(&failedResume, false); err == nil {
		t.Error("a failed resume reported success")
	}
}

// Hard-won: 0877ce6d: the refusal named API fields instead of the --profile and --target flags.
func TestARefusedStartNamesTheFlagsTheUserTyped(t *testing.T) {
	refusal := nameLaunchFlags(fmt.Errorf("the Mjolnir API answered 400 Bad Request: name a profile_id; this instance has no saved default to fall back on"))
	if want := "the Mjolnir API answered 400 Bad Request: name a --profile; this instance has no saved default to fall back on"; refusal.Error() != want {
		t.Errorf("refusal = %q", refusal.Error())
	}
}

// R2-3: the `mj suspend` refusal told the user to "retry with
// acknowledge_unpublished_work=true", the API field. The CLI's flag is
// `--acknowledge-unpublished-work`.
// Hard-won: d7b2f496: the unpublished-work refusal named the API field instead of the CLI flag.
func TestARefusedSuspendNamesTheFlagTheUserTypes(t *testing.T) {
	for _, entry := range []struct{ api, cli string }{
		{
			"the Mjolnir API answered 409 Conflict: publication status is unverified for this live clone; retry with acknowledge_unpublished_work=true to suspend it",
			"the Mjolnir API answered 409 Conflict: publication status is unverified for this live clone; retry with --acknowledge-unpublished-work to suspend it",
		},
		{
			"the Mjolnir API answered 409 Conflict: the checkout has unpublished or unverified work; confirm suspension with acknowledge_unpublished_work=true",
			"the Mjolnir API answered 409 Conflict: the checkout has unpublished or unverified work; confirm suspension with --acknowledge-unpublished-work",
		},
	} {
		if got := nameSuspendFlags(fmt.Errorf("%s", entry.api)).Error(); got != entry.cli {
			t.Errorf("%q, want %q", got, entry.cli)
		}
	}
}

// F-11: a destroyed session that never took a prompt was offered `mj resume
// --wiki`, which then failed with "no prompt to restore from".
// Hard-won: 94305629: a never-prompted archived session was offered a resume that always failed.
func TestAnArchivedRowIsOfferedAResumeOnlyWhenItHasSomethingToRestore(t *testing.T) {
	row := func(nothingToRestore bool) client.WikiSessionInfo {
		return client.WikiSessionInfo{
			WikiID:           "w1",
			Tool:             "mjolnir",
			Path:             "/data/sessions/w1",
			Status:           client.WikiSessionStatusArchived,
			MjolnirSessionID: stringPointer("w1"),
			Title:            "burst3",
			Project:          "",
			NothingToRestore: nothingToRestore,
		}
	}
	restorable := row(false)
	if !slices.ContainsFunc(wikiSessionLines(&restorable), func(line string) bool { return strings.Contains(line, "mj resume --wiki w1") }) {
		t.Errorf("%q", wikiSessionLines(&restorable))
	}
	empty := row(true)
	lines := wikiSessionLines(&empty)
	if slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "mj resume") }) {
		t.Errorf("%q", lines)
	}
	if !slices.ContainsFunc(lines, func(line string) bool { return strings.Contains(line, "never received a prompt") }) {
		t.Errorf("%q", lines)
	}
}

// `mj prompt` sends as the user. Inside a session that would let an agent's
// prompt read exactly as the user's own, so it refuses before it reads any
// input or reaches the daemon (this call has no prompt text, so anything
// past the refusal stops at asking for it).
func TestAPromptFromInsideASessionIsRefusedBeforeItIsSent(t *testing.T) {
	parsed, ok := mustParse(t, "prompt", "--session", "peer-1").(commandPrompt)
	if !ok {
		t.Fatal("expected the prompt subcommand")
	}
	err := promptFrom(context.Background(), parsed.args, stringPointer("caller-1"))
	if err == nil {
		t.Fatal("the prompt was sent")
	}
	if want := "`mj prompt` sends as the user, so it cannot run inside a session (MJ_SESSION_ID is set). Send with `mj message`, then follow with `mj wait`."; err.Error() != want {
		t.Errorf("error = %q", err.Error())
	}
}

// Hard-won: 4209cb10: branch export refusal did not name the required --branch flag.
func TestTheSessionDrivingSubcommandsParseTheirSelectors(t *testing.T) {
	prompt, ok := mustParse(t, "prompt", "--session", "s1", "--wait", "--timeout", "30", "now add a test").(commandPrompt)
	if !ok {
		t.Fatal("expected the prompt subcommand")
	}
	if prompt.args.session != "s1" || prompt.args.text == nil || *prompt.args.text != "now add a test" || prompt.args.timeout == nil || *prompt.args.timeout != 30 {
		t.Errorf("prompt = %+v", prompt.args)
	}
	// `mj prompt` always waits. Scripts written for the retired `--wait`
	// keep working, and help no longer offers it.
	root := newCommandTree(func(cli) error { return nil })
	promptCmd, _, err := root.Find([]string{"prompt"})
	if err != nil {
		t.Fatal(err)
	}
	var retired *clapArg
	for _, arg := range clapOf(promptCmd).args {
		if arg.long == "wait" {
			retired = arg
		}
	}
	if retired == nil {
		t.Fatal("the retired --wait")
	}
	if !retired.hidden {
		t.Error("the retired --wait is offered in help")
	}

	message, ok := mustParse(t, "message", "--session", "s1", "--command-id", "steer-1", "--prompt-file", "steer.md", "--json").(commandMessage)
	if !ok {
		t.Fatal("expected the message subcommand")
	}
	if message.args.commandID == nil || *message.args.commandID != "steer-1" {
		t.Errorf("command id = %v", message.args.commandID)
	}
	if message.args.promptFile == nil || *message.args.promptFile != "steer.md" {
		t.Errorf("prompt file = %v", message.args.promptFile)
	}
	if message.args.text != nil {
		t.Errorf("text = %q", *message.args.text)
	}
	if !message.args.json {
		t.Error("--json not taken")
	}
	if !refused("message", "--session", "s1", "--wait", "go") {
		t.Error("`mj message` sends and returns; it has no --wait")
	}

	wait, ok := mustParse(t, "wait", "--session", "s1", "--turn", "12").(commandWait)
	if !ok {
		t.Fatal("expected the wait subcommand")
	}
	if wait.args.turn == nil || *wait.args.turn != 12 {
		t.Errorf("turn = %v", wait.args.turn)
	}

	transcript, ok := mustParse(t, "transcript", "--session", "s1", "--after-seq", "5", "--limit", "10", "--json").(commandTranscript)
	if !ok {
		t.Fatal("expected the transcript subcommand")
	}
	if transcript.args.afterSeq == nil || *transcript.args.afterSeq != 5 || transcript.args.limit == nil || *transcript.args.limit != 10 || !transcript.args.json {
		t.Errorf("transcript = %+v", transcript.args)
	}
	if len(transcript.args.role) != 0 {
		t.Errorf("roles = %q", transcript.args.role)
	}

	export, ok := mustParse(t, "export", "--session", "s1", "--kind", "bundle", "--out", "work.bundle").(commandExport)
	if !ok {
		t.Fatal("expected the export subcommand")
	}
	if export.args.kind != exportKindArgBundle {
		t.Errorf("kind = %s", export.args.kind)
	}
	if export.args.out == nil || *export.args.out != "work.bundle" {
		t.Errorf("out = %v", export.args.out)
	}

	// Finding R3-11: without a branch name the API answered "a branch export
	// needs a branch name", which named no flag. The command line refuses it
	// first and names `--branch`.
	_, err = parseCommandLine([]string{"export", "--session", "s1", "--kind", "branch"})
	if err == nil {
		t.Fatal("a branch export without --branch is refused")
	}
	if !strings.Contains(err.Error(), "--branch") {
		t.Errorf("%v", err)
	}

	export, ok = mustParse(t, "export", "--session", "s1").(commandExport)
	if !ok {
		t.Fatal("expected the export subcommand")
	}
	if export.args.kind != exportKindArgPatch {
		t.Errorf("kind = %s", export.args.kind)
	}

	destroy, ok := mustParse(t, "destroy", "--session", "s1").(commandDestroy)
	if !ok {
		t.Fatal("expected the destroy subcommand")
	}
	if destroy.args.session != "s1" {
		t.Errorf("session = %q", destroy.args.session)
	}
	if !refused("destroy", "--session", "s1", "--delete-branch") {
		t.Error("--delete-branch parsed")
	}

	suspend, ok := mustParse(t, "suspend", "--session", "s1").(commandSuspend)
	if !ok {
		t.Fatal("expected the suspend subcommand")
	}
	if suspend.args.session == nil || *suspend.args.session != "s1" {
		t.Errorf("session = %v", suspend.args.session)
	}
	if !refused("suspend", "--session", "s1", "--force") {
		t.Error("--force parsed")
	}
	if _, ok := replacementNotice(mustParse(t, "close", "--session", "s1")); !ok {
		t.Error("close names no replacement")
	}

	resume, ok := mustParse(t, "resume", "--session", "s1").(commandResume)
	if !ok {
		t.Fatal("expected the resume subcommand")
	}
	if resume.args.session == nil || *resume.args.session != "s1" {
		t.Errorf("session = %v", resume.args.session)
	}

	{
		restart, ok := mustParse(t, "restart", "--session", "s1").(commandRestart)
		if !ok {
			t.Fatal("expected the restart subcommand")
		}
		if restart.args.session != "s1" {
			t.Errorf("session = %q", restart.args.session)
		}

		move, ok := mustParse(t, "move", "--session", "s1", "--target", "remote", "--skip", "cache/big.bin", "--dry-run").(commandMove)
		if !ok {
			t.Fatal("expected the move subcommand")
		}
		if !slices.Equal(move.args.skipKeepPaths, []string{"cache/big.bin"}) {
			t.Errorf("skip = %q", move.args.skipKeepPaths)
		}
		if !move.args.dryRun {
			t.Error("--dry-run not taken")
		}
		for _, argv := range [][]string{
			{"move", "--session", "s1", "--prepare"},
			{"move", "--session", "s1", "--allow-large-transfer"},
			{"move", "--session", "s1", "--exclude", "repo:a"},
		} {
			if !refused(argv...) {
				t.Errorf("%q parsed", argv)
			}
		}
	}
	if resume.args.profile != nil || resume.args.target != nil || resume.args.queue != nil {
		t.Errorf("resume = %+v", resume.args)
	}

	resume, ok = mustParse(t, "resume", "--session", "s1", "--profile", "deepseek", "--target", "localhost", "--queue", "discard", "--json").(commandResume)
	if !ok {
		t.Fatal("expected the resume subcommand")
	}
	if resume.args.profile == nil || *resume.args.profile != "deepseek" || resume.args.target == nil || *resume.args.target != "localhost" {
		t.Errorf("resume = %+v", resume.args)
	}
	if resume.args.queue == nil || *resume.args.queue != resumeQueueArgDiscard || !resume.args.json {
		t.Errorf("resume = %+v", resume.args)
	}

	diff, ok := mustParse(t, "diff", "--session", "s1", "--base", "HEAD~2", "--json").(commandDiff)
	if !ok {
		t.Fatal("expected the diff subcommand")
	}
	if diff.args.base == nil || *diff.args.base != "HEAD~2" || !diff.args.json {
		t.Errorf("diff = %+v", diff.args)
	}

	for _, entry := range []struct {
		argv    []string
		matched string
	}{
		{[]string{"diff", "--session", "s1"}, "diff"},
		{[]string{"resume", "--session", "s1"}, "resume"},
		{[]string{"sessions", "--json"}, "sessions"},
		{[]string{"suspend", "--session", "s1"}, "suspend"},
		{[]string{"interrupt-turn", "--session", "s1"}, "interrupt-turn"},
		{[]string{"api-info"}, "api-info"},
	} {
		parsed, err := parseCommandLine(entry.argv)
		if err != nil {
			t.Fatalf("%s should parse: %v", entry.matched, err)
		}
		if got := commandName(parsed.command); got != entry.matched {
			t.Errorf("%q: %s", entry.argv, got)
		}
	}
}

func TestTranscriptRoleSelectionIsRepeatableAndDefaultsOmitVerboseRoles(t *testing.T) {
	transcript, ok := mustParse(t, "transcript", "--session", "s1", "--role", "agent", "--role", "user").(commandTranscript)
	if !ok {
		t.Fatal("expected the transcript subcommand")
	}
	if !slices.Equal(transcript.args.role, []core.TranscriptRole{core.TranscriptRoleAgent, core.TranscriptRoleUser}) {
		t.Errorf("roles = %q", transcript.args.role)
	}
	roles, err := requestedTranscriptRoles(&transcript.args)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(roles, transcript.args.role) {
		t.Errorf("requested = %q", roles)
	}

	transcript, ok = mustParse(t, "transcript", "--session", "s1").(commandTranscript)
	if !ok {
		t.Fatal("expected the transcript subcommand")
	}
	if len(transcript.args.role) != 0 {
		t.Errorf("roles = %q", transcript.args.role)
	}
	roles, err = requestedTranscriptRoles(&transcript.args)
	if err != nil {
		t.Fatal(err)
	}
	if want := []core.TranscriptRole{
		core.TranscriptRoleUser,
		core.TranscriptRoleAgent,
		core.TranscriptRoleMessage,
		core.TranscriptRoleThought,
		core.TranscriptRolePlan,
		core.TranscriptRolePlanProposal,
		core.TranscriptRoleSystem,
	}; !slices.Equal(roles, want) {
		t.Errorf("default roles = %q", roles)
	}

	if !refused("transcript", "--session", "s1", "--all-roles") {
		t.Error("--all-roles parsed")
	}
	_, err = parseCommandLine([]string{"transcript", "--session", "s1", "--role", "unknown"})
	if err == nil || !strings.Contains(err.Error(), `unknown transcript role "unknown"`) {
		t.Errorf("%v", err)
	}

	transcript, ok = mustParse(t, "transcript", "--session", "s1", "--finished-only").(commandTranscript)
	if !ok {
		t.Fatal("expected the transcript subcommand")
	}
	if !transcript.args.finishedOnly {
		t.Error("--finished-only not taken")
	}
	if roles, err := requestedTranscriptRoles(&transcript.args); err != nil || !slices.Equal(roles, []core.TranscriptRole{core.TranscriptRoleAgent}) {
		t.Errorf("finished-only roles = %q, %v", roles, err)
	}

	transcript, ok = mustParse(t, "transcript", "--session", "s1", "--role", "agent", "--finished-only").(commandTranscript)
	if !ok {
		t.Fatal("expected the transcript subcommand")
	}
	if roles, err := requestedTranscriptRoles(&transcript.args); err != nil || !slices.Equal(roles, []core.TranscriptRole{core.TranscriptRoleAgent}) {
		t.Errorf("agent finished-only roles = %q, %v", roles, err)
	}

	transcript, ok = mustParse(t, "transcript", "--session", "s1", "--role", "user", "--finished-only").(commandTranscript)
	if !ok {
		t.Fatal("expected the transcript subcommand")
	}
	if _, err := requestedTranscriptRoles(&transcript.args); err == nil || err.Error() != "--finished-only can only be combined with --role agent" {
		t.Errorf("%v", err)
	}
}

// F-16: `mj usage` added text output while keeping partial coverage visible.
func TestUsageTextKeepsTheCoverageBesideTheTotals(t *testing.T) {
	var page core.UsagePage
	if err := json.Unmarshal([]byte(`{
		"session_id": "s1",
		"turns": [],
		"next_after_seq": 4,
		"latest_seq": 4,
		"totals": {"input_tokens": {"tokens": 1200, "reported_turns": 2}},
		"coverage": {
			"recorded_turns": 3, "full_turn_reports": 2, "last_request_reports": 1,
			"unspecified_reports": 0, "missing_reports": 0
		}
	}`), &page); err != nil {
		t.Fatal(err)
	}
	lines := usageLines(&page)
	want := []string{
		"totals from the 2 of 3 turns that reported a whole turn:",
		"  input_tokens  1200  (2 turns)",
		"not in the totals: 1 reported only their last request (turns)",
	}
	if len(lines) < 3 || !slices.Equal(lines[:3], want) {
		t.Errorf("lines = %q", lines)
	}
}

func TestGoldenAPIUsageTree(t *testing.T) {
	var tree core.UsageTree
	if err := json.Unmarshal([]byte(`{
		"parent_session_id": "session-parent",
		"sessions": [
			{
				"session_id": "session-parent",
				"parent_session_id": null,
				"task_name": null,
				"operational_session_present": true,
				"totals": {},
				"coverage": {
					"recorded_turns": 0,
					"full_turn_reports": 0,
					"last_request_reports": 0,
					"unspecified_reports": 0,
					"missing_reports": 0,
					"unfinished_turns": 0
				},
				"by_model": [],
				"provider_session_cost": {
					"amount": 0.0123,
					"currency": "USD",
					"observed_at_ms": 1
				}
			},
			{
				"session_id": "session-reviewer",
				"parent_session_id": "session-parent",
				"task_name": "Review the migration plan",
				"operational_session_present": false,
				"totals": {},
				"coverage": {
					"recorded_turns": 0,
					"full_turn_reports": 0,
					"last_request_reports": 0,
					"unspecified_reports": 0,
					"missing_reports": 0,
					"unfinished_turns": 0
				},
				"by_model": [],
				"provider_session_cost": {
					"amount": 0.0045,
					"currency": "USD",
					"observed_at_ms": 2
				}
			}
		],
		"totals": {
			"input_tokens": { "tokens": 2400, "reported_turns": 3 },
			"output_tokens": { "tokens": 650, "reported_turns": 3 }
		},
		"coverage": {
			"recorded_turns": 5,
			"full_turn_reports": 3,
			"last_request_reports": 1,
			"unspecified_reports": 0,
			"missing_reports": 1,
			"unfinished_turns": 1
		},
		"by_model": [
			{
				"model": "gpt-5.4",
				"effort": "high",
				"totals": {
					"input_tokens": { "tokens": 1800, "reported_turns": 2 },
					"output_tokens": { "tokens": 500, "reported_turns": 2 }
				}
			},
			{
				"model": null,
				"effort": null,
				"totals": {
					"cache_read_tokens": { "tokens": 600, "reported_turns": 1 }
				}
			}
		]
	}`), &tree); err != nil {
		t.Fatalf("usage tree fixture: %v", err)
	}
	output := "=== parent session usage tree (CLI text) ===\n" + strings.Join(usageTreeLines(&tree), "\n")
	core.AssertGolden(t, "testdata", "api-usage-tree", output)
}

// Hard-won: #1501: Terra was missing from the shared pricing table.
func TestAPIModelsMatchesCLIGolden(t *testing.T) {
	choices := core.ProfileConfig{
		Model: stringPointer("deepseek-flash"),
		Models: []core.SessionConfigChoice{
			{Value: "gpt-6.1-terra", Name: "GPT-6.1 Terra"},
			{Value: "deepseek-flash", Name: "DeepSeek Flash"},
			{Value: "unknown-model", Name: "Unknown Model"},
		},
		Efforts:    []core.SessionConfigChoice{},
		ObservedAt: 1,
	}
	output := "=== mj models text and JSON output ==="
	for _, entry := range []struct {
		name    string
		billing client.QuotaBilling
	}{
		{"usage", client.QuotaBillingUsage},
		{"subscription", client.QuotaBillingSubscription},
		{"unknown", client.QuotaBillingUnknown},
	} {
		text, err := renderModels(&choices, entry.billing, false)
		if err != nil {
			t.Fatal(err)
		}
		output += fmt.Sprintf("\n--- %s text ---\n", entry.name) + text
		encoded, err := renderModels(&choices, entry.billing, true)
		if err != nil {
			t.Fatal(err)
		}
		output += fmt.Sprintf("\n--- %s JSON ---\n", entry.name) + encoded
	}
	core.AssertGolden(t, "testdata", "api-models", output)
}

func TestAPIModelsAggregateMatchesCLIGolden(t *testing.T) {
	choices := func(models ...[2]string) core.ProfileConfig {
		config := core.ProfileConfig{Models: []core.SessionConfigChoice{}, Efforts: []core.SessionConfigChoice{}, ObservedAt: 1}
		for _, model := range models {
			config.Models = append(config.Models, core.SessionConfigChoice{Value: model[0], Name: model[1]})
		}
		return config
	}
	results := []profileModels{
		{profile: "claude-main", config: choices([2]string{"claude-opus-4-7", "Claude Opus 4.7"}, [2]string{"claude-sonnet-4-6", "Claude Sonnet 4.6"})},
		{profile: "claude-backup", config: choices([2]string{"anthropic.claude-opus-4-7", "Opus"})},
		{profile: "codex", config: choices([2]string{"gpt-5.3-codex", "GPT-5.3 Codex"})},
		{profile: "grok", err: fmt.Errorf("profile discovery failed: login expired")},
	}
	text, err := renderModelsAggregate(results, false)
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := renderModelsAggregate(results, true)
	if err != nil {
		t.Fatal(err)
	}
	none, err := renderModelsAggregate(nil, false)
	if err != nil {
		t.Fatal(err)
	}
	output := "=== mj models text ===\n" + text + "\n=== mj models --json ===\n" + encoded + "\n=== mj models, nothing offered ===\n" + none
	core.AssertGolden(t, "testdata", "api-models-aggregate", output)
}

// Hard-won: #1566: a timed-out quota feed left the control client unusable for disabled-profile probes.
func TestQuotaFeedTimeoutPreservesTheControlConnection(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	address, err := netip.ParseAddrPort(listener.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	metadata := client.DaemonMetadata{
		ProtocolVersion: client.ProtocolVersion,
		Pid:             uint32(os.Getpid()),
		Address:         address,
		Token:           "quota-feed-timeout",
		StartedAt:       "test",
		BuildVersion:    core.PackageVersion,
		InstanceID:      "",
		Placement:       client.DaemonPlacement{Kind: client.DaemonPlacementKindHost},
	}
	received := make(chan struct{})
	served := make(chan error, 1)
	go func() {
		served <- func() error {
			control, err := listener.Accept()
			if err != nil {
				return err
			}
			defer control.Close()
			feed, err := listener.Accept()
			if err != nil {
				return err
			}
			defer feed.Close()
			request, err := client.ReadFrame[client.RequestEnvelope](feed)
			if err != nil {
				return err
			}
			if _, ok := request.Action.(client.DaemonActionRuntimeChanges); !ok {
				return fmt.Errorf("the feed asked %T", request.Action)
			}
			close(received)
			var one [1]byte
			if read, err := feed.Read(one[:]); read != 0 || err != io.EOF {
				return fmt.Errorf("the feed connection stayed open: read %d, %v", read, err)
			}
			request, err = client.ReadFrame[client.RequestEnvelope](control)
			if err != nil {
				return err
			}
			if _, ok := request.Action.(client.DaemonActionProbeQuotaProfiles); !ok {
				return fmt.Errorf("the control connection asked %T", request.Action)
			}
			return client.WriteFrame(control, client.ResponseEnvelope{
				ProtocolVersion: request.ProtocolVersion,
				RequestID:       request.RequestID,
				Result:          client.DaemonOk[client.DaemonReply](client.DaemonReplyDone{}),
			})
		}()
	}()
	ctx := context.Background()
	control, err := client.DaemonClientConnect(ctx, metadata)
	if err != nil {
		t.Fatal(err)
	}
	defer control.Close()
	feed, err := client.DaemonClientConnect(ctx, metadata)
	if err != nil {
		t.Fatal(err)
	}
	// The deadline is QUOTA_REFRESH_WAIT away on a clock the test owns:
	// firing the timer is tokio's paused clock advanced past it.
	deadline := make(chan time.Time)
	type pumped struct {
		metadata client.RuntimeMetadata
		err      error
	}
	pump := make(chan pumped, 1)
	go func() {
		_, latest, err := pumpQuotaFeedUntil(ctx, feed, nil, client.RuntimeMetadata{}, deadline,
			func(*client.RuntimeMetadata) bool { return false })
		pump <- pumped{latest, err}
	}()
	select {
	case result := <-pump:
		t.Fatalf("feed reply remains pending: %+v", result)
	case <-received:
	}
	close(deadline)
	result := <-pump
	if result.err != nil {
		t.Fatal(result.err)
	}
	if observed := (client.RuntimeMetadata{}); !reflect.DeepEqual(result.metadata, observed) {
		t.Errorf("latest = %+v, want the metadata observed before", result.metadata)
	}
	if err := control.ProbeQuotaProfiles(ctx, []string{"disabled"}, false); err != nil {
		t.Fatal(err)
	}
	if err := <-served; err != nil {
		t.Fatal(err)
	}
}

func TestAPIQuotaMatchesCLIGolden(t *testing.T) {
	now := uint64(1_800_000_000)
	window := func(label string, remaining *uint8, used, limit *int64, resetsIn *uint64) client.QuotaWindow {
		var resetsAt *int64
		if resetsIn != nil {
			at := int64(now + *resetsIn)
			resetsAt = &at
		}
		return client.QuotaWindow{Label: label, RemainingPercent: remaining, Used: used, Limit: limit, ResetsAtEpochSeconds: resetsAt}
	}
	percent := func(value uint8) *uint8 { return &value }
	count := func(value int64) *int64 { return &value }
	seconds := func(value uint64) *uint64 { return &value }
	report := func(profile string, harness core.HarnessKind, windows []client.QuotaWindow, extra, failure *string, age uint64) client.ProfileQuota {
		return client.ProfileQuota{
			ProfileID:               profile,
			Harness:                 harness,
			Windows:                 windows,
			Extra:                   extra,
			Error:                   failure,
			RefreshedAtEpochSeconds: now - age,
		}
	}
	config := core.DefaultConfig()
	if config.Profiles == nil {
		config.Profiles = map[string]core.HarnessProfile{}
	}
	for _, entry := range []struct {
		id      string
		kind    core.HarnessKind
		enabled bool
	}{
		{"agy", core.HarnessKindAntigravity, true},
		{"claude", core.HarnessKindClaude, true},
		{"codex", core.HarnessKindCodex, true},
		{"kimi", core.HarnessKindKimi, true},
		{"muse", core.HarnessKindMuse, true},
		{"opencode", core.HarnessKindOpenCode, true},
		{"dsh", core.HarnessKindDsh, false},
	} {
		profile := core.NewHarnessProfile(entry.kind, "/home/agent/."+entry.id)
		profile.Enabled = entry.enabled
		config.Profiles[entry.id] = profile
	}
	claude := report("claude", core.HarnessKindClaude, []client.QuotaWindow{
		window("5H", percent(62), nil, nil, seconds(2*3600+41*60)),
		window("Week", nil, count(340), count(500), seconds(3*24*3600+4*3600)),
	}, nil, nil, 240)
	price := uint32(100)
	claude.MonthlyPriceUsd = &price
	// A Pro plan reports Argon apart, and an unused pool has no reset.
	poolWindow := func(pool core.AntigravityQuotaPool, label string, remaining uint8, resetsIn *uint64) client.QuotaWindow {
		scoped := window(label, percent(remaining), nil, nil, resetsIn)
		scoped.ModelScope = stringPointer(pool.ReportedScope(true))
		return scoped
	}
	agy := report("agy", core.HarnessKindAntigravity, []client.QuotaWindow{
		poolWindow(core.AntigravityQuotaPoolGemini, "Week", 73, seconds(6*24*3600)),
		poolWindow(core.AntigravityQuotaPoolGemini, "5H", 100, seconds(5*3600)),
		poolWindow(core.AntigravityQuotaPoolClaude, "Week", 100, nil),
		poolWindow(core.AntigravityQuotaPoolClaude, "5H", 100, nil),
		poolWindow(core.AntigravityQuotaPoolArgon, "Week", 40, seconds(6*24*3600+23*3600)),
	}, nil, nil, 90)
	codex := report("codex", core.HarnessKindCodex, []client.QuotaWindow{}, stringPointer(client.APILabel), nil, 60)
	codex.PaidCredits = client.NewPaidCreditsLimited(43.2)
	kimi := report("kimi", core.HarnessKindKimi, []client.QuotaWindow{}, stringPointer(client.NotReportedLabel), nil, 30)
	grokProfile := core.NewHarnessProfile(core.HarnessKindGrok, "/home/agent/.grok")
	grokProfile.Enabled = true
	config.Profiles["grok"] = grokProfile
	grok := report("grok", core.HarnessKindGrok, []client.QuotaWindow{}, nil, stringPointer("login expired"), 600)
	var snapshot client.QuotaSnapshot
	if err := json.Unmarshal([]byte(`{"reports": {}, "probing": ["opencode"], "cycles": 7}`), &snapshot); err != nil {
		t.Fatal(err)
	}
	snapshot.Reports = map[string]client.ProfileQuota{"agy": agy, "claude": claude, "codex": codex, "kimi": kimi, "grok": grok}
	metadata := client.RuntimeMetadata{
		Config: core.NewSharedSnapshot(config),
		Quotas: core.NewSharedSnapshot(snapshot),
	}
	render := func(only *string, withDisabled, json bool) string {
		t.Helper()
		rendered, err := renderQuotaAt(&metadata, only, withDisabled, json, now)
		if err != nil {
			t.Fatal(err)
		}
		return rendered
	}
	output := "=== mj quota text ===\n" + render(nil, false, false)
	output += "\n=== mj quota --profile claude ===\n" + render(stringPointer("claude"), false, false)
	output += "\n=== mj quota --json ===\n" + render(nil, false, true)
	output += "\n=== mj quota --with-disabled ===\n" + render(nil, true, false)
	_, missing := renderQuotaAt(&metadata, stringPointer("missing"), false, false, now)
	if missing == nil {
		t.Fatal("a missing profile was rendered")
	}
	output += "\n=== mj quota --profile missing ===\n" + missing.Error()
	core.AssertGolden(t, "testdata", "api-quota", output)
}

func TestAPIHostsMatchesCLIGolden(t *testing.T) {
	const gib = uint64(1 << 30)
	now := uint64(1_700_000_000)
	filesystem := func(host, mount string, available, total uint64, condition core.StorageCondition) core.HostFilesystemRow {
		return core.HostFilesystemRow{Host: host, Mount: mount, AvailableBytes: available, TotalBytes: total, Condition: condition}
	}
	cpu := func(value uint8) *uint8 { return &value }
	sampled := func(value uint64) *uint64 { return &value }
	instances := func(value int) *int { return &value }
	row := func(id string, kind core.DeploymentCapacityKind, targets ...string) core.HostCapacityRow {
		return core.HostCapacityRow{
			ID:                    id,
			Host:                  id,
			TargetIDs:             targets,
			Kind:                  kind,
			HasReading:            true,
			CPUPercent:            cpu(12),
			MemoryUsedBytes:       14 * gib,
			MemoryTotalBytes:      31 * gib,
			LogicalCores:          16,
			Filesystems:           []core.HostFilesystemRow{},
			SampledAtEpochSeconds: sampled(now - 20),
		}
	}
	local := row("local", core.DeploymentCapacityKindHost, "local", "podman")
	local.Filesystems = []core.HostFilesystemRow{
		filesystem("local", "/", 300*gib, 900*gib, core.StorageConditionOk),
		filesystem("local", "/mnt/optane", 3*gib, 400*gib, core.StorageConditionLow),
	}
	build := row("build.example", core.DeploymentCapacityKindHost, "build")
	build.CPUPercent = nil
	build.SampledAtEpochSeconds = sampled(now - 600)
	build.ProbeError = stringPointer("ssh: connection timed out")
	build.Filesystems = []core.HostFilesystemRow{filesystem("build.example", "/srv", gib/2, 200*gib, core.StorageConditionFull)}
	fleet := row("ec2-fleet", core.DeploymentCapacityKindAwsFleet, "ec2")
	fleet.FleetInstances = instances(2)
	fleet.CPUPercent = nil
	fleet.LogicalCores = 96
	fleet.MemoryTotalBytes = 384 * gib
	fleet.Filesystems = []core.HostFilesystemRow{
		filesystem("i-0a1", "/mnt/ebs", 80*gib, 100*gib, core.StorageConditionOk),
		filesystem("i-0b2", "/mnt/ebs", 60*gib, 100*gib, core.StorageConditionOk),
	}
	idle := row("ec2-idle", core.DeploymentCapacityKindAwsFleet, "ec2-spare")
	idle.FleetInstances = instances(0)
	idle.HasReading = false
	idle.OnDemand = true
	// The fleet last answered empty, then its probe failed: the size is
	// unknown, so the row must not claim "0 VMs".
	failedIdle := row("ec2-failed", core.DeploymentCapacityKindAwsFleet, "ec2-broken")
	failedIdle.FleetInstances = instances(0)
	failedIdle.HasReading = false
	failedIdle.ProbeError = stringPointer("describe-instances: access denied")
	fresh := row("new.example", core.DeploymentCapacityKindHost, "new")
	fresh.HasReading = false
	fresh.SampledAtEpochSeconds = nil
	// The rows travel from the daemon as JSON.
	encoded, err := core.SerdeJSON([]core.HostCapacityRow{local, build, fleet, idle, failedIdle, fresh})
	if err != nil {
		t.Fatal(err)
	}
	var rows []core.HostCapacityRow
	if err := json.Unmarshal(encoded, &rows); err != nil {
		t.Fatal(err)
	}
	render := func(only *string, json bool) string {
		t.Helper()
		rendered, err := renderHosts(rows, only, json, now)
		if err != nil {
			t.Fatal(err)
		}
		return rendered
	}
	output := "=== mj hosts ===\n" + render(nil, false)
	output += "\n=== mj hosts --host podman ===\n" + render(stringPointer("podman"), false)
	output += "\n=== mj hosts --host ec2-fleet --json ===\n" + render(stringPointer("ec2-fleet"), true)
	_, missing := renderHosts(rows, stringPointer("missing"), false, now)
	if missing == nil {
		t.Fatal("a missing host was rendered")
	}
	output += "\n=== mj hosts --host missing ===\n" + missing.Error()
	core.AssertGolden(t, "testdata", "api-hosts", output)
}

func TestGoldenSuspendRefusalAndDryRunOutput(t *testing.T) {
	refusal := core.FormatUntrackedRefusal(6, []string{"a.txt", "b.txt", "c.txt", "d.txt", "e.txt", "f.txt"})
	delta, state := uint64(1024*64), uint64(4096)
	dryRunClean := formatSuspendDryRun(&core.SuspendPreflight{
		SessionID: "session-1",
		Untracked: []core.UntrackedPath{{
			Checkout: core.DefaultCheckoutKey(),
			Path:     "scratch.txt",
			Bytes:    512,
			Identity: core.NewFileIdentity("abc"),
		}},
		KeepDirBytes: 2048,
		DryRun: &core.DryRunEstimate{
			GitDeltaBytes:     &delta,
			SessionStateBytes: &state,
			KeepDirBytes:      2048,
		},
	})
	dryRunRebase := formatSuspendDryRun(&core.SuspendPreflight{
		SessionID:  "session-2",
		Operations: []core.ShownOperation{{Checkout: core.DefaultCheckoutKey(), Kind: core.GitOperationKindRebase}},
		DryRun: &core.DryRunEstimate{
			GitDeltaUnavailableReason: stringPointer("Git size estimate unavailable during rebase"),
			KeepDirBytes:              0,
		},
	})
	output := fmt.Sprintf("=== untracked refusal ===\n%s\n\n=== dry run ===\n%s\n=== dry run during rebase ===\n%s", refusal, dryRunClean, dryRunRebase)
	core.AssertGolden(t, "testdata", "api-suspend-consent", output)
}

func TestSuspendPreflightQueryIncludesWithoutCheckpoint(t *testing.T) {
	for _, entry := range []struct {
		dryRun, withoutCheckpoint bool
		want                      string
	}{
		{false, false, ""},
		{true, false, "?dry_run=true"},
		{false, true, "?without_checkpoint=true"},
		{true, true, "?dry_run=true&without_checkpoint=true"},
	} {
		if got := apiClientSuspendPreflightQuery(entry.dryRun, entry.withoutCheckpoint); got != entry.want {
			t.Errorf("(%v, %v) = %q, want %q", entry.dryRun, entry.withoutCheckpoint, got, entry.want)
		}
	}
}

func TestPurgeUntrackedParsesItsSelectorAndRendersEachSession(t *testing.T) {
	purge, ok := mustParse(t, "purge-untracked", "--session", "session-1", "--dry-run", "--json").(commandPurgeUntracked)
	if !ok {
		t.Fatal("expected purge-untracked")
	}
	if purge.args.session == nil || *purge.args.session != "session-1" || purge.args.all || !purge.args.dryRun || !purge.args.json {
		t.Errorf("args = %+v", purge.args)
	}

	purge, ok = mustParse(t, "purge-untracked", "--all").(commandPurgeUntracked)
	if !ok {
		t.Fatal("expected purge-untracked")
	}
	if purge.args.session != nil || !purge.args.all || purge.args.dryRun || purge.args.json {
		t.Errorf("args = %+v", purge.args)
	}

	for _, argv := range [][]string{
		{"purge-untracked"},
		{"purge-untracked", "--session", "session-1", "--all"},
		{"purge-untracked", "--dry-run"},
	} {
		if !refused(argv...) {
			t.Errorf("%q parsed", argv)
		}
	}

	response := samplePurgeReport(false)
	if got, want := purgeUntrackedLines(&response), []string{
		"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: 4096 reclaimed",
		"  stash project abcdef (WIP notes)",
		"  warning: could not read one stash",
		"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: skipped (lifecycle operation in progress)",
		"Total: 4096 reclaimed",
	}; !slices.Equal(got, want) {
		t.Errorf("lines = %q", got)
	}
	dry := samplePurgeReport(true)
	if got, want := purgeUntrackedLines(&dry), []string{
		"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: would reclaim 4096",
		"  stash project abcdef (WIP notes)",
		"  warning: could not read one stash",
		"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: skipped (lifecycle operation in progress)",
		"Total: 4096 would be reclaimed",
	}; !slices.Equal(got, want) {
		t.Errorf("dry lines = %q", got)
	}
	encoded, err := core.SerdeJSONPretty(response)
	if err != nil {
		t.Fatal(err)
	}
	if want := `{
  "sessions": [
    {
      "session_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
      "status": "purged",
      "bytes_reclaimed": 4096,
      "reason": null,
      "stash_untracked_notes": [
        {
          "repository_id": "project",
          "commit": "abcdef",
          "message": "WIP notes"
        }
      ],
      "warnings": [
        "could not read one stash"
      ]
    },
    {
      "session_id": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
      "status": "skipped",
      "bytes_reclaimed": 0,
      "reason": "lifecycle operation in progress",
      "stash_untracked_notes": [],
      "warnings": []
    }
  ],
  "total_bytes_reclaimed": 4096,
  "dry_run": false
}`; string(encoded) != want {
		t.Errorf("JSON =\n%s", encoded)
	}
	failed := samplePurgeReport(false)
	failed.Sessions = append(failed.Sessions, server.PurgeUntrackedSession{
		SessionID:           "cccccccccccccccccccccccccccccccc",
		Status:              server.PurgeUntrackedStatusFailed,
		BytesReclaimed:      0,
		Reason:              stringPointer("checkpoint archive is missing"),
		StashUntrackedNotes: []server.PurgeUntrackedStashNote{},
		Warnings:            []string{},
	})
	lines := purgeUntrackedLines(&failed)
	if lines[len(lines)-2] != "cccccccccccccccccccccccccccccccc: failed (checkpoint archive is missing)" {
		t.Errorf("lines = %q", lines)
	}
	if !strings.HasSuffix(lines[len(lines)-1], "4096 reclaimed") {
		t.Errorf("lines = %q", lines)
	}
	encoded, err = core.SerdeJSONPretty(failed)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(encoded), `"status": "failed"`) || !strings.Contains(string(encoded), "checkpoint archive is missing") {
		t.Errorf("JSON =\n%s", encoded)
	}
}

func TestPurgeUntrackedAllPrintsTheFullReportAndFailsWhenASessionFailed(t *testing.T) {
	purge, ok := mustParse(t, "purge-untracked", "--all").(commandPurgeUntracked)
	if !ok {
		t.Fatal("expected purge-untracked")
	}
	if !purge.args.all || purge.args.json {
		t.Errorf("args = %+v", purge.args)
	}

	response := samplePurgeReport(false)
	response.Sessions = append(response.Sessions, server.PurgeUntrackedSession{
		SessionID:           "cccccccccccccccccccccccccccccccc",
		Status:              server.PurgeUntrackedStatusFailed,
		BytesReclaimed:      0,
		Reason:              stringPointer("checkpoint archive is missing"),
		StashUntrackedNotes: []server.PurgeUntrackedStashNote{},
		Warnings:            []string{},
	})
	report, status := purgeUntrackedConclusion(&response, false)
	for _, line := range []string{
		"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb: 4096 reclaimed",
		"cccccccccccccccccccccccccccccccc: failed (checkpoint archive is missing)",
		"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa: skipped",
	} {
		if !strings.Contains(report, line) {
			t.Errorf("report lacks %q:\n%s", line, report)
		}
	}
	if !strings.HasSuffix(report, "Total: 4096 reclaimed") {
		t.Errorf("report =\n%s", report)
	}
	if status == nil || status.Error() != "1 session(s) failed to purge untracked payloads" {
		t.Errorf("a failed session fails the command: %v", status)
	}

	encoded, status := purgeUntrackedConclusion(&response, true)
	for _, fragment := range []string{`"status": "purged"`, `"status": "failed"`, "checkpoint archive is missing", `"bytes_reclaimed": 4096`} {
		if !strings.Contains(encoded, fragment) {
			t.Errorf("JSON lacks %q:\n%s", fragment, encoded)
		}
	}
	if status == nil || status.Error() != "1 session(s) failed to purge untracked payloads" {
		t.Errorf("json still fails after the report: %v", status)
	}

	clean := samplePurgeReport(false)
	report, status = purgeUntrackedConclusion(&clean, false)
	if !strings.Contains(report, "4096 reclaimed") {
		t.Errorf("report =\n%s", report)
	}
	if status != nil {
		t.Errorf("a clean purge failed: %v", status)
	}
}

func samplePurgeReport(dryRun bool) server.PurgeUntrackedResponse {
	status := server.PurgeUntrackedStatusPurged
	if dryRun {
		status = server.PurgeUntrackedStatusDryRun
	}
	return server.PurgeUntrackedResponse{
		Sessions: []server.PurgeUntrackedSession{
			{
				SessionID:      "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
				Status:         status,
				BytesReclaimed: 4096,
				Reason:         nil,
				StashUntrackedNotes: []server.PurgeUntrackedStashNote{{
					RepositoryID: "project",
					Commit:       "abcdef",
					Message:      "WIP notes",
				}},
				Warnings: []string{"could not read one stash"},
			},
			{
				SessionID:           "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
				Status:              server.PurgeUntrackedStatusSkipped,
				BytesReclaimed:      0,
				Reason:              stringPointer("lifecycle operation in progress"),
				StashUntrackedNotes: []server.PurgeUntrackedStashNote{},
				Warnings:            []string{},
			},
		},
		TotalBytesReclaimed: 4096,
		DryRun:              dryRun,
	}
}
