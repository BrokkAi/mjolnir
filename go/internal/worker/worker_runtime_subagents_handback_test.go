// Port of mj-worker/src/worker_runtime/subagents/handback_tests.rs.

//go:build unix

package worker

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func handbackReport(id string) core.SubagentToolRequest {
	return core.SubagentToolRequest{
		RequestID:   id,
		CreatedAtMs: 1,
		Action:      core.SubagentToolActionHandback{Message: "Implementation is ready; validation remains."},
	}
}

// handbackCall sends request over a fresh socket pair served by endpoint and
// returns the decoded reply.
func handbackCall(t *testing.T, endpoint *SubagentEndpoint, request core.SubagentToolRequest) <-chan map[string]any {
	replies := make(chan map[string]any, 1)
	client, server := subagentsSocketPair(t)
	handler := subagentsServeOnce(server, endpoint)
	go func() {
		defer client.Close()
		body, _ := json.Marshal(request)
		if _, err := client.Write(append(body, '\n')); err != nil {
			t.Error(err)
		}
		line, err := bufio.NewReader(client).ReadString('\n')
		if err != nil {
			t.Error(err)
		}
		if err := <-handler; err != nil {
			t.Error(err)
		}
		var reply map[string]any
		if err := json.Unmarshal([]byte(line), &reply); err != nil {
			t.Error(err)
		}
		replies <- reply
	}()
	return replies
}

// handbackReadRequest reads one HTTP request's body.
func handbackReadRequest(reader *bufio.Reader) ([]byte, error) {
	length := 0
	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			return nil, err
		}
		if line == "\r\n" {
			break
		}
		if value, ok := strings.CutPrefix(strings.ToLower(line), "content-length:"); ok {
			length, _ = strconv.Atoi(strings.TrimSpace(value))
		}
	}
	body := make([]byte, length)
	_, err := io.ReadFull(reader, body)
	return body, err
}

// handbackRelay is a relay whose child assignment was delivered, with the
// handback classifier at listener.
func handbackRelay(t *testing.T, root string, listener net.Listener, log *core.DecisionLog) *RelayCell {
	t.Helper()
	owner, err := DurableRelayOpen(root, session, "test")
	subagentsMust(t, err)
	setVerdictHarness(t, owner, core.HarnessKindCodex)
	_, err = owner.RecordObservation(core.RelayObservationSessionConfigured{ConfigOptions: nil})
	subagentsMust(t, err)
	submitRelay(owner, "subagent-spawn-first", prompt("Implement and validate the requested change."))
	_, err = owner.ClaimPendingCommands(true)
	subagentsMust(t, err)
	client, err := newVerdictClient(VerdictSourceHosted{Endpoint: fmt.Sprintf("http://%s/", listener.Addr())})
	subagentsMust(t, err)
	if log != nil {
		logged := client.withLog(log)
		client = &logged
	}
	owner.handbackVerdictClient = client
	return NewRelayCell(owner)
}

// handbackWaitForReference waits until one more handback call holds the gate
// than before.
func handbackWaitForReference(endpoint *SubagentEndpoint, before int64) {
	for endpoint.handbackGate.references.Load() <= before {
		time.Sleep(time.Millisecond)
	}
}

func handbackQueued(endpoint *SubagentEndpoint, id string) bool {
	endpoint.state.mu.Lock()
	defer endpoint.state.mu.Unlock()
	_, ok := endpoint.state.state.requests[id]
	return ok
}

func handbackQueueEmpty(endpoint *SubagentEndpoint) bool {
	endpoint.state.mu.Lock()
	defer endpoint.state.mu.Unlock()
	return len(endpoint.state.state.requests) == 0
}

