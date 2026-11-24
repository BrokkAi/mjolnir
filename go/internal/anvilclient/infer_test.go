// Port of the tests in brokk-anvil-client 0.28.6 src/infer.rs.

package anvilclient

import (
	"context"
	"encoding/json"
	"reflect"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

type observedRequest struct {
	model               string
	roles               []string
	toolsAreNone        bool
	hasStructuredOutput bool
	reasoningEffort     *string
}

type recordingBackend struct {
	mu       sync.Mutex
	observed []observedRequest
	response string
}

func (b *recordingBackend) ListModels(context.Context) ([]string, error) {
	return []string{"utility-model"}, nil
}

func (b *recordingBackend) StreamChat(_ context.Context, request StreamChatRequest) (LlmResponse, error) {
	roles := make([]string, 0, len(request.Messages))
	for _, message := range request.Messages {
		roles = append(roles, message.Role)
	}
	b.mu.Lock()
	b.observed = append(b.observed, observedRequest{
		model:               request.Model,
		roles:               roles,
		toolsAreNone:        request.Tools == nil,
		hasStructuredOutput: request.StructuredOutput != nil,
		reasoningEffort:     request.ReasoningEffort,
	})
	b.mu.Unlock()
	return LlmResponseText{Text: b.response, Usage: TokenUsage{InputTokens: 3, OutputTokens: 2}}, nil
}

func TestRequestMessagesHaveNoAssistantOrToolRole(t *testing.T) {
	var message InferMessage
	err := json.Unmarshal([]byte(`{"role":"assistant","content":"not allowed"}`), &message)
	if err == nil || !strings.Contains(err.Error(), "unknown variant") {
		t.Fatalf("error = %v", err)
	}
}

func TestDefaultsMatchCliInferencePolicy(t *testing.T) {
	options := DefaultInferOptions()
	if options.ValidationRetries != 1 {
		t.Fatalf("validation retries = %d", options.ValidationRetries)
	}
	if options.IdleTimeouts.FirstProgress != time.Duration(DefaultIdleChunkTimeoutSecs)*time.Second {
		t.Fatalf("first progress = %v", options.IdleTimeouts.FirstProgress)
	}
}

func TestStructuredInferenceNeverSuppliesToolsOrSessionMessages(t *testing.T) {
	backend := &recordingBackend{response: `{"answer":"ok"}`}
	options := DefaultInferOptions()
	low := "low"
	options.ReasoningEffort = &low
	response, err := InferStructured(context.Background(), backend, "utility-model", StructuredInferRequest{
		Messages:   []InferMessage{InferMessageSystem("follow the schema"), InferMessageUser("summarize this")},
		SchemaName: "answer",
		Schema: jsonvalue.Object{
			{Key: "type", Value: "object"},
			{Key: "properties", Value: jsonvalue.Object{{Key: "answer", Value: jsonvalue.Object{{Key: "type", Value: "string"}}}}},
			{Key: "required", Value: []any{"answer"}},
			{Key: "additionalProperties", Value: false},
		},
	}, options)
	if err != nil {
		t.Fatal(err)
	}
	if !jsonvalue.Equal(response.Output, jsonvalue.Object{{Key: "answer", Value: "ok"}}) {
		t.Fatalf("output = %#v", response.Output)
	}
	if response.Usage.InputTokens != 3 || response.Usage.OutputTokens != 2 {
		t.Fatalf("usage = %+v", response.Usage)
	}
	backend.mu.Lock()
	defer backend.mu.Unlock()
	want := []observedRequest{{
		model:               "utility-model",
		roles:               []string{"system", "user", "user"},
		toolsAreNone:        true,
		hasStructuredOutput: true,
		reasoningEffort:     &low,
	}}
	if len(backend.observed) != 1 || !reflect.DeepEqual(backend.observed[0].roles, want[0].roles) ||
		backend.observed[0].model != want[0].model || !backend.observed[0].toolsAreNone ||
		!backend.observed[0].hasStructuredOutput || backend.observed[0].reasoningEffort == nil ||
		*backend.observed[0].reasoningEffort != low {
		t.Fatalf("observed = %+v", backend.observed)
	}
}
