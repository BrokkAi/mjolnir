// Port of brokk-anvil-client 0.28.6 src/infer.rs (the part Mjolnir uses).

// One-shot, tool-free structured inference over an existing LLM backend.

package anvilclient

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"slices"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// InferRole serializes snake_case.
type InferRole string

const (
	InferRoleSystem InferRole = "system"
	InferRoleUser   InferRole = "user"
)

// UnmarshalJSON accepts only the two variants, as serde does: a request
// message is never an assistant or tool message.
func (r *InferRole) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch role := InferRole(text); role {
	case InferRoleSystem, InferRoleUser:
		*r = role
		return nil
	}
	return fmt.Errorf("unknown variant `%s`, expected `system` or `user`", text)
}

// InferMessage denies unknown fields when decoded.
type InferMessage struct {
	Role    InferRole `json:"role"`
	Content string    `json:"content"`
}

func (m *InferMessage) UnmarshalJSON(data []byte) error {
	type plain InferMessage
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	return decoder.Decode((*plain)(m))
}

func InferMessageSystem(content string) InferMessage {
	return InferMessage{Role: InferRoleSystem, Content: content}
}

func InferMessageUser(content string) InferMessage {
	return InferMessage{Role: InferRoleUser, Content: content}
}

// StructuredInferRequest denies unknown fields when decoded.
type StructuredInferRequest struct {
	Messages   []InferMessage `json:"messages"`
	SchemaName string         `json:"schema_name"`
	// Schema is a JSON value.
	Schema any `json:"schema"`
}

func (r *StructuredInferRequest) UnmarshalJSON(data []byte) error {
	type plain StructuredInferRequest
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	return decoder.Decode((*plain)(r))
}

type InferOptions struct {
	ReasoningEffort   *string
	ServiceTier       *string
	IdleTimeouts      IdleTimeouts
	ValidationRetries int
}

// DefaultInferOptions is Rust's `InferOptions::default()`.
func DefaultInferOptions() InferOptions {
	return InferOptions{
		IdleTimeouts: IdleTimeouts{
			FirstProgress: time.Duration(DefaultIdleChunkTimeoutSecs) * time.Second,
			InterChunk:    time.Duration(DefaultInterChunkTimeoutSecs) * time.Second,
		},
		ValidationRetries: 1,
	}
}

type StructuredInferResponse struct {
	Model           string  `json:"model"`
	ReasoningEffort *string `json:"reasoning_effort"`
	ServiceTier     *string `json:"service_tier"`
	// Output is a JSON value.
	Output any        `json:"output"`
	Usage  InferUsage `json:"usage"`
}

type InferUsage struct {
	InputTokens       uint64 `json:"input_tokens"`
	OutputTokens      uint64 `json:"output_tokens"`
	ThoughtTokens     uint64 `json:"thought_tokens"`
	CachedReadTokens  uint64 `json:"cached_read_tokens"`
	CachedWriteTokens uint64 `json:"cached_write_tokens"`
}

func InferUsageFromTokenUsage(value TokenUsage) InferUsage {
	return InferUsage(value)
}

type InferErrorKind int

const (
	InferErrorKindInvalidRequest InferErrorKind = iota
	InferErrorKindCancelled
	InferErrorKindAuthentication
	InferErrorKindRateLimited
	InferErrorKindContextLength
	InferErrorKindTransport
	InferErrorKindStructuredOutput
	InferErrorKindProvider
)

// String is the Rust Debug name, which callers put in error messages.
func (k InferErrorKind) String() string {
	switch k {
	case InferErrorKindInvalidRequest:
		return "InvalidRequest"
	case InferErrorKindCancelled:
		return "Cancelled"
	case InferErrorKindAuthentication:
		return "Authentication"
	case InferErrorKindRateLimited:
		return "RateLimited"
	case InferErrorKindContextLength:
		return "ContextLength"
	case InferErrorKindTransport:
		return "Transport"
	case InferErrorKindStructuredOutput:
		return "StructuredOutput"
	case InferErrorKindProvider:
		return "Provider"
	}
	return fmt.Sprintf("InferErrorKind(%d)", int(k))
}

// InferError is a failed inference and what kind of failure it was.
type InferError struct {
	kind   InferErrorKind
	source error
}

func newInferError(kind InferErrorKind, source error) *InferError {
	return &InferError{kind: kind, source: source}
}

func (e *InferError) Kind() InferErrorKind { return e.kind }

// Error is the source's text with its whole context chain, as Rust's
// alternate Display (`{error:#}`) prints it.
func (e *InferError) Error() string { return e.source.Error() }

func (e *InferError) Unwrap() error { return e.source }

