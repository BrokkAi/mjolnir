// Port of brokk-anvil-client 0.28.6 src/responses_api.rs (the part Mjolnir uses).

// The Responses API request and stream shared by the Grok and Meta clients.

package anvilclient

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// responsesRequestBody is the Responses API request.
type responsesRequestBody struct {
	Model              string                     `json:"model"`
	Instructions       *string                    `json:"instructions,omitempty"`
	Input              []json.RawMessage          `json:"input"`
	Tools              []json.RawMessage          `json:"tools,omitempty"`
	ToolChoice         *string                    `json:"tool_choice,omitempty"`
	ParallelToolCalls  bool                       `json:"parallel_tool_calls"`
	Stream             bool                       `json:"stream"`
	Store              bool                       `json:"store"`
	PreviousResponseID *string                    `json:"previous_response_id,omitempty"`
	Reasoning          *responsesReasoningConfig  `json:"reasoning,omitempty"`
	Include            []string                   `json:"include,omitempty"`
	Text               *responsesTextConfigFormat `json:"text,omitempty"`
}

type responsesReasoningConfig struct {
	Effort  *string `json:"effort,omitempty"`
	Summary *string `json:"summary,omitempty"`
}

// responsesRequestOptions chooses how a Responses request carries state.
type responsesRequestOptions struct {
	store              bool
	previousResponseID *string
	// replayReasoning: MiMo accepts plain reasoning text when replaying
	// stateless history. Other providers may require encrypted items or
	// server-side state.
	replayReasoning bool
}

// buildResponsesRequest builds a Responses API request body from messages:
// the full conversation, or only the delta since a stored response (the
// caller decides which slice to pass).
func buildResponsesRequest(
	model string,
	messages []ChatMessage,
	tools []ToolDefinition,
	reasoningEffort *string,
	structuredOutput *StructuredOutputRequest,
	options responsesRequestOptions,
) (*responsesRequestBody, error) {
	var instructionsParts []string
	input := []json.RawMessage{}
	push := func(members jsonvalue.Object) error {
		item, err := jsonvalue.Marshal(members)
		if err != nil {
			return err
		}
		input = append(input, item)
		return nil
	}
	for index := range messages {
		message := &messages[index]
		switch message.Role {
		case "system":
			if text := message.ContentText(); text != "" {
				instructionsParts = append(instructionsParts, text)
			}
		case "user":
			if len(message.Content) == 0 {
				continue
			}
			if err := push(jsonvalue.Object{
				{Key: "type", Value: "message"},
				{Key: "role", Value: "user"},
				{Key: "content", Value: responsesContentParts(message.Content, "input_text")},
			}); err != nil {
				return nil, err
			}
		case "assistant":
			if options.replayReasoning && message.ReasoningContent != nil && *message.ReasoningContent != "" {
				// Stateless replay needs unique, stable item IDs, not a
				// reference to a previous server-side response.
				if err := push(jsonvalue.Object{
					{Key: "type", Value: "reasoning"},
					{Key: "id", Value: fmt.Sprintf("rs_anvil_%d", index)},
					{Key: "content", Value: []any{jsonvalue.Object{{Key: "type", Value: "reasoning_text"}, {Key: "text", Value: *message.ReasoningContent}}}},
				}); err != nil {
					return nil, err
				}
			}
			switch {
			case message.ToolCalls != nil:
				for _, call := range message.ToolCalls {
					if err := push(jsonvalue.Object{
						{Key: "type", Value: "function_call"},
						{Key: "name", Value: call.Function.Name},
						{Key: "arguments", Value: normalizeRequestToolArguments(call.Function.Arguments, call.Function.Name)},
						{Key: "call_id", Value: call.ID},
					}); err != nil {
						return nil, err
					}
				}
			case len(message.Content) != 0:
				if err := push(jsonvalue.Object{
					{Key: "type", Value: "message"},
					{Key: "role", Value: "assistant"},
					{Key: "content", Value: []any{jsonvalue.Object{{Key: "type", Value: "output_text"}, {Key: "text", Value: message.ContentText()}}}},
				}); err != nil {
					return nil, err
				}
			}
		case "tool":
			if message.ToolCallID != nil {
				if err := push(jsonvalue.Object{
					{Key: "type", Value: "function_call_output"},
					{Key: "call_id", Value: *message.ToolCallID},
					{Key: "output", Value: message.ContentText()},
				}); err != nil {
					return nil, err
				}
			}
		}
	}
	encodedTools, err := responsesTools(tools)
	if err != nil {
		return nil, err
	}
	text, err := responsesTextFormat(structuredOutput)
	if err != nil {
		return nil, err
	}
	request := &responsesRequestBody{
		Model:              model,
		Input:              input,
		Tools:              encodedTools,
		ParallelToolCalls:  true,
		Stream:             true,
		Store:              options.store,
		PreviousResponseID: options.previousResponseID,
		Text:               text,
	}
	if encodedTools != nil {
		auto := "auto"
		request.ToolChoice = &auto
	}
	if len(instructionsParts) != 0 {
		instructions := strings.Join(instructionsParts, "\n\n")
		request.Instructions = &instructions
	}
	if reasoningEffort != nil {
		effort := *reasoningEffort
		request.Reasoning = &responsesReasoningConfig{Effort: &effort}
	}
	return request, nil
}

