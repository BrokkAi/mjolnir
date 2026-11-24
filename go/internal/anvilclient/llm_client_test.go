// Port of tests in brokk-anvil-client 0.28.6 src/llm_client.rs: the
// chat-completions wire and stream driver Mjolnir's utility inference uses.

package anvilclient

import (
	"context"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// chunkStream is a response body that yields each chunk after its delay,
// then ends.
type chunkStream struct {
	chunks  []delayedChunk
	pending []byte
	closed  chan struct{}
	once    sync.Once
}

type delayedChunk struct {
	delay time.Duration
	data  string
}

func newChunkStream(chunks ...delayedChunk) *chunkStream {
	return &chunkStream{chunks: chunks, closed: make(chan struct{})}
}

func immediateChunks(data ...string) *chunkStream {
	chunks := make([]delayedChunk, 0, len(data))
	for _, chunk := range data {
		chunks = append(chunks, delayedChunk{data: chunk})
	}
	return newChunkStream(chunks...)
}

func (s *chunkStream) Read(buffer []byte) (int, error) {
	if len(s.pending) == 0 {
		if len(s.chunks) == 0 {
			return 0, io.EOF
		}
		next := s.chunks[0]
		s.chunks = s.chunks[1:]
		select {
		case <-time.After(next.delay):
		case <-s.closed:
			return 0, io.ErrClosedPipe
		}
		s.pending = []byte(next.data)
	}
	count := copy(buffer, s.pending)
	s.pending = s.pending[count:]
	return count, nil
}

func (s *chunkStream) Close() error {
	s.once.Do(func() { close(s.closed) })
	return nil
}

func collectTokens() (TokenSink, func() []string) {
	var mu sync.Mutex
	var collected []string
	return func(text string) {
			mu.Lock()
			collected = append(collected, text)
			mu.Unlock()
		}, func() []string {
			mu.Lock()
			defer mu.Unlock()
			return slices.Clone(collected)
		}
}

func TestDeepseekReasoningEffortClampsToHighOrMax(t *testing.T) {
	// Only `high`/`max` are valid for DeepSeek. xhigh/max -> max; everything
	// else floors to high.
	for input, want := range map[string]string{
		"low": "high", "medium": "high", "high": "high", "xhigh": "max",
		"max": "max", "minimal": "high", "  MAX ": "max",
	} {
		if got := deepseekReasoningEffort(input); got != want {
			t.Fatalf("%q -> %q, want %q", input, got, want)
		}
	}
}

// DeepSeek `/models` is id-only, so the client declares the levels itself:
// every model gets `[high, max]` with `high` as the default.
func TestDeepseekWireDeclaresHighMaxReasoningLevels(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.Method != http.MethodGet || request.URL.Path != "/v1/models" {
			http.NotFound(writer, request)
			return
		}
		fmt.Fprint(writer, `{"object":"list","data":[{"id":"deepseek-reasoner","object":"model","owned_by":"deepseek"},{"id":"deepseek-chat","object":"model","owned_by":"deepseek"}]}`)
	}))
	defer server.Close()
	client := NewOpenAiClientWithDeepseekReasoningSupport(server.URL, nil, http.Header{})
	metadata, err := client.ListModelMetadata(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if len(metadata) != 2 {
		t.Fatalf("metadata = %+v", metadata)
	}
	for _, model := range metadata {
		if model.DefaultReasoningLevel == nil || *model.DefaultReasoningLevel != "high" {
			t.Fatalf("%+v", model)
		}
		var levels []string
		for _, preset := range model.SupportedReasoningLevels {
			levels = append(levels, preset.Effort)
		}
		if !slices.Equal(levels, []string{"high", "max"}) {
			t.Fatalf("%+v", model)
		}
	}
}

type testWire int

const (
	testWireOff testWire = iota
	testWireUnified
	testWireKimi
	testWireDeepSeek
)