func TestPendingHandbackCheckWithholdsPublicationAndRepeatIsForwardedWithoutHTTP(t *testing.T) {
	root := t.TempDir()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	subagentsMust(t, err)
	relay := handbackRelay(t, root, listener, nil)
	endpoint, err := SubagentEndpointOpen(root)
	subagentsMust(t, err)
	endpoint.relay = relay
	worker := core.SubagentKindWorker
	endpoint.subagentKind = &worker
	seen := make(chan any, 1)
	release := make(chan struct{})
	http := make(chan struct{})
	go func() {
		defer close(http)
		defer listener.Close()
		socket, err := listener.Accept()
		if err != nil {
			t.Error(err)
			return
		}
		defer socket.Close()
		body, err := handbackReadRequest(bufio.NewReader(socket))
		if err != nil {
			t.Error(err)
			return
		}
		var evidence any
		_ = json.Unmarshal(body, &evidence)
		seen <- evidence
		<-release
		response := `{"probabilities":{"unfinished":0.5,"needs_parent_input":0.1,"blocked":0.1,"permitted_stop":0.1,"finished":0.1,"unclear":0.1}}`
		fmt.Fprintf(socket, "HTTP/1.1 200 OK\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", len(response), response)
	}()
	// Excluded identities do not consume the ordinary child's single attempt.
	for _, kind := range []*core.SubagentKind{nil, subagentsKind(core.SubagentKindReviewer), subagentsKind(core.SubagentKindOracle)} {
		excluded := endpoint.clone()
		excluded.subagentKind = kind
		report := handbackReport("excluded")
		result, err := excluded.checkHandback(context.Background(), &report)
		subagentsMust(t, err)
		if result != nil {
			t.Fatalf("%v must not be checked", kind)
		}
	}
	first := handbackCall(t, endpoint.clone(), handbackReport("first"))
	evidence := <-seen
	encodedEvidence, _ := json.Marshal(evidence)
	fmt.Printf("HANDBACK_WORKER_BODY=%s\n", encodedEvidence)
	retryReferences := endpoint.handbackGate.references.Load()
	retry := handbackCall(t, endpoint.clone(), handbackReport("first"))
	handbackWaitForReference(endpoint, retryReferences)
	if !strings.Contains(string(encodedEvidence), "subagent-spawn-first") {
		t.Fatalf("evidence = %s", encodedEvidence)
	}
	if !handbackQueueEmpty(endpoint) {
		t.Fatal("daemon must not see the handback while HTTP waits")
	}
	// Both state owners remain available while classification is awaiting
	// HTTP.
	owner := relay.Lock()
	_ = owner.OperationalState()
	owner.Unlock()
	// Hard-won: 2afc06c9a: a concurrent handback with another request ID was delivered while the first check was pending
	differentReferences := endpoint.handbackGate.references.Load()
	different := handbackCall(t, endpoint.clone(), handbackReport("different"))
	handbackWaitForReference(endpoint, differentReferences)
	if !handbackQueueEmpty(endpoint) {
		t.Fatal("neither handback may publish while the first HTTP check waits")
	}
	close(release)
	bounced := <-first
	retried := <-retry
	bouncedJSON, _ := json.Marshal(bounced)
	retriedJSON, _ := json.Marshal(retried)
	if string(retriedJSON) != string(bouncedJSON) {
		t.Fatalf("same-ID retry must await and reuse the first decision:\n%s\n%s", bouncedJSON, retriedJSON)
	}
	for !handbackQueued(endpoint, "different") {
		time.Sleep(time.Millisecond)
	}
	<-http // Listener is gone: no second HTTP check.
	_, err = endpoint.Complete(core.SubagentToolResult{RequestID: "different", CompletedAtMs: 1, IsError: false, Message: "Report delivered"})
	subagentsMust(t, err)
	if reply := <-different; subagentsGet(reply, "result", "message") != "Report delivered" {
		t.Fatalf("reply = %v", reply)
	}
	if subagentsGet(bounced, "result", "is_error") != false {
		t.Fatalf("bounced = %v", bounced)
	}
	if message, _ := subagentsGet(bounced, "result", "message").(string); !strings.Contains(message, "Not delivered") {
		t.Fatalf("bounced = %v", bounced)
	}
	if handbackQueued(endpoint, "first") {
		t.Fatal("a bounced handback is never queued")
	}
	second := handbackCall(t, endpoint.clone(), handbackReport("second"))
	for {
		changed := endpoint.generations().Changed()
		if handbackQueued(endpoint, "second") {
			break
		}
		<-changed
	}
	_, err = endpoint.Complete(core.SubagentToolResult{RequestID: "second", CompletedAtMs: 1, IsError: false, Message: "Report delivered"})
	subagentsMust(t, err)
	if reply := <-second; subagentsGet(reply, "result", "message") != "Report delivered" {
		t.Fatalf("reply = %v", reply)
	}
}

func subagentsKind(kind core.SubagentKind) *core.SubagentKind { return &kind }

// Hard-won: 8ad2f6a1d: a slow optional handback check delayed delivery to the parent.
func TestSlowHandbackGlideDeliversUncheckedAndConsumesCheck(t *testing.T) {
	root := t.TempDir()
	logs := t.TempDir()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	subagentsMust(t, err)
	log, err := core.DecisionLogOpen(logs)
	subagentsMust(t, err)
	defer log.Close()
	relay := handbackRelay(t, root, listener, &log)
	endpoint, err := SubagentEndpointOpen(root)
	subagentsMust(t, err)
	endpoint.relay = relay
	worker := core.SubagentKindWorker
	endpoint.subagentKind = &worker
	http := make(chan struct{})
	go func() {
		defer close(http)
		defer listener.Close()
		socket, err := listener.Accept()
		if err != nil {
			t.Error(err)
			return
		}
		defer socket.Close()
		if _, err := handbackReadRequest(bufio.NewReader(socket)); err != nil {
			t.Error(err)
			return
		}
		time.Sleep(7 * time.Second)
		// The caller has already delivered unchecked; no response is needed.
	}()
	started := time.Now()
	first := handbackCall(t, endpoint.clone(), handbackReport("slow"))
	for {
		changed := endpoint.generations().Changed()
		if handbackQueued(endpoint, "slow") {
			break
		}
		select {
		case <-changed:
		case <-time.After(7 * time.Second):
			t.Fatal("the handback must be delivered before the slow check answers")
		}
	}
	if elapsed := time.Since(started); elapsed < 5*time.Second || elapsed >= 7*time.Second {
		t.Fatalf("delivered after %v", elapsed)
	}
	_, err = endpoint.Complete(core.SubagentToolResult{RequestID: "slow", CompletedAtMs: 1, IsError: false, Message: "Report delivered"})
	subagentsMust(t, err)
	if reply := <-first; subagentsGet(reply, "result", "message") != "Report delivered" {
		t.Fatalf("reply = %v", reply)
	}
	repeatStarted := time.Now()
	repeat := handbackReport("repeat")
	result, err := endpoint.checkHandback(context.Background(), &repeat)
	subagentsMust(t, err)
	if result != nil {
		t.Fatal("the spent check is not asked again")
	}
	if time.Since(repeatStarted) >= time.Second {
		t.Fatal("the repeat must not wait for a classifier")
	}
	page, err := core.JevRead(logs, session, nil)
	subagentsMust(t, err)
	page, err = core.JevRead(logs, session, &page.Decisions[0].ID)
	subagentsMust(t, err)
	var technical map[string]any
	subagentsMust(t, json.Unmarshal(page.Decisions[0].Technical, &technical))
	if technical["outcome"] != "delivered" || technical["reason"] != "timeout" {
		t.Fatalf("technical = %v", technical)
	}
	<-http
}