// InferStructured makes one tool-free request for output matching
// request's schema, validating (and retrying invalid output up to
// options.ValidationRetries times) locally. A failure is an *InferError.
func InferStructured(ctx context.Context, backend LlmBackend, model string, request StructuredInferRequest, options InferOptions) (StructuredInferResponse, error) {
	if strings.TrimSpace(model) == "" {
		return StructuredInferResponse{}, newInferError(InferErrorKindInvalidRequest, errors.New("model must be non-empty"))
	}
	if len(request.Messages) == 0 {
		return StructuredInferResponse{}, newInferError(InferErrorKindInvalidRequest, errors.New("messages must contain at least one system or user message"))
	}
	messages := make([]ChatMessage, 0, len(request.Messages)+1)
	for index, message := range request.Messages {
		if message.Content == "" {
			return StructuredInferResponse{}, newInferError(InferErrorKindInvalidRequest, fmt.Errorf("messages[%d].content must be non-empty", index))
		}
		switch message.Role {
		case InferRoleSystem:
			messages = append(messages, ChatMessageSystem(message.Content))
		case InferRoleUser:
			messages = append(messages, ChatMessageUser(message.Content))
		default:
			return StructuredInferResponse{}, newInferError(InferErrorKindInvalidRequest,
				fmt.Errorf("unknown variant `%s`, expected `system` or `user`", message.Role))
		}
	}
	if _, isObject := request.Schema.(jsonvalue.Object); strings.TrimSpace(request.SchemaName) == "" || !isObject {
		return StructuredInferResponse{}, newInferError(InferErrorKindInvalidRequest, errors.New("schema_name must be non-empty and schema must be an object"))
	}
	if _, err := compileSchema(request.Schema); err != nil {
		return StructuredInferResponse{}, newInferError(InferErrorKindInvalidRequest, withContext(err, "compiling structured-output schema"))
	}
	structuredOutput := StructuredOutputRequest{
		SchemaName: request.SchemaName,
		Schema:     request.Schema,
	}
	// JSON-object fallbacks need an in-band schema, but dynamic schemas must
	// follow the caller's stable prompt/article prefix rather than displace
	// it.
	if !SupportsNativeStructuredOutput(backend) {
		messages = append(messages, ChatMessageUser(jsonSchemaInstruction(&structuredOutput)))
	}
	var totalUsage TokenUsage
	validationAttempt := 0
	var output any
	for {
		response, err := StreamChatNoVisibleOutputWithRetry(ctx, backend, "bare structured inference", func() StreamChatRequest {
			structured := structuredOutput
			var reasoningEffort, serviceTier *string
			if options.ReasoningEffort != nil {
				effort := *options.ReasoningEffort
				reasoningEffort = &effort
			}
			if options.ServiceTier != nil {
				tier := *options.ServiceTier
				serviceTier = &tier
			}
			return StreamChatRequest{
				Model:            model,
				Messages:         slices.Clone(messages),
				ReasoningEffort:  reasoningEffort,
				ServiceTier:      serviceTier,
				StructuredOutput: &structured,
				OnToken:          func(string) {},
				OnThought:        func(string) {},
				IdleTimeouts:     options.IdleTimeouts,
			}
		})
		if err != nil {
			return StructuredInferResponse{}, classifyError(ctx, err)
		}
		var text string
		var usage TokenUsage
		switch response := response.(type) {
		case LlmResponseText:
			text, usage = response.Text, response.Usage
		default:
			return StructuredInferResponse{}, newInferError(InferErrorKindProvider,
				errors.New("inference backend returned tool calls even though no tools were supplied"))
		}
		totalUsage.Add(usage)
		success, failure := validateResponse(&structuredOutput, text)
		if success != nil {
			output = success.ValidatedOutput
			break
		}
		if validationAttempt < options.ValidationRetries {
			validationAttempt++
			messages = append(messages, ChatMessageAssistant(text), ChatMessageUser(validationRetryPrompt(failure)))
			continue
		}
		return StructuredInferResponse{}, newInferError(InferErrorKindStructuredOutput,
			fmt.Errorf("structured output validation failed: %s", debugSchemaErrors(failure.Errors)))
	}
	return StructuredInferResponse{
		Model:           model,
		ReasoningEffort: options.ReasoningEffort,
		ServiceTier:     options.ServiceTier,
		Output:          output,
		Usage:           InferUsageFromTokenUsage(totalUsage),
	}, nil
}

func classifyError(ctx context.Context, err error) *InferError {
	if ctx.Err() != nil {
		return newInferError(InferErrorKindCancelled, err)
	}
	message := asciiLower(err.Error())
	contains := func(markers ...string) bool {
		for _, marker := range markers {
			if strings.Contains(message, marker) {
				return true
			}
		}
		return false
	}
	var retryable *RetryableLlmError
	switch {
	case contains("context length", "maximum context", "too many tokens", "request too large"):
		return newInferError(InferErrorKindContextLength, err)
	case contains("unauthorized", "http 401", "credentials", "login", "auth.json"):
		return newInferError(InferErrorKindAuthentication, err)
	case contains("rate limit", "http 429", "quota exhausted"):
		return newInferError(InferErrorKindRateLimited, err)
	case errors.As(err, &retryable):
		return newInferError(InferErrorKindTransport, err)
	}
	return newInferError(InferErrorKindProvider, err)
}