// captureRequestBody captures the JSON body a client sends for one chat
// request via a server answering with an empty SSE stream.
func captureRequestBody(t *testing.T, wire testWire, effort *string, structured *StructuredOutputRequest) any {
	t.Helper()
	var mu sync.Mutex
	var bodies [][]byte
	server := httptest.NewServer(http.HandlerFunc(func(writer http.ResponseWriter, request *http.Request) {
		if request.Method != http.MethodPost || request.URL.Path != "/v1/chat/completions" {
			http.NotFound(writer, request)
			return
		}
		body, _ := io.ReadAll(request.Body)
		mu.Lock()
		bodies = append(bodies, body)
		mu.Unlock()
		fmt.Fprint(writer, "data: [DONE]\n")
	}))
	defer server.Close()
	var client *OpenAiClient
	switch wire {
	case testWireDeepSeek:
		client = NewOpenAiClientWithDeepseekReasoningSupport(server.URL, nil, http.Header{})
	case testWireUnified:
		client = NewOpenAiClientWithReasoningSupport(server.URL, nil, http.Header{})
	case testWireKimi:
		client = NewOpenAiClientWithKimiSupport(server.URL, staticBearerToken{}, http.Header{})
	default:
		client = NewOpenAiClient(server.URL, nil)
	}
	onToken, _ := collectTokens()
	if _, err := client.StreamChat(context.Background(), StreamChatRequest{
		Model:            "deepseek-reasoner",
		Messages:         []ChatMessage{ChatMessageUser("hi")},
		ReasoningEffort:  effort,
		StructuredOutput: structured,
		OnToken:          onToken,
		OnThought:        func(string) {},
		IdleTimeouts:     IdleTimeoutsUniform(30 * time.Second),
	}); err != nil {
		t.Fatalf("mock stream completes: %v", err)
	}
	mu.Lock()
	defer mu.Unlock()
	if len(bodies) != 1 {
		t.Fatalf("requests = %d", len(bodies))
	}
	value, err := jsonvalue.Parse(bodies[0])
	if err != nil {
		t.Fatalf("body is JSON: %v", err)
	}
	return value
}

func testStructuredOutputRequest() *StructuredOutputRequest {
	return &StructuredOutputRequest{
		SchemaName: "audit_result",
		Schema: jsonvalue.Object{
			{Key: "type", Value: "object"},
			{Key: "properties", Value: jsonvalue.Object{{Key: "answer", Value: jsonvalue.Object{{Key: "type", Value: "string"}}}}},
			{Key: "required", Value: []any{"answer"}},
		},
	}
}

func effort(value string) *string { return &value }

func bodyPath(value any, keys ...string) (any, bool) {
	for _, key := range keys {
		member, ok := jsonMember(value, key)
		if !ok {
			return nil, false
		}
		value = member
	}
	return value, true
}

// DeepSeek wire sends `thinking: {type: enabled}` + a top-level
// `reasoning_effort` (clamped to DeepSeek's scale), and not the unified
// `reasoning` object.
func TestDeepseekWireSendsThinkingAndTopLevelEffort(t *testing.T) {
	body := captureRequestBody(t, testWireDeepSeek, effort("medium"), nil)
	if kind, _ := bodyPath(body, "thinking", "type"); kind != "enabled" {
		t.Fatalf("body = %v", body)
	}
	if got, _ := bodyPath(body, "reasoning_effort"); got != "high" {
		t.Fatalf("body = %v", body)
	}
	if _, ok := bodyPath(body, "reasoning"); ok {
		t.Fatalf("body = %v", body)
	}
}

// With no effort, the DeepSeek wire sends none of the reasoning fields and
// lets DeepSeek apply its default.
func TestDeepseekWireOmitsReasoningWhenNoEffort(t *testing.T) {
	body := captureRequestBody(t, testWireDeepSeek, nil, nil)
	for _, key := range []string{"thinking", "reasoning_effort", "reasoning"} {
		if _, ok := bodyPath(body, key); ok {
			t.Fatalf("%s present: %v", key, body)
		}
	}
}

func TestKimiWireSendsNestedThinkingEffort(t *testing.T) {
	body := captureRequestBody(t, testWireKimi, effort("max"), nil)
	if kind, _ := bodyPath(body, "thinking", "type"); kind != "enabled" {
		t.Fatalf("body = %v", body)
	}
	if got, _ := bodyPath(body, "thinking", "effort"); got != "max" {
		t.Fatalf("body = %v", body)
	}
	for _, key := range []string{"reasoning", "reasoning_effort"} {
		if _, ok := bodyPath(body, key); ok {
			t.Fatalf("%s present: %v", key, body)
		}
	}
}

