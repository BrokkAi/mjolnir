// Port of the tests in mj-worker-mcp/src/subagent_mcp.rs that serve a fake
// worker on a Unix socket (Rust's #[cfg(unix)] tests).

//go:build unix

package workermcp

import (
	"bufio"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// subagentSocket is a socket path in a short temporary directory: a Unix
// socket path must fit in about a hundred bytes, which a test's own
// temporary directory can exceed.
func subagentSocket(t *testing.T) string {
	t.Helper()
	directory, err := os.MkdirTemp("", "mj-subagents")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(directory) })
	return filepath.Join(directory, "subagents.sock")
}

func subagentListen(t *testing.T, socket string) net.Listener {
	t.Helper()
	listener, err := net.Listen("unix", socket)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = listener.Close() })
	return listener
}

// subagentServeOne accepts one connection, reads its request line and hands
// both to serve, closing the connection afterwards. Its result arrives on
// the returned channel.
func subagentServeOne(listener net.Listener, serve func(conn net.Conn, reader *bufio.Reader, line []byte) error) <-chan error {
	done := make(chan error, 1)
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			done <- err
			return
		}
		defer conn.Close()
		reader := bufio.NewReader(conn)
		line, err := reader.ReadBytes('\n')
		if err != nil {
			done <- err
			return
		}
		done <- serve(conn, reader, line)
	}()
	return done
}

func subagentWriteLine(conn net.Conn, value any) error {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return err
	}
	_, err = conn.Write(append(encoded, '\n'))
	return err
}

func subagentParseLine(line []byte) (any, error) {
	return jsonvalue.Parse([]byte(strings.TrimSpace(string(line))))
}

func TestSpawnWithOnlyStraightforwardSendsStraightforward(t *testing.T) {
	socket := subagentSocket(t)
	listener := subagentListen(t, socket)
	worker := subagentServeOne(listener, func(conn net.Conn, _ *bufio.Reader, line []byte) error {
		var request core.SubagentToolRequest
		if err := json.Unmarshal(line, &request); err != nil {
			return err
		}
		spawn, ok := request.Action.(core.SubagentToolActionSpawn)
		if !ok || spawn.TaskType != core.SubagentTaskTypeStraightforward || !spawn.NoGithubCredentials {
			return fmt.Errorf("sent %#v", request.Action)
		}
		_, err := conn.Write([]byte(`{"result":{"child_session_id":"child"}}` + "\n"))
		return err
	})
	value, isError, err := testCallWithOptions(socket, nil, core.SubagentMcpRoleParent,
		`{"name":"spawn_worker","arguments":{"task_name":"t","instructions":"i","no_github_credentials":true}}`,
		ProgressSilent(time.Second), 5*time.Second, subagentStraightforwardOnly)
	if err != nil {
		t.Fatal(err)
	}
	if err := <-worker; err != nil {
		t.Fatal(err)
	}
	if isError || subagentText(value, "child_session_id") != "child" {
		t.Fatal(value)
	}
}

