// Port of mj-worker/src/acp/verdict_client.rs: its test-only items and
// tests.

package worker

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/transcript"
)

// newVerdictClient is the test-only VerdictClient::new: a client for source
// without a Fastino key.
func newVerdictClient(source VerdictSource) (*VerdictClient, error) {
	return verdictClientNewWithKey(source, nil)
}

// verdictRequest is one HTTP request a test server read.
type verdictRequest struct {
	path    string
	headers string
	body    []byte
}

// verdictReadRequest reads one HTTP/1.1 request: its request line, headers
// and Content-Length body.
func verdictReadRequest(t *testing.T, reader *bufio.Reader) verdictRequest {
	t.Helper()
	requestLine, err := reader.ReadString('\n')
	if err != nil {
		t.Errorf("read request line: %v", err)
		return verdictRequest{}
	}
	fields := strings.Fields(requestLine)
	request := verdictRequest{headers: requestLine}
	if len(fields) > 1 {
		request.path = fields[1]
	}
	length := 0
	for {
		line, err := reader.ReadString('\n')
		if err != nil {
			t.Errorf("client disconnected before headers: %v", err)
			return request
		}
		if line == "\r\n" {
			break
		}
		if value, ok := strings.CutPrefix(strings.ToLower(line), "content-length:"); ok {
			length, _ = strconv.Atoi(strings.TrimSpace(value))
		}
		request.headers += line
	}
	request.body = make([]byte, length)
	if _, err := io.ReadFull(reader, request.body); err != nil {
		t.Errorf("read request body: %v", err)
	}
	return request
}

// verdictHostedServer is a hosted-source server for one request; it yields
// the request path and body.
func verdictHostedServer(t *testing.T, body string) (*VerdictClient, <-chan verdictRequest) {
	return verdictHostedServerWithStatus(t, "200 OK", body)
}

func verdictHostedServerWithStatus(t *testing.T, status, body string) (*VerdictClient, <-chan verdictRequest) {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { listener.Close() })
	endpoint := fmt.Sprintf("http://%s/", listener.Addr())
	served := make(chan verdictRequest, 1)
	go func() {
		socket, err := listener.Accept()
		if err != nil {
			t.Errorf("accept: %v", err)
			close(served)
			return
		}
		defer socket.Close()
		request := verdictReadRequest(t, bufio.NewReader(socket))
		fmt.Fprintf(socket, "HTTP/1.1 %s\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", status, len(body), body)
		served <- request
	}()
	client, err := newVerdictClient(VerdictSourceHosted{Endpoint: endpoint})
	if err != nil {
		t.Fatal(err)
	}
	return client, served
}

func verdictEvidence() core.TurnEvidence {
	context := &transcript.TurnContext{}
	context.Reset(nil, "Implement the requested feature")
	facts := core.DefaultActivityFacts()
	return context.Evidence(core.HarnessKindClaude, core.TurnPhaseRunning, &facts, 0)
}

func verdictHistory(messages ...core.EvidenceMessage) *core.ContextHistory {
	history := core.DefaultContextHistory()
	history.Messages = messages
	return &history
}

// Hard-won: dc4c34617: a client-side byte cap made long turns permanently unclassifiable
func TestGlideHandbackLargeRequestsShrinkOnlyAfterSizeRefusal(t *testing.T) {
	for _, handback := range []bool{false, true} {
		for _, refusal := range []bool{false, true} {
			listener, err := net.Listen("tcp", "127.0.0.1:0")
			if err != nil {
				t.Fatal(err)
			}
			endpoint := fmt.Sprintf("http://%s/", listener.Addr())
			served := make(chan []int, 1)
			go func() {
				var sizes []int
				for {
					socket, err := listener.Accept()
					if err != nil {
						t.Errorf("accept: %v", err)
						served <- sizes
						return
					}
					request := verdictReadRequest(t, bufio.NewReader(socket))
					sizes = append(sizes, len(request.body))
					reject := refusal && len(sizes) <= 3
					var status, answer string
					switch {
					case reject && len(sizes) == 1:
						status, answer = "413 Payload Too Large", `{"error":"body_too_large"}`
					case reject:
						status, answer = "400 Bad Request",
							`{"error":{"code":"input_too_long","message":"prompt exceeds the maximum context length"}}`
					case handback:
						status, answer = "200 OK",
							`{"probabilities":{"unfinished":0.0,"needs_parent_input":0.0,"blocked":0.0,"permitted_stop":0.0,"finished":1.0,"unclear":0.0}}`
					default:
						samples := testSamples().([]any)
						encoded, err := jsonvalue.Marshal(samples[0])
						if err != nil {
							t.Errorf("encode sample: %v", err)
						}
						status, answer = "200 OK", string(encoded)
					}
					// Hard-won: 8ad2f6a1d: GLiDE inherited Jev's ten-second timeout.
					if !handback && !refusal {
						time.Sleep(15 * time.Second)
					}
					fmt.Fprintf(socket, "HTTP/1.1 %s\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", status, len(answer), answer)
					socket.Close()
					if !reject {
						served <- sizes
						return
					}
				}
			}()
			client, err := newVerdictClient(VerdictSourceHosted{Endpoint: endpoint})
			if err != nil {
				t.Fatal(err)
			}
			completed := verdictEvidence()
			completed.Phase = core.TurnPhaseReplied
			completed.Authorization = verdictHistory(
				core.EvidenceMessage{ID: "u", Role: "user", Text: "Complete task"},
				core.EvidenceMessage{ID: "a", Role: "assistant", Text: strings.Repeat("synthetic reply ", 14000)},
			)
			if handback {
				attempt, _, err := client.askHandbackLogged(context.Background(), "isolated", 1, &completed)
				attempt.Close()
				if err != nil {
					t.Fatalf("handback: %v", err)
				}
			} else {
				facts := core.DefaultActivityFacts()
				if _, err := client.askGlideTurn(context.Background(), &completed, *completed.Authorization, &facts, 0); err != nil {
					t.Fatalf("glide turn: %v", err)
				}
			}
			sizes := <-served
			listener.Close()
			if sizes[0] <= 200*1024 {
				t.Fatalf("first request %d bytes", sizes[0])
			}
			want := 1
			if refusal {
				want = 4
			}
			if len(sizes) != want {
				t.Fatalf("handback %v refusal %v: %d requests", handback, refusal, len(sizes))
			}
			for index := 1; index < len(sizes); index++ {
				if sizes[index] >= sizes[index-1] {
					t.Fatalf("requests did not shrink: %v", sizes)
				}
			}
		}
	}
}