func TestGenericOpenaiCompatibleWireSendsStrictJSONSchemaResponseFormat(t *testing.T) {
	body := captureRequestBody(t, testWireOff, nil, testStructuredOutputRequest())
	for _, check := range []struct {
		path []string
		want any
	}{
		{[]string{"response_format", "type"}, "json_schema"},
		{[]string{"response_format", "json_schema", "name"}, "audit_result"},
		{[]string{"response_format", "json_schema", "schema", "type"}, "object"},
		{[]string{"response_format", "json_schema", "strict"}, true},
	} {
		if got, _ := bodyPath(body, check.path...); got != check.want {
			t.Fatalf("%v = %v in %v", check.path, got, body)
		}
	}
}

func TestDeepseekWireSendsJSONObjectResponseFormatForStructuredOutput(t *testing.T) {
	body := captureRequestBody(t, testWireDeepSeek, nil, testStructuredOutputRequest())
	if got, _ := bodyPath(body, "response_format", "type"); got != "json_object" {
		t.Fatalf("body = %v", body)
	}
	if _, ok := bodyPath(body, "response_format", "json_schema"); ok {
		t.Fatalf("body = %v", body)
	}
}

// A request opting into `prefer_json_object` downgrades to basic JSON mode
// even on a strict-json_schema client.
func TestPreferJSONObjectDowngradesStrictSchemaClientToJSONObject(t *testing.T) {
	request := testStructuredOutputRequest()
	request.PreferJSONObject = true
	body := captureRequestBody(t, testWireUnified, nil, request)
	if got, _ := bodyPath(body, "response_format", "type"); got != "json_object" {
		t.Fatalf("body = %v", body)
	}
	if _, ok := bodyPath(body, "response_format", "json_schema"); ok {
		t.Fatalf("body = %v", body)
	}
}

// The unified wire still sends `reasoning: {effort}` verbatim and none of
// DeepSeek's fields.
func TestUnifiedWireSendsReasoningObject(t *testing.T) {
	body := captureRequestBody(t, testWireUnified, effort("high"), nil)
	if got, _ := bodyPath(body, "reasoning", "effort"); got != "high" {
		t.Fatalf("body = %v", body)
	}
	for _, key := range []string{"thinking", "reasoning_effort"} {
		if _, ok := bodyPath(body, key); ok {
			t.Fatalf("%s present: %v", key, body)
		}
	}
}

// A stream that emits only SSE keepalive comments must trip the deadline:
// otherwise a server can hold the request open forever by drip-feeding
// pings.
func TestDriveSSEStreamBailsOnKeepaliveOnlyChunks(t *testing.T) {
	stream := newChunkStream(delayedChunk{data: ":keepalive\n"}, delayedChunk{delay: time.Hour, data: ""})
	onToken, _ := collectTokens()
	_, err := driveSSEStream(context.Background(), stream, onToken, nil, IdleTimeoutsUniform(time.Second))
	if err == nil || !strings.Contains(err.Error(), "no first token") || !IsRetryableLlmError(err) {
		t.Fatalf("error = %v", err)
	}
}

func TestDriveSSEStreamTimesOutWaitingForFirstProgress(t *testing.T) {
	stream := newChunkStream(delayedChunk{delay: 3 * time.Second, data: "data: {\"choices\":[{\"delta\":{\"content\":\"late\"}}]}\n"})
	onToken, _ := collectTokens()
	_, err := driveSSEStream(context.Background(), stream, onToken, nil, IdleTimeouts{FirstProgress: 2 * time.Second, InterChunk: time.Second})
	if err == nil {
		t.Fatal("missing first progress should abort")
	}
	if !strings.Contains(err.Error(), "no first token for 2s") {
		t.Fatalf("got: %v", err)
	}
	if !IsRetryableLlmError(err) {
		t.Fatalf("not retryable: %v", err)
	}
}

