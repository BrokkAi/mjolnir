// Port of tests in brokk-anvil-client 0.28.6 src/codex_client.rs: the
// Responses request shape and stream parser Mjolnir's Codex utility model
// uses.

package anvilclient

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

func testContinuation() responsesContinuation {
	return responsesContinuation{identity: codexPromptCacheIdentity{sessionID: "session-fixed", threadID: "thread-fixed"}}
}

func driveCodex(t *testing.T, raw string) (LlmResponse, string, error) {
	t.Helper()
	var collected strings.Builder
	response, err := codexDriveResponsesSSEStream(context.Background(), immediateChunks(raw),
		func(text string) { collected.WriteString(text) }, nil, IdleTimeoutsUniform(5*time.Second))
	return response, collected.String(), err
}

func TestBuildRequestCollapsesSystemMessagesIntoInstructions(t *testing.T) {
	messages := []ChatMessage{
		ChatMessageSystem("be helpful"),
		ChatMessageSystem("also be brief"),
		ChatMessageUser("hi"),
	}
	continuation := testContinuation()
	encoded, err := codexBuildResponsesRequest("gpt-5-codex", messages, nil, nil, nil, nil, &continuation)
	if err != nil {
		t.Fatal(err)
	}
	request, err := jsonvalue.Parse(encoded)
	if err != nil {
		t.Fatal(err)
	}
	if instructions := jsonField(request, "instructions"); instructions != "be helpful\n\nalso be brief" {
		t.Fatalf("instructions = %v", instructions)
	}
	input, _ := jsonField(request, "input").([]any)
	if len(input) != 1 {
		t.Fatalf("input = %v", input)
	}
	if jsonField(input[0], "type") != "message" || jsonField(input[0], "role") != "user" {
		t.Fatalf("input = %v", input)
	}
	content, _ := jsonField(input[0], "content").([]any)
	if len(content) != 1 || jsonField(content[0], "type") != "input_text" || jsonField(content[0], "text") != "hi" {
		t.Fatalf("content = %v", content)
	}
	for _, key := range []string{"tools", "tool_choice", "parallel_tool_calls", "max_output_tokens"} {
		if _, ok := jsonMember(request, key); ok {
			t.Fatalf("%s present: %v", key, request)
		}
	}
	if jsonField(request, "store") != false || jsonField(request, "stream") != true {
		t.Fatalf("request = %v", request)
	}
}

func TestSSEParserStreamsTextDeltasAndToolCalls(t *testing.T) {
	raw := "data: {\"type\":\"response.created\",\"response\":{}}\n\n" +
		"data: {\"type\":\"response.output_text.delta\",\"delta\":\"hel\"}\n\n" +
		"data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n" +
		"data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"name\":\"search\",\"arguments\":\"{\\\"q\\\":\\\"x\\\"}\",\"call_id\":\"fc_1\"}}\n\n" +
		"data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
	response, collected, err := driveCodex(t, raw)
	if err != nil {
		t.Fatalf("stream completes: %v", err)
	}
	calls, ok := response.(LlmResponseToolCalls)
	if !ok || calls.Text != "hello" || len(calls.Calls) != 1 || calls.Calls[0].Function.Name != "search" || calls.Calls[0].ID != "fc_1" {
		t.Fatalf("response = %#v", response)
	}
	if collected != "hello" {
		t.Fatalf("collected = %q", collected)
	}
}

// Some short replies ship the whole message in a single output_item.done
// event with no preceding deltas; the parser must still surface the text.
func TestSSEParserBackfillsTextFromOutputItemDoneWhenNoDeltas(t *testing.T) {
	raw := "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"ok\"}]}}\n\n" +
		"data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
	response, collected, err := driveCodex(t, raw)
	if err != nil {
		t.Fatalf("stream completes: %v", err)
	}
	if text, ok := response.(LlmResponseText); !ok || text.Text != "ok" {
		t.Fatalf("response = %#v", response)
	}
	if collected != "ok" {
		t.Fatalf("collected = %q", collected)
	}
}

// A `reasoning` output_item.done ahead of the response's own text is
// captured for replay on the next turn.
func TestSSEParserCapturesReasoningItemForReplay(t *testing.T) {
	raw := "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc_1\",\"content\":[],\"summary\":[]}}\n\n" +
		"data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n" +
		"data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
	response, _, err := driveCodex(t, raw)
	if err != nil {
		t.Fatalf("stream completes: %v", err)
	}
	text, ok := response.(LlmResponseText)
	if !ok || text.Text != "ok" {
		t.Fatalf("response = %#v", response)
	}
	item := text.CodexReasoning
	if item == nil || item.ID != "rs_1" || item.EncryptedContent == nil || *item.EncryptedContent != "enc_1" ||
		len(item.Content) != 0 || len(item.Summary) != 0 {
		t.Fatalf("reasoning = %#v", item)
	}
}

func TestSSEParserSurfacesResponseFailedAsError(t *testing.T) {
	raw := "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit_exceeded\",\"message\":\"slow down\"}}}\n\n"
	_, _, err := driveCodex(t, raw)
	if err == nil {
		t.Fatal("response.failed must surface as Err")
	}
	if !strings.Contains(err.Error(), "rate_limit_exceeded") || !strings.Contains(err.Error(), "slow down") {
		t.Fatalf("got: %v", err)
	}
}

func TestSSEParserErrorsOnEOFWithoutResponseCompleted(t *testing.T) {
	raw := "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n"
	_, collected, err := driveCodex(t, raw)
	if !isIncompleteStreamError(err) {
		t.Fatalf("EOF before response.completed must be incomplete: %v", err)
	}
	if collected != "partial" {
		t.Fatalf("collected = %q", collected)
	}
}

func manifestEntry(visibility, version string) codexManifestModelEntry {
	return codexManifestModelEntry{Visibility: &visibility, MinimalClientVersion: &version}
}

func TestExtractsLatestVisibleMinimalClientVersionFromManifest(t *testing.T) {
	models := []codexManifestModelEntry{
		manifestEntry("list", "0.143.0"),
		manifestEntry("hide", "0.200.0"),
		manifestEntry("list", "0.144.0-alpha.10"),
		manifestEntry("none", "0.300.0"),
	}
	if version := latestVisibleMinimalClientVersion(models); version == nil || *version != "0.144.0" {
		t.Fatalf("version = %v", version)
	}
}

func TestManifestVersionToleratesUnknownVisibleStatesWhenListIsAbsent(t *testing.T) {
	models := []codexManifestModelEntry{
		manifestEntry("public", "0.145.0"),
		manifestEntry("hide", "0.200.0"),
		manifestEntry("none", "0.300.0"),
	}
	if version := latestVisibleMinimalClientVersion(models); version == nil || *version != "0.145.0" {
		t.Fatalf("version = %v", version)
	}
}