// Hard-won: 32d96d199: sessions without delegation could not request push reviews under their own role.
func TestReviewToolsSendDurableActionsThroughTheSameRequestPath(t *testing.T) {
	other := "/other"
	for _, check := range []struct {
		params string
		action core.SubagentToolAction
	}{
		{
			`{"name":"request_review","arguments":{"paths":["/workspace","/other"]}}`,
			core.SubagentToolActionRequestReview{Paths: []string{"/workspace", "/other"}},
		},
		{
			`{"name":"request_review","arguments":{}}`,
			core.SubagentToolActionRequestReview{},
		},
		{
			`{"name":"override_review","arguments":{"overrides":[
				{"finding":"  the parser accepts a bare tab ","reason":" it is a documented extension "},
				{"finding":"error text names the wrong file","reason":"the file is generated"}
			]}}`,
			core.SubagentToolActionOverrideReview{Overrides: []core.ReviewOverride{
				{Finding: "the parser accepts a bare tab", Reason: "it is a documented extension"},
				{Finding: "error text names the wrong file", Reason: "the file is generated"},
			}},
		},
		{
			`{"name":"set_baseline","arguments":{"commit":" abc1234 ","path":"/other"}}`,
			core.SubagentToolActionSetReviewBaseline{Commit: "abc1234", Path: &other},
		},
		{
			`{"name":"set_baseline","arguments":{"commit":"abc1234"}}`,
			core.SubagentToolActionSetReviewBaseline{Commit: "abc1234"},
		},
	} {
		socket := subagentSocket(t)
		listener := subagentListen(t, socket)
		worker := subagentServeOne(listener, func(conn net.Conn, _ *bufio.Reader, line []byte) error {
			var request core.SubagentToolRequest
			if err := json.Unmarshal(line, &request); err != nil {
				return err
			}
			if !reflect.DeepEqual(request.Action, check.action) {
				return fmt.Errorf("sent %#v, want %#v", request.Action, check.action)
			}
			_, err := conn.Write([]byte(`{"result":{"accepted":true}}` + "\n"))
			return err
		})
		value, isError, err := callReview(socket, json.RawMessage(check.params), ProgressSilent(time.Second),
			func(core.SubagentToolAction) time.Duration { return 5 * time.Second })
		if err != nil {
			t.Fatal(err)
		}
		if err := <-worker; err != nil {
			t.Fatal(err)
		}
		if isError || subagentValue(value, "accepted") != true {
			t.Fatal(value)
		}
	}
}

func TestSpawnOrchestratorDispatchesTheNewWireAction(t *testing.T) {
	socket := subagentSocket(t)
	listener := subagentListen(t, socket)
	target := "remote"
	repos := []string{"https://example.com/code.git"}
	want := core.SubagentToolActionSpawnOrchestrator{
		TaskName:            "plan",
		Instructions:        "coordinate",
		Target:              &target,
		Repos:               &repos,
		NoGithubCredentials: true,
	}
	worker := subagentServeOne(listener, func(conn net.Conn, _ *bufio.Reader, line []byte) error {
		var request core.SubagentToolRequest
		if err := json.Unmarshal(line, &request); err != nil {
			return err
		}
		if !reflect.DeepEqual(request.Action, want) {
			return fmt.Errorf("sent %#v, want %#v", request.Action, want)
		}
		_, err := conn.Write([]byte(`{"result":{"child_session_id":"child"}}` + "\n"))
		return err
	})
	options := DefaultParentTools()
	options.OrchestratorsEnabled = true
	value, isError, err := testCallWithOptions(socket, nil, core.SubagentMcpRoleParent,
		`{"name":"spawn_orchestrator","arguments":{"task_name":"plan","instructions":"coordinate","target":"remote","repos":["https://example.com/code.git"],"no_github_credentials":true}}`,
		ProgressSilent(time.Second), 5*time.Second, options)
	if err != nil {
		t.Fatal(err)
	}
	if err := <-worker; err != nil {
		t.Fatal(err)
	}
	if isError || subagentText(value, "child_session_id") != "child" {
		t.Fatal(value)
	}
}