// responsesStreamOutcome is a driven Responses stream: the response, the
// server-assigned response id, and whether it was a token-limited partial
// response, which structured callers must reject.
type responsesStreamOutcome struct {
	response   LlmResponse
	responseID *string
	incomplete bool
}

// responsesCreatedBody is the response id a `response.created` or
// `response.completed` event carries.
type responsesCreatedBody struct {
	ID *string `json:"id"`
}

// reasoningTexts is the `reasoning_text` parts of a reasoning item's
// content, or false when a part does not decode.
func reasoningTexts(content []any) (string, bool) {
	var text strings.Builder
	for _, part := range content {
		kind, ok := jsonField(part, "type").(string)
		if !ok {
			return "", false
		}
		if kind != "reasoning_text" {
			continue
		}
		if member, present := jsonMember(part, "text"); present {
			value, ok := member.(string)
			if !ok {
				return "", false
			}
			text.WriteString(value)
		}
	}
	return text.String(), true
}

func driveResponsesSSEStream(ctx context.Context, body io.ReadCloser, onToken, onThought TokenSink, idle IdleTimeouts) (responsesStreamOutcome, error) {
	done := make(chan struct{})
	defer close(done)
	defer body.Close()
	reads := readStream(body, done)
	emitToken := func(text string) {
		if onToken != nil {
			onToken(text)
		}
	}
	emitThought := func(text string) {
		if onThought != nil {
			onThought(text)
		}
	}
	var fullText, fullReasoning, pendingReasoning strings.Builder
	var toolCalls []ToolCall
	var rawBuffer []byte
	deadline := time.NewTimer(idle.FirstProgress)
	defer deadline.Stop()
	sawProgress, completed, incomplete, deltasReceived := false, false, false, false
	var failure error
	usage := TokenUsage{}
	var responseID *string

loop:
	for {
		select {
		case <-ctx.Done():
			break loop
		case <-deadline.C:
			if sawProgress {
				return responsesStreamOutcome{}, RetryableLlmErrorWith(
					fmt.Sprintf("Responses stream stalled mid-stream for %ds; aborting", int64(idle.InterChunk/time.Second)),
					RetryableLlmErrorFast("Responses stream stalled mid-stream"))
			}
			return responsesStreamOutcome{}, RetryableLlmErrorWith(
				fmt.Sprintf("Responses stream made no first token for %ds; aborting", int64(idle.FirstProgress/time.Second)),
				RetryableLlmErrorFast("Responses stream made no first token"))
		case read := <-reads:
			eofAfterBuffer := false
			switch {
			case read.err != nil && !errors.Is(read.err, io.EOF):
				// Tag mid-stream transport failures so a dropped SSE body is
				// retryable.
				return responsesStreamOutcome{}, retryableLlmContext(read.err, "Responses stream read error", RetryableLlmErrorFast("Responses stream read error"))
			case read.err == nil:
				rawBuffer = append(rawBuffer, read.data...)
			case len(rawBuffer) == 0:
				break loop
			default:
				rawBuffer = append(rawBuffer, '\n')
				eofAfterBuffer = true
			}
			madeProgress := false
		lines:
			for {
				position := bytes.IndexByte(rawBuffer, '\n')
				if position < 0 {
					break
				}
				lineBytes := rawBuffer[:position+1]
				rawBuffer = rawBuffer[position+1:]
				line := strings.TrimSpace(strings.ToValidUTF8(string(lineBytes), "�"))
				if line == "" || strings.HasPrefix(line, ":") || strings.HasPrefix(line, "event:") {
					continue
				}
				var data string
				if stripped, ok := strings.CutPrefix(line, "data: "); ok {
					data = strings.TrimSpace(stripped)
				} else if stripped, ok := strings.CutPrefix(line, "data:"); ok {
					data = strings.TrimSpace(stripped)
				} else {
					continue
				}
				if data == "[DONE]" {
					continue
				}
				var event struct {
					responsesStreamEvent
					Created *responsesCreatedBody `json:"-"`
				}
				if err := json.Unmarshal([]byte(data), &event.responsesStreamEvent); err != nil || event.Kind == nil {
					continue
				}
				var created struct {
					Response *responsesCreatedBody `json:"response"`
				}
				if json.Unmarshal([]byte(data), &created) == nil && created.Response != nil {
					event.Created = created.Response
				}
				switch *event.Kind {
				case "response.created":
					if event.Created != nil && event.Created.ID != nil {
						responseID = event.Created.ID
					}
					madeProgress = true
				case "response.output_text.delta":
					if event.Delta != nil {
						emitToken(*event.Delta)
						fullText.WriteString(*event.Delta)
						deltasReceived = true
						madeProgress = true
					}
				case "response.output_item.done":
					if len(event.Item) == 0 {
						continue
					}
					item, ok := parseResponsesOutputItem(event.Item)
					if !ok {
						continue
					}
					switch item.kind {
					case "reasoning":
						text, ok := reasoningTexts(item.content)
						if !ok {
							continue
						}
						if pendingReasoning.Len() == 0 && text != "" {
							emitThought(text)
						}
						if text == "" {
							fullReasoning.WriteString(pendingReasoning.String())
						} else {
							fullReasoning.WriteString(text)
						}
						pendingReasoning.Reset()
						madeProgress = true
					case "message":
						if item.role != nil && *item.role == "assistant" && !deltasReceived {
							for _, text := range item.texts {
								emitToken(text)
								fullText.WriteString(text)
							}
						}
						madeProgress = true
					case "function_call":
						resolvedID := fmt.Sprintf("call_%d", len(toolCalls))
						if item.callID != nil {
							resolvedID = *item.callID
						} else if item.id != nil {
							resolvedID = *item.id
						}
						arguments, err := normalizeStreamedToolArguments(resolvedID, item.name, item.arguments, "Responses SSE")
						if err != nil {
							return responsesStreamOutcome{}, err
						}
						toolCalls = append(toolCalls, ToolCall{ID: resolvedID, Type: "function", Function: FunctionCall{Name: item.name, Arguments: arguments}})
						madeProgress = true
					}
				case "response.completed":
					if event.Created != nil && event.Created.ID != nil {
						responseID = event.Created.ID
					}
					if event.Response != nil && event.Response.Usage != nil {
						usage = event.Response.Usage.intoUsage()
					}
					completed = true
					break lines
				case "response.failed":
					message := "unknown error"
					if event.Response != nil && event.Response.Error != nil {
						code, text := "unknown", ""
						if event.Response.Error.Code != nil {
							code = *event.Response.Error.Code
						}
						if event.Response.Error.Message != nil {
							text = *event.Response.Error.Message
						}
						message = code + ": " + text
					}
					failure = RetryableLlmErrorForResponsesFailure("Responses stream failed: "+message, message)
					completed = true
					break lines
				case "response.incomplete":
					incomplete = true
					if event.Response == nil {
						slog.Warn("Responses stream ended with incomplete response without body")
						continue
					}
					if event.Response.Usage != nil {
						usage = event.Response.Usage.intoUsage()
					}
					reason := "unknown"
					if details := event.Response.IncompleteDetails; details != nil && details.Reason != nil {
						reason = *details.Reason
					}
					if reason == "max_output_tokens" {
						if strings.TrimSpace(fullText.String()) == "" && len(toolCalls) == 0 {
							failure = OutputBudgetExhaustedError{}
						} else {
							slog.Warn("Responses stream became incomplete after emitting output; returning truncated content", "reason", reason)
						}
						completed = true
						break lines
					}
					slog.Warn("Responses stream ended with incomplete response", "reason", reason)
				case "response.reasoning_text.delta":
					if event.Delta != nil {
						emitThought(*event.Delta)
						pendingReasoning.WriteString(*event.Delta)
						madeProgress = true
					}
				case "response.reasoning_summary_text.delta":
					if event.Delta != nil {
						emitThought(*event.Delta)
						madeProgress = true
					}
				default:
					madeProgress = true
				}
			}
			if completed {
				break loop
			}
			if madeProgress {
				sawProgress = true
				if !deadline.Stop() {
					select {
					case <-deadline.C:
					default:
					}
				}
				deadline.Reset(idle.InterChunk)
			}
			if eofAfterBuffer {
				break loop
			}
		}
	}

	if failure != nil {
		return responsesStreamOutcome{}, failure
	}
	fullReasoning.WriteString(pendingReasoning.String())
	var reasoningContent *string
	if fullReasoning.Len() != 0 {
		text := fullReasoning.String()
		reasoningContent = &text
	}
	text := LlmResponseText{Text: fullText.String(), ReasoningContent: reasoningContent, Usage: usage}
	if ctx.Err() != nil {
		return responsesStreamOutcome{response: text, responseID: responseID, incomplete: incomplete}, nil
	}
	if !completed {
		return responsesStreamOutcome{}, NewIncompleteStreamError("Responses SSE", "response.completed")
	}
	if len(toolCalls) == 0 {
		return responsesStreamOutcome{response: text, responseID: responseID, incomplete: incomplete}, nil
	}
	return responsesStreamOutcome{
		response:   LlmResponseToolCalls{Text: fullText.String(), ReasoningContent: reasoningContent, Calls: toolCalls, Usage: usage},
		responseID: responseID,
		incomplete: incomplete,
	}, nil
}