func TestDriveSSEStreamReturnsTextOnDone(t *testing.T) {
	stream := immediateChunks("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n", "data: [DONE]\n")
	onToken, collected := collectTokens()
	response, err := driveSSEStream(context.Background(), stream, onToken, nil, IdleTimeoutsUniform(90*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	if text, ok := response.(LlmResponseText); !ok || text.Text != "hi" {
		t.Fatalf("response = %#v", response)
	}
	if !slices.Equal(collected(), []string{"hi"}) {
		t.Fatalf("tokens = %q", collected())
	}
}

func TestDriveSSEStreamErrorsOnEmptyLengthFinishReason(t *testing.T) {
	stream := immediateChunks(
		"data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":\"length\"}]}\n",
		"data: [DONE]\n")
	onToken, _ := collectTokens()
	onThought, thoughts := collectTokens()
	_, err := driveSSEStream(context.Background(), stream, onToken, onThought, IdleTimeoutsUniform(90*time.Second))
	if err == nil {
		t.Fatal("empty length-limited completion should be classified")
	}
	if !IsOutputBudgetExhaustedError(err) || IsRetryableLlmError(err) {
		t.Fatalf("error = %v", err)
	}
	if !slices.Equal(thoughts(), []string{"thinking"}) {
		t.Fatalf("thoughts = %q", thoughts())
	}
}

func TestDriveSSEStreamReturnsTruncatedTextOnLengthFinishReason(t *testing.T) {
	stream := immediateChunks(
		"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"},\"finish_reason\":\"length\"}]}\n",
		"data: [DONE]\n")
	onToken, collected := collectTokens()
	response, err := driveSSEStream(context.Background(), stream, onToken, nil, IdleTimeoutsUniform(90*time.Second))
	if err != nil {
		t.Fatalf("length after text should return truncated content: %v", err)
	}
	if text, ok := response.(LlmResponseText); !ok || text.Text != "partial" {
		t.Fatalf("response = %#v", response)
	}
	if !slices.Equal(collected(), []string{"partial"}) {
		t.Fatalf("tokens = %q", collected())
	}
}

func TestDriveSSEStreamIgnoresEmptyFinishReasonAndAllowsEmptyStop(t *testing.T) {
	for _, finishReason := range []string{"", "stop"} {
		raw := fmt.Sprintf("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"%s\"}]}\n", finishReason)
		onToken, _ := collectTokens()
		response, err := driveSSEStream(context.Background(), immediateChunks(raw, "data: [DONE]\n"), onToken, nil, IdleTimeoutsUniform(90*time.Second))
		if err != nil {
			t.Fatalf("non-length empty completions keep existing behavior: %v", err)
		}
		if text, ok := response.(LlmResponseText); !ok || text.Text != "" {
			t.Fatalf("response = %#v", response)
		}
	}
}

func TestDriveSSEStreamAcceptsFinalDoneWithoutNewline(t *testing.T) {
	stream := immediateChunks("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n", "data: [DONE]")
	onToken, collected := collectTokens()
	response, err := driveSSEStream(context.Background(), stream, onToken, nil, IdleTimeoutsUniform(90*time.Second))
	if err != nil {
		t.Fatalf("final buffered [DONE] should complete: %v", err)
	}
	if text, ok := response.(LlmResponseText); !ok || text.Text != "hi" {
		t.Fatalf("response = %#v", response)
	}
	if !slices.Equal(collected(), []string{"hi"}) {
		t.Fatalf("tokens = %q", collected())
	}
}

func TestDriveSSEStreamCapturesTrailingUsageChunk(t *testing.T) {
	stream := immediateChunks(
		"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n",
		"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":30,\"prompt_tokens_details\":{\"cached_tokens\":40},\"completion_tokens_details\":{\"reasoning_tokens\":10}}}\n",
		"data: [DONE]\n")
	onToken, _ := collectTokens()
	response, err := driveSSEStream(context.Background(), stream, onToken, nil, IdleTimeoutsUniform(90*time.Second))
	if err != nil {
		t.Fatal(err)
	}
	text, ok := response.(LlmResponseText)
	if !ok || text.Text != "hi" {
		t.Fatalf("response = %#v", response)
	}
	usage := text.Usage
	if usage.InputTokens != 80 || usage.OutputTokens != 20 || usage.ThoughtTokens != 10 ||
		usage.CachedReadTokens != 40 || usage.CachedWriteTokens != 0 || usage.TotalTokens() != 150 {
		t.Fatalf("usage = %+v", usage)
	}
}