func TestHandbackSizeRefusalNeverShrinksAwayInstructions(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	endpoint := fmt.Sprintf("http://%s/", listener.Addr())
	served := make(chan int, 1)
	go func() {
		requests := 0
		for {
			if tcp, ok := listener.(*net.TCPListener); ok {
				tcp.SetDeadline(time.Now().Add(500 * time.Millisecond))
			}
			socket, err := listener.Accept()
			if err != nil {
				served <- requests
				return
			}
			verdictReadRequest(t, bufio.NewReader(socket))
			requests++
			answer := `{"error":"body_too_large"}`
			fmt.Fprintf(socket, "HTTP/1.1 413 Payload Too Large\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", len(answer), answer)
			socket.Close()
		}
	}()
	client, err := newVerdictClient(VerdictSourceHosted{Endpoint: endpoint})
	if err != nil {
		t.Fatal(err)
	}
	completed := verdictEvidence()
	completed.Phase = core.TurnPhaseReplied
	// The oldest message is the parent's instruction: the next smaller
	// request would have to drop it.
	completed.Authorization = verdictHistory(
		core.EvidenceMessage{ID: "user:first", Role: "user", Text: "First instruction"},
		core.EvidenceMessage{ID: "agent:progress", Role: "assistant", Text: "Progress"},
		core.EvidenceMessage{ID: "user:second", Role: "user", Text: "Second instruction"},
		core.EvidenceMessage{ID: "agent:report", Role: "assistant", Text: "Report"},
	)
	attempt, _, err := client.askHandbackLogged(context.Background(), "isolated", 1, &completed)
	attempt.Close()
	if err == nil || !strings.Contains(err.Error(), "drop the parent's instructions") {
		t.Fatalf("error %v", err)
	}
	if requests := <-served; requests != 1 {
		t.Fatalf("%d requests", requests)
	}
}

func TestLocatorSendsRedactedStateAloneToTheHostedGlideRoute(t *testing.T) {
	const token = "ghp_aBcDeFgHiJkLmNoPqRsTuVwXyZ0123456789"
	client, served := verdictHostedServer(t, `{"probabilities":{"r0":0.2,"r1":0.7,"none":0.1}}`)
	history := verdictHistory(
		core.EvidenceMessage{ID: "user:task", Role: "user", Text: "Task"},
		core.EvidenceMessage{ID: "agent:one", Role: "assistant", Text: "Pushed with GH_TOKEN=" + token},
		core.EvidenceMessage{ID: "agent:two", Role: "assistant", Text: "Choose?"},
	)
	request := core.Build(history)
	index, ok, err := client.locateQuestion(context.Background(), "session", &request)
	if err != nil {
		t.Fatal(err)
	}
	if !ok || index != 1 {
		t.Fatalf("located %d %v", index, ok)
	}
	received := <-served
	if received.path != "/v1/question-locator-glide" {
		t.Fatalf("path %s", received.path)
	}
	if strings.Contains(string(received.body), token) {
		t.Fatalf("token sent: %s", received.body)
	}
	sent := acpFeaturesJSON(t, string(received.body))
	if text, _ := acpFeaturesPointer(sent, "replies"); text == nil {
		t.Fatalf("no replies: %s", received.body)
	}
	replies, _ := acpFeaturesGet(sent, "replies")
	first, _ := acpFeaturesGet(replies.([]any)[0], "text")
	if first != "Pushed with GH_TOKEN=[redacted]" {
		t.Fatalf("first reply %v", first)
	}
	want, err := acpFeaturesValue(request.State.Replies[1])
	if err != nil {
		t.Fatal(err)
	}
	if !acpFeaturesJSONEqual(replies.([]any)[1], want) {
		t.Fatalf("second reply %v, want %v", replies.([]any)[1], want)
	}
	// The hosted proxy owns the question; the request is the state alone.
	if keys := sent.(jsonvalue.Object).Keys(); len(keys) != 2 {
		t.Fatalf("request %s", received.body)
	}
}

func TestLocatorSizeRefusalIsAFailureWithoutARetry(t *testing.T) {
	client, served := verdictHostedServerWithStatus(t, "413 Payload Too Large", `{"error":"body_too_large"}`)
	var messages []core.EvidenceMessage
	for _, id := range []string{"a", "b"} {
		messages = append(messages, core.EvidenceMessage{ID: "agent:" + id, Role: "assistant", Text: "Choose?"})
	}
	history := core.DefaultContextHistory()
	history.Messages = messages
	request := core.Build(&history)
	_, _, err := client.locateQuestion(context.Background(), "session", &request)
	// The server accepts one connection; a retry would fail to connect
	// instead.
	if err == nil || !strings.Contains(err.Error(), "413") {
		t.Fatalf("error %v", err)
	}
	<-served
}