func TestWorkerReplyAcknowledgmentFollowsTheCompleteLargeReply(t *testing.T) {
	socket := subagentSocket(t)
	listener := subagentListen(t, socket)
	report := strings.Repeat("large report λ", 10_000)
	worker := make(chan error, 1)
	go func() {
		worker <- func() error {
			conn, err := listener.Accept()
			if err != nil {
				return err
			}
			line, err := bufio.NewReader(conn).ReadBytes('\n')
			if err != nil {
				return err
			}
			request, err := subagentParseLine(line)
			if err != nil {
				return err
			}
			message, err := jsonvalue.Marshal(subagentObject("report", report))
			if err != nil {
				return err
			}
			if err := subagentWriteLine(conn, subagentObject(
				"accepted", true,
				"acknowledgment_required", true,
				"result", subagentObject(
					"request_id", subagentValue(request, "request_id"),
					"completed_at_ms", 2,
					"is_error", false,
					"message", string(message),
				),
			)); err != nil {
				return err
			}
			_ = conn.Close()
			conn, err = listener.Accept()
			if err != nil {
				return err
			}
			defer conn.Close()
			line, err = bufio.NewReader(conn).ReadBytes('\n')
			if err != nil {
				return err
			}
			acknowledgment, err := subagentParseLine(line)
			if err != nil {
				return err
			}
			if !jsonvalue.Equal(subagentValue(acknowledgment, "acknowledge_result"), subagentValue(request, "request_id")) {
				return fmt.Errorf("acknowledged %v", acknowledgment)
			}
			_, err = conn.Write([]byte(`{"accepted":true}` + "\n"))
			return err
		}()
	}()
	reply, isError, err := subagentCallWithBudget(socket, nil, core.SubagentMcpRoleParent,
		`{"name":"list_agents"}`, ProgressSilent(50*time.Millisecond), 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if err := <-worker; err != nil {
		t.Fatal(err)
	}
	if isError || subagentText(reply, "report") != report {
		t.Fatal("the large report did not arrive whole")
	}
}

func TestReportBudgetDoesNotFollowFileOrDirectorySymlinks(t *testing.T) {
	directory := t.TempDir()
	reports := filepath.Join(directory, "reports")
	outside := filepath.Join(directory, "outside")
	for _, path := range []string{reports, outside} {
		if err := os.Mkdir(path, 0o777); err != nil {
			t.Fatal(err)
		}
	}
	large, err := os.Create(filepath.Join(outside, "large"))
	if err != nil {
		t.Fatal(err)
	}
	if err := large.Truncate(1_000_000); err != nil {
		t.Fatal(err)
	}
	_ = large.Close()
	for link, target := range map[string]string{
		"directory-link": outside,
		"file-link":      filepath.Join(outside, "large"),
		"broken-link":    filepath.Join(outside, "missing"),
	} {
		if err := os.Symlink(target, filepath.Join(reports, link)); err != nil {
			t.Fatal(err)
		}
	}
	if err := (&childReportBudget{directory: reports, limit: 0}).check(); err != nil {
		t.Fatal(err)
	}
}

// spawnAnsweredByWorker answers one spawn the way a worker does when the
// daemon has not finished it: accepted, no result, and what the worker
// knows about the daemon.
func spawnAnsweredByWorker(t *testing.T, daemon string) (any, bool) {
	t.Helper()
	socket := subagentSocket(t)
	listener := subagentListen(t, socket)
	worker := subagentServeOne(listener, func(conn net.Conn, _ *bufio.Reader, _ []byte) error {
		_, err := conn.Write([]byte(`{"accepted":true,"daemon":` + daemon + "}\n"))
		return err
	})
	value, isError, err := subagentCallWithBudget(socket, nil, core.SubagentMcpRoleParent,
		`{"name":"spawn_worker","arguments":{"task_name":"t","instructions":"i","task_type":"straightforward"}}`,
		ProgressSilent(50*time.Millisecond), 30*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if err := <-worker; err != nil {
		t.Fatal(err)
	}
	return value, isError
}

// #1197: a parent's spawn and list_agents went unanswered while the daemon
// was stalled, and the model read only "did not answer". When the worker
// reports that the daemon never picked the request up, the model is told
// that, that the spawn is saved and may still start, and not to repeat it.
// Hard-won: #1197: a queued spawn ran after the parent had given up.
func TestASpawnTheDaemonNeverPickedUpNamesTheDaemonAndForbidsARepeat(t *testing.T) {
	value, isError := spawnAnsweredByWorker(t, `{"picked_up":false,"last_collected_seconds_ago":140}`)
	if !isError || subagentText(value, "status") != "not_picked_up" {
		t.Fatal(value)
	}
	refusal := subagentText(value, "error")
	for _, want := range []string{
		"has not picked up this request",
		"140 seconds ago",
		"restarting, stalled, or unreachable",
		"do not repeat it",
		"list_agents",
	} {
		if !strings.Contains(refusal, want) {
			t.Fatal(refusal)
		}
	}
	value, _ = spawnAnsweredByWorker(t, `{"picked_up":false}`)
	if refusal := subagentText(value, "error"); !strings.Contains(refusal, "since this session's worker started") {
		t.Fatal(refusal)
	}
}

func TestASpawnTheDaemonPickedUpButDidNotFinishIsStillPending(t *testing.T) {
	value, isError := spawnAnsweredByWorker(t, `{"picked_up":true,"last_collected_seconds_ago":2}`)
	if isError || subagentValue(value, "accepted") != true {
		t.Fatal(value)
	}
	note := subagentText(value, "note")
	if !strings.Contains(note, "picked up this request but has not finished it") || !strings.Contains(note, "list_agents") {
		t.Fatal(note)
	}
}

// subagentSilentWorker takes one request and never answers it.
func subagentSilentWorker(t *testing.T, socket string) {
	t.Helper()
	listener := subagentListen(t, socket)
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		reader := bufio.NewReader(conn)
		_, _ = reader.ReadBytes('\n')
		time.Sleep(2 * time.Second)
		_, _ = reader.Read(make([]byte, 1))
	}()
}

// Hard-won: #1037: the shim blocked forever after a lost worker reply.
func TestAnUnansweredCallBecomesAToolErrorInsteadOfHanging(t *testing.T) {
	socket := subagentSocket(t)
	// Fake worker: consume the request, then hold the connection open
	// without ever answering.
	subagentSilentWorker(t, socket)
	started := time.Now()
	value, isError, err := subagentCallWithBudget(socket, nil, core.SubagentMcpRoleParent,
		`{"name":"list_agents"}`, ProgressSilent(50*time.Millisecond), 200*time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}
	if elapsed := time.Since(started); elapsed >= 1500*time.Millisecond {
		t.Fatalf("the call must give up at its budget, took %v", elapsed)
	}
	if !isError || subagentText(value, "request_id") == "" {
		t.Fatal(value)
	}
	if refusal := subagentText(value, "error"); !strings.Contains(refusal, "did not answer") ||
		!strings.Contains(refusal, "list_agents") || strings.Contains(refusal, "request_key") {
		t.Fatal(refusal)
	}
}

func TestAChildSendsItsReportAsAHandbackRequest(t *testing.T) {
	for _, configured := range []bool{false, true} {
		childSendsItsReport(t, configured)
	}
}

func childSendsItsReport(t *testing.T, configured bool) {
	t.Helper()
	socket := subagentSocket(t)
	directory := filepath.Dir(socket)
	if configured {
		reports := filepath.Join(directory, "reports")
		if err := os.Mkdir(reports, 0o777); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(reports, "evidence.log"), []byte("evidence"), 0o666); err != nil {
			t.Fatal(err)
		}
		subagentWriteLaunch(t, directory, subagentObject("environment", subagentObject(
			core.ChildReportDirEnv, reports,
			core.ChildReportLimitEnv, "8",
		)))
	}
	listener := subagentListen(t, socket)
	received := make(chan any, 1)
	worker := subagentServeOne(listener, func(conn net.Conn, reader *bufio.Reader, line []byte) error {
		request, err := subagentParseLine(line)
		if err != nil {
			return err
		}
		received <- request
		if err := subagentWriteLine(conn, subagentObject("accepted", true, "result", subagentObject(
			"request_id", subagentValue(request, "request_id"),
			"completed_at_ms", 1,
			"is_error", false,
			"message", `{"delivered":true}`,
		))); err != nil {
			return err
		}
		_, _ = reader.Read(make([]byte, 1))
		return nil
	})
	value, isError, err := subagentCallWithBudget(socket, nil, core.SubagentMcpRoleChild,
		`{"name":"handback","arguments":{"message":"the report"}}`, ProgressSilent(50*time.Millisecond), 5*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	if isError || subagentValue(value, "delivered") != true {
		t.Fatal(value)
	}
	request := <-received
	if !subagentEqualJSON(subagentValue(request, "action"), `{"action":"handback","params":{"message":"the report"}}`) {
		t.Fatal(subagentValue(request, "action"))
	}
	if err := <-worker; err != nil {
		t.Fatal(err)
	}
}

// answerAnnouncingWriter observes output after the server owns the
// writer, and announces the response the fake worker is waiting for. The
// writer is the one place that knows a response has been written, so
// releasing the slow call from here orders the two responses by events: no
// sleep, and no assumption about how fast either goroutine runs.
type answerAnnouncingWriter struct {
	mu                sync.Mutex
	written           []byte
	cheapCallAnswered chan struct{}
	announced         bool
}

func (w *answerAnnouncingWriter) Write(data []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()
	w.written = append(w.written, data...)
	// The server writes one whole response per call.
	if strings.Contains(string(data), `"id":2`) && !w.announced {
		w.announced = true
		close(w.cheapCallAnswered)
	}
	return len(data), nil
}

func TestASlowToolCallDoesNotBlockALaterOne(t *testing.T) {
	socket := subagentSocket(t)
	cheapCallAnswered := make(chan struct{})
	// Fake worker: reply to any non-spawn call at once, and hold the spawn
	// reply until the cheap call has been answered. Each accepted
	// connection is served on its own goroutine so the held reply cannot
	// serialize the two calls at the socket layer.
	listener := subagentListen(t, socket)
	var served sync.WaitGroup
	for range 2 {
		served.Add(1)
		go func() {
			defer served.Done()
			conn, err := listener.Accept()
			if err != nil {
				return
			}
			defer conn.Close()
			reader := bufio.NewReader(conn)
			line, err := reader.ReadBytes('\n')
			if err != nil {
				return
			}
			request, err := subagentParseLine(line)
			if err != nil {
				return
			}
			action := subagentText(request, "action", "action")
			if action == "spawn" {
				select {
				case <-cheapCallAnswered:
				case <-time.After(30 * time.Second):
					t.Error("the cheap call must be answered while the spawn is open")
				}
			}
			if err := subagentWriteLine(conn, subagentObject("accepted", true, "result", subagentObject("action", action))); err != nil {
				return
			}
			// Hold the connection open until the client has read the reply,
			// mirroring the real worker's serve_one.
			_, _ = reader.Read(make([]byte, 1))
		}()
	}
	// The slow spawn is sent first, the cheap list_agents second. Only
	// concurrent dispatch can answer the cheap call first, because the
	// spawn's reply is released by that answer being written. A dispatcher
	// that ran the calls in order would answer the spawn first - nothing
	// releases it, so it would fall back to its own budget - and the two
	// responses would come back the other way round.
	input := `{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"spawn_worker","arguments":{"task_name":"slow","instructions":"work","task_type":"straightforward"}}}` + "\n" +
		`{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"list_agents"}}` + "\n"
	writer := &answerAnnouncingWriter{cheapCallAnswered: cheapCallAnswered}
	if err := subagentRun(strings.NewReader(input), writer, socket, nil, core.SubagentMcpRoleParent); err != nil {
		t.Fatal(err)
	}
	// The server has answered both calls, so every connection it made has
	// been accepted. Rust never joins its fake worker; closing the listener
	// ends an Accept no call will satisfy, so a call refused before it
	// connects fails the order check below instead of hanging here.
	_ = listener.Close()
	served.Wait()
	writer.mu.Lock()
	text := string(writer.written)
	writer.mu.Unlock()
	// Progress notifications carry no id; only the two responses do.
	var answered []int64
	for _, line := range subagentLines(text) {
		response, err := jsonvalue.Parse([]byte(line))
		if err != nil {
			continue
		}
		if id, ok := subagentValue(response, "id").(int64); ok {
			answered = append(answered, id)
		}
	}
	if len(answered) != 2 || answered[0] != 2 || answered[1] != 1 {
		t.Fatalf("the cheap list_agents response must be written before the slow spawn: %s", text)
	}
}

// A close is not finished when it is taken: the child is checkpointed, its
// transcript sealed and its process tree torn down, and until that ends
// the child still holds its share of the target's processes. Answering
// "accepted" let a parent spawn the replacement child on top of the one
// leaving, which exhausted a container's process slots (#1087). An
// unconfirmed close must say it is still closing and name the tool that can
// observe the child being gone.
// Hard-won: #1087: premature close completion let replacement process trees overlap.
func TestAnUnconfirmedCloseSaysItIsStillClosingRatherThanAccepted(t *testing.T) {
	socket := subagentSocket(t)
	// Fake worker: take the close and never confirm it.
	subagentSilentWorker(t, socket)
	value, isError, err := subagentCallWithBudget(socket, nil, core.SubagentMcpRoleParent,
		`{"name":"close","arguments":{"child_session_id":"c1"}}`, ProgressSilent(50*time.Millisecond), 200*time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}
	if isError {
		t.Fatalf("an unconfirmed close is an answer with a next step, not a tool error: %v", value)
	}
	if subagentText(value, "status") != "still_closing" || subagentValue(value, "closed") != false || subagentText(value, "child_session_id") != "c1" {
		t.Fatal(value)
	}
	next := subagentText(value, "next_action")
	if !strings.Contains(next, "list_agents") || !strings.Contains(next, "stopped") || !strings.Contains(next, "replacement") {
		t.Fatal(next)
	}
}

// A close only answers once Mjolnir has confirmed it, so the confirmed
// answer is the one a parent may act on. Nothing may manufacture an
// "accepted" answer while the close is still outstanding.
// Hard-won: #1087: close was reported finished while teardown was still running.
func TestACloseAnswersOnlyOnceMjolnirConfirmsTheChildIsGone(t *testing.T) {
	socket := subagentSocket(t)
	childIsGone := make(chan struct{})
	// Fake worker: hold the close until the test says the child is gone.
	listener := subagentListen(t, socket)
	worker := subagentServeOne(listener, func(conn net.Conn, reader *bufio.Reader, _ []byte) error {
		<-childIsGone
		if _, err := conn.Write([]byte(`{"accepted":true,"result":{"is_error":false,"message":"{\"child_session_id\":\"c1\",\"closed\":true}"}}` + "\n")); err != nil {
			return err
		}
		_, _ = reader.Read(make([]byte, 1))
		return nil
	})
	type answer struct {
		value   any
		isError bool
		err     error
	}
	answered := make(chan answer, 1)
	go func() {
		value, isError, err := subagentCallWithBudget(socket, nil, core.SubagentMcpRoleParent,
			`{"name":"close","arguments":{"child_session_id":"c1"}}`, ProgressSilent(50*time.Millisecond), 30*time.Second)
		answered <- answer{value, isError, err}
	}()
	// The close cannot have been answered yet: the only reply it can get is
	// the one the fake worker is still holding.
	select {
	case <-answered:
		t.Fatal("the close answered before the child was gone")
	default:
	}
	close(childIsGone)
	result := <-answered
	if result.err != nil {
		t.Fatal(result.err)
	}
	if result.isError || subagentValue(result.value, "closed") != true {
		t.Fatal(result.value)
	}
	if err := <-worker; err != nil && err != io.EOF {
		t.Fatal(err)
	}
}
