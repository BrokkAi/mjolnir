// Port of brokk-anvil-client 0.28.6 src/infer.rs (the part Mjolnir uses).

// One-shot, tool-free structured inference over an existing LLM backend.

package anvilclient

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"time"
)

// InferRole serializes snake_case.
type InferRole string

const (
	InferRoleSystem InferRole = "system"
	InferRoleUser   InferRole = "user"
)

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
	panic("unported: brokk-anvil-client/src/infer.rs infer_structured")
}
