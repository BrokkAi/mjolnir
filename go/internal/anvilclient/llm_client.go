// Port of brokk-anvil-client 0.28.6 src/llm_client.rs (the part Mjolnir uses).

package anvilclient

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// DefaultIdleChunkTimeoutSecs is the default value for
// `--llm-idle-timeout-secs`: how long a stream may go without its first
// meaningful chunk.
//
// Also used as the pre-stream HTTP send budget for streaming requests: if
// response headers do not arrive within this window, the request is retried
// instead of waiting for the transport's last-resort wall-clock timeout.
const DefaultIdleChunkTimeoutSecs uint64 = 120

// DefaultInterChunkTimeoutSecs is the default value for
// `--llm-stall-timeout-secs`: the maximum gap between meaningful chunks
// after the first progress event has arrived.
//
// 30s was tried and reverted: a benchmark sweep showed ~11 stall-aborts per
// task, which looked like a provider defect worth detecting faster. The
// stalls turned out to be an artifact of that sweep's sealed container
// network (0.8/task unsealed vs 10.9/task sealed, same 60s budget). At 30s
// on a healthy network the abort rate tripled against 60s with nothing left
// to detect, and every abort costs a full request retry.
const DefaultInterChunkTimeoutSecs uint64 = 60

// IdleTimeouts is the first-progress timeout and the post-progress
// inter-chunk stall timeout before a backend aborts a stream.
type IdleTimeouts struct {
	FirstProgress time.Duration
	InterChunk    time.Duration
}

// IdleTimeoutsUniform uses one timeout for both phases.
func IdleTimeoutsUniform(timeout time.Duration) IdleTimeouts {
	return IdleTimeouts{FirstProgress: timeout, InterChunk: timeout}
}

// TokenSink receives streamed text deltas (Rust `Box<dyn FnMut(&str)>`).
type TokenSink func(text string)

// ToolDefinition is one OpenAI-compatible tool offered to the model.
type ToolDefinition struct {
	Type     string      `json:"type"`
	Function FunctionDef `json:"function"`
}

type FunctionDef struct {
	Name        string `json:"name"`
	Description string `json:"description"`
	// Parameters is a JSON value.
	Parameters any `json:"parameters"`
}

// ToolCall is one tool call the model asked for.
type ToolCall struct {
	ID       string       `json:"id"`
	Type     string       `json:"type"`
	Function FunctionCall `json:"function"`
}

type FunctionCall struct {
	Name      string `json:"name"`
	Arguments string `json:"arguments"`
}

// TokenUsage is the provider-reported token counts for a single StreamChat
// call.
//
// Field names mirror the ACP `session/usage` RFD so they map 1:1 into ACP's
// Usage. Counts are populated from the upstream LLM's own accounting
// (OpenAI's `usage` SSE chunk, the Codex Responses API `response.completed`
// event); backends that don't surface usage leave the fields at zero.
type TokenUsage struct {
	InputTokens       uint64
	OutputTokens      uint64
	ThoughtTokens     uint64
	CachedReadTokens  uint64
	CachedWriteTokens uint64
}

// IsZero is whether every token bucket is zero.
func (u TokenUsage) IsZero() bool {
	return u == TokenUsage{}
}

// TotalTokens is the sum of all categories, saturating. Matches the ACP
// `total_tokens` semantics: cached counts are included.
func (u TokenUsage) TotalTokens() uint64 {
	total := uint64(0)
	for _, count := range []uint64{u.InputTokens, u.OutputTokens, u.ThoughtTokens, u.CachedReadTokens, u.CachedWriteTokens} {
		total = saturatingAdd(total, count)
	}
	return total
}

// Add merges another reading into this one, saturating. Used to roll up
// per-call usage into a per-turn or per-session total.
func (u *TokenUsage) Add(other TokenUsage) {
	u.InputTokens = saturatingAdd(u.InputTokens, other.InputTokens)
	u.OutputTokens = saturatingAdd(u.OutputTokens, other.OutputTokens)
	u.ThoughtTokens = saturatingAdd(u.ThoughtTokens, other.ThoughtTokens)
	u.CachedReadTokens = saturatingAdd(u.CachedReadTokens, other.CachedReadTokens)
	u.CachedWriteTokens = saturatingAdd(u.CachedWriteTokens, other.CachedWriteTokens)
}

func saturatingAdd(left, right uint64) uint64 {
	if sum := left + right; sum >= left {
		return sum
	}
	return ^uint64(0)
}

// LlmResponse is what the LLM returned: either final text
// (LlmResponseText) or tool calls to execute (LlmResponseToolCalls), plus
// the provider-reported token usage for the call.
type LlmResponse interface {
	isLlmResponse()
}

type LlmResponseText struct {
	Text             string
	ReasoningContent *string
	Usage            TokenUsage
	// CodexReasoning is set only by the Codex (ChatGPT-auth Responses API)
	// backend, when the server returned an encrypted `reasoning` item
	// alongside this response. Every other backend leaves it nil.
	CodexReasoning *CodexReasoningItem
}

type LlmResponseToolCalls struct {
	Text             string
	ReasoningContent *string
	Calls            []ToolCall
	Usage            TokenUsage
	// CodexReasoning: see LlmResponseText.CodexReasoning.
	CodexReasoning *CodexReasoningItem
}

func (LlmResponseText) isLlmResponse()      {}
func (LlmResponseToolCalls) isLlmResponse() {}

// LlmResponseUsage is the token usage of either response.
func LlmResponseUsage(response LlmResponse) TokenUsage {
	switch response := response.(type) {
	case LlmResponseText:
		return response.Usage
	case LlmResponseToolCalls:
		return response.Usage
	}
	return TokenUsage{}
}

// StreamChatRequest is the parameters for a single streamed chat request.
// Rust's `cancel` token is the context passed to StreamChat.
type StreamChatRequest struct {
	Model            string
	Messages         []ChatMessage
	Tools            []ToolDefinition
	ReasoningEffort  *string
	ServiceTier      *string
	Temperature      *float64
	StructuredOutput *StructuredOutputRequest
	OnToken          TokenSink
	OnThought        TokenSink
	IdleTimeouts     IdleTimeouts
}

// ChatMessage is one chat message, extended for tool calling. Its JSON
// encoding is hand-written in Rust (CodexReasoning never reaches a request
// body), so MarshalJSON is its own.
type ChatMessage struct {
	Role             string            `json:"role"`
	Content          []ChatContentPart `json:"content,omitempty"`
	ToolCalls        []ToolCall        `json:"tool_calls,omitempty"`
	ToolCallID       *string           `json:"tool_call_id,omitempty"`
	Name             *string           `json:"name,omitempty"`
	ReasoningContent *string           `json:"reasoning_content,omitempty"`
	// CodexReasoning is the Codex backend's encrypted `reasoning` item for
	// the response that produced this assistant message. Codex only.
	CodexReasoning *CodexReasoningItem `json:"codex_reasoning,omitempty"`
}

// MarshalJSON is Rust's hand-written Serialize: CodexReasoning never
// reaches a request body, and reasoning content is written twice, as
// `reasoning_content` and `reasoning`.
func (m ChatMessage) MarshalJSON() ([]byte, error) {
	object := jsonvalue.Object{{Key: "role", Value: m.Role}}
	if len(m.Content) != 0 {
		parts, err := chatContentPartsValue(m.Content)
		if err != nil {
			return nil, err
		}
		object = append(object, jsonvalue.Member{Key: "content", Value: parts})
	}
	return chatMessageTail(object, &m)
}

// chatMessageTail appends the members after `content` that both message
// encodings share.
func chatMessageTail(object jsonvalue.Object, m *ChatMessage) ([]byte, error) {
	if m.ToolCalls != nil {
		calls, err := toJSONValue(m.ToolCalls)
		if err != nil {
			return nil, err
		}
		object = append(object, jsonvalue.Member{Key: "tool_calls", Value: calls})
	}
	if m.ToolCallID != nil {
		object = append(object, jsonvalue.Member{Key: "tool_call_id", Value: *m.ToolCallID})
	}
	if m.Name != nil {
		object = append(object, jsonvalue.Member{Key: "name", Value: *m.Name})
	}
	if m.ReasoningContent != nil {
		object = append(object,
			jsonvalue.Member{Key: "reasoning_content", Value: *m.ReasoningContent},
			jsonvalue.Member{Key: "reasoning", Value: *m.ReasoningContent})
	}
	return jsonvalue.Marshal(object)
}

// UnmarshalJSON is Rust's derived Deserialize: `role` is required and
// `content` defaults to no parts.
func (m *ChatMessage) UnmarshalJSON(data []byte) error {
	var wire struct {
		Role             *string             `json:"role"`
		Content          []json.RawMessage   `json:"content"`
		ToolCalls        []ToolCall          `json:"tool_calls"`
		ToolCallID       *string             `json:"tool_call_id"`
		Name             *string             `json:"name"`
		ReasoningContent *string             `json:"reasoning_content"`
		CodexReasoning   *CodexReasoningItem `json:"codex_reasoning"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Role == nil {
		return errors.New("missing field `role`")
	}
	content := make([]ChatContentPart, 0, len(wire.Content))
	for _, raw := range wire.Content {
		var part struct {
			Type     *string `json:"type"`
			Text     *string `json:"text"`
			ImageURL *string `json:"image_url"`
		}
		if err := json.Unmarshal(raw, &part); err != nil {
			return err
		}
		switch {
		case part.Type == nil:
			return errors.New("missing field `type`")
		case *part.Type == "text" && part.Text != nil:
			content = append(content, ChatContentPartText{Text: *part.Text})
		case *part.Type == "text":
			return errors.New("missing field `text`")
		case *part.Type == "image" && part.ImageURL != nil:
			content = append(content, ChatContentPartImage{ImageURL: *part.ImageURL})
		case *part.Type == "image":
			return errors.New("missing field `image_url`")
		default:
			return fmt.Errorf("unknown variant `%s`, expected `text` or `image`", *part.Type)
		}
	}
	*m = ChatMessage{
		Role:             *wire.Role,
		Content:          content,
		ToolCalls:        wire.ToolCalls,
		ToolCallID:       wire.ToolCallID,
		Name:             wire.Name,
		ReasoningContent: wire.ReasoningContent,
		CodexReasoning:   wire.CodexReasoning,
	}
	return nil
}

func ChatMessageSystem(content string) ChatMessage {
	return ChatMessage{Role: "system", Content: []ChatContentPart{ChatContentPartText{Text: content}}}
}

func ChatMessageUser(content string) ChatMessage {
	return ChatMessage{Role: "user", Content: []ChatContentPart{ChatContentPartText{Text: content}}}
}

func ChatMessageAssistant(content string) ChatMessage {
	return ChatMessage{Role: "assistant", Content: []ChatContentPart{ChatContentPartText{Text: content}}}
}

// TextContent is the first text part.
func (m *ChatMessage) TextContent() (string, bool) {
	for _, part := range m.Content {
		if text, ok := part.(ChatContentPartText); ok {
			return text.Text, true
		}
	}
	return "", false
}

// ContentText is the text parts joined by newlines.
func (m *ChatMessage) ContentText() string {
	var texts []string
	for _, part := range m.Content {
		if text, ok := part.(ChatContentPartText); ok {
			texts = append(texts, text.Text)
		}
	}
	return strings.Join(texts, "\n")
}

// chatContentPartsValue is the parts as Rust's hand-written Serialize
// writes them: `{"type":"text","text":...}` or
// `{"type":"image_url","image_url":{"url":...}}`.
func chatContentPartsValue(parts []ChatContentPart) ([]any, error) {
	values := make([]any, 0, len(parts))
	for _, part := range parts {
		switch part := part.(type) {
		case ChatContentPartText:
			values = append(values, jsonvalue.Object{{Key: "type", Value: "text"}, {Key: "text", Value: part.Text}})
		case ChatContentPartImage:
			values = append(values, jsonvalue.Object{
				{Key: "type", Value: "image_url"},
				{Key: "image_url", Value: jsonvalue.Object{{Key: "url", Value: part.ImageURL}}},
			})
		default:
			return nil, fmt.Errorf("unknown chat content part %T", part)
		}
	}
	return values, nil
}

// toJSONValue re-reads a Go value's encoding as a JSON value.
func toJSONValue(value any) (any, error) {
	encoded, err := json.Marshal(value)
	if err != nil {
		return nil, err
	}
	return jsonvalue.Parse(encoded)
}

// CodexReasoningItem is an encrypted reasoning item the Codex backend
// returned, carried forward on the assistant message it preceded.
type CodexReasoningItem struct {
	ID               string  `json:"id"`
	EncryptedContent *string `json:"encrypted_content"`
	// Content and Summary are JSON values.
	Content []any `json:"content"`
	Summary []any `json:"summary"`
}

// ChatContentPart is ChatContentPartText or ChatContentPartImage.
type ChatContentPart interface {
	isChatContentPart()
}

type ChatContentPartText struct {
	Text string
}

type ChatContentPartImage struct {
	ImageURL string
}

func (ChatContentPartText) isChatContentPart()  {}
func (ChatContentPartImage) isChatContentPart() {}

// ReasoningLevelPreset is one reasoning-effort preset advertised by the
// server for a given model. Mirrors the `supported_reasoning_levels[]`
// entries returned by the ChatGPT `/models` endpoint. Backends without
// per-effort presets leave the per-model list empty.
type ReasoningLevelPreset struct {
	Effort      string `json:"effort"`
	Description string `json:"description"`
}

// ModelServiceTier is one optional service tier advertised by a provider
// for a model. Codex's ChatGPT subscription catalog uses this for
// fast/priority mode.
type ModelServiceTier struct {
	ID          string `json:"id"`
	Name        string `json:"name"`
	Description string `json:"description"`
}

// ModelPricing is per-token USD pricing published by a provider's model
// catalog.
type ModelPricing struct {
	InputCostPerTokenUsd  float64 `json:"input_cost_per_token_usd"`
	OutputCostPerTokenUsd float64 `json:"output_cost_per_token_usd"`
}

// EstimateCostUsd estimates the provider-billed cost for one call. Cached
// input tokens are billed at the input rate and thought tokens at the
// output rate.
func (p ModelPricing) EstimateCostUsd(usage TokenUsage) float64 {
	billedInput := usage.InputTokens + usage.CachedReadTokens + usage.CachedWriteTokens
	billedOutput := usage.OutputTokens + usage.ThoughtTokens
	return float64(billedInput)*p.InputCostPerTokenUsd + float64(billedOutput)*p.OutputCostPerTokenUsd
}

type ModelDiscoveryNotice struct {
	Source  string
	Message string
}

// ModelMetadata is the richer model descriptor surfaced through
// ListModelMetadata. ID is the wire identifier the backend expects in
// StreamChat; the optional reasoning fields are populated only for backends
// whose catalog publishes them.
type ModelMetadata struct {
	ID                       string                 `json:"id"`
	DefaultReasoningLevel    *string                `json:"default_reasoning_level"`
	SupportedReasoningLevels []ReasoningLevelPreset `json:"supported_reasoning_levels"`
	ServiceTiers             []ModelServiceTier     `json:"service_tiers"`
	// SupportsImages is tri-state image-input support as published by the
	// provider; nil means the backend does not expose reliable modality
	// info.
	SupportsImages *bool `json:"supports_images"`
	// ContextLength is the maximum context window in tokens, as published
	// by the provider; nil when the backend doesn't expose it.
	ContextLength *uint32 `json:"context_length"`
	// Pricing is provider-published per-token pricing, when available.
	Pricing *ModelPricing `json:"pricing"`
}

// ModelMetadataIDOnly lifts a bare model id to a metadata record with no
// reasoning data.
func ModelMetadataIDOnly(id string) ModelMetadata {
	return ModelMetadata{ID: id, SupportedReasoningLevels: []ReasoningLevelPreset{}, ServiceTiers: []ModelServiceTier{}}
}

type ResolvedModelInfo struct {
	ConfiguredModel  string
	ResolvedProvider *string
	ResolvedModel    string
}

// LlmBackend is one LLM provider. The Rust trait's methods with default
// bodies are optional interfaces here (ModelMetadataLister,
// NativeStructuredOutputSupporter, ModelInfoResolver,
// ModelDiscoveryNoticeTaker); call them through the package functions of
// the same name, which apply the Rust default when a backend does not
// implement one.
type LlmBackend interface {
	ListModels(ctx context.Context) ([]string, error)

	// StreamChat streams a chat completion. ReasoningEffort is honored only
	// by backends that route to a reasoning-capable endpoint; OnThought
	// receives chain-of-thought deltas separate from the assistant text on
	// OnToken. ctx is the request's cancellation.
	StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error)
}

// ModelMetadataLister is a backend whose catalog carries per-model
// metadata.
type ModelMetadataLister interface {
	ListModelMetadata(ctx context.Context) ([]ModelMetadata, error)
}

// NativeStructuredOutputSupporter is a backend that sends the schema
// through a native output format and needs no in-band schema instruction.
type NativeStructuredOutputSupporter interface {
	SupportsNativeStructuredOutput() bool
}

type ModelInfoResolver interface {
	ResolveModelInfo(configuredModel string) ResolvedModelInfo
}

type ModelDiscoveryNoticeTaker interface {
	TakeModelDiscoveryNotices() []ModelDiscoveryNotice
}

// ListModelMetadata is the backend's catalog with any per-model metadata.
// The Rust default lifts each id from ListModels to ModelMetadataIDOnly.
func ListModelMetadata(ctx context.Context, backend LlmBackend) ([]ModelMetadata, error) {
	if lister, ok := backend.(ModelMetadataLister); ok {
		return lister.ListModelMetadata(ctx)
	}
	ids, err := backend.ListModels(ctx)
	if err != nil {
		return nil, err
	}
	metadata := make([]ModelMetadata, 0, len(ids))
	for _, id := range ids {
		metadata = append(metadata, ModelMetadataIDOnly(id))
	}
	return metadata, nil
}

// SupportsNativeStructuredOutput is false unless the backend says
// otherwise. This is not a guarantee of provider enforcement: local
// validation remains mandatory.
func SupportsNativeStructuredOutput(backend LlmBackend) bool {
	supporter, ok := backend.(NativeStructuredOutputSupporter)
	return ok && supporter.SupportsNativeStructuredOutput()
}

func ResolveModelInfo(backend LlmBackend, configuredModel string) ResolvedModelInfo {
	if resolver, ok := backend.(ModelInfoResolver); ok {
		return resolver.ResolveModelInfo(configuredModel)
	}
	return ResolvedModelInfo{ConfiguredModel: configuredModel, ResolvedModel: configuredModel}
}

func TakeModelDiscoveryNotices(backend LlmBackend) []ModelDiscoveryNotice {
	if taker, ok := backend.(ModelDiscoveryNoticeTaker); ok {
		return taker.TakeModelDiscoveryNotices()
	}
	return nil
}

// BearerTokenProvider supplies the bearer token for each request.
type BearerTokenProvider interface {
	BearerToken(ctx context.Context) (*string, error)
}

// RejectedBearerTokenProvider is a BearerTokenProvider that can answer an
// explicit 401: it returns only a token already replaced by its owner. A
// provider without it disables auth retries (the Rust default returns
// None); implementations must not replay a credential mutation here.
type RejectedBearerTokenProvider interface {
	RejectedBearerToken(ctx context.Context, rejected string) (*string, error)
}

// staticBearerToken supplies one fixed token, or none.
type staticBearerToken struct {
	token *string
}

func (t staticBearerToken) BearerToken(context.Context) (*string, error) { return t.token, nil }

// IncompleteStreamError: a stream ended before its completion marker.
type IncompleteStreamError struct {
	protocol       string
	expectedMarker string
}

func NewIncompleteStreamError(protocol, expectedMarker string) *IncompleteStreamError {
	return &IncompleteStreamError{protocol: protocol, expectedMarker: expectedMarker}
}

func (e *IncompleteStreamError) Error() string {
	return fmt.Sprintf("%s stream ended before completion marker %s", e.protocol, e.expectedMarker)
}

func isIncompleteStreamError(err error) bool {
	var incomplete *IncompleteStreamError
	return errors.As(err, &incomplete)
}

// OutputBudgetExhaustedError: the model spent its output budget before
// emitting anything; the same request would hit the same cap.
type OutputBudgetExhaustedError struct{}

func (OutputBudgetExhaustedError) Error() string {
	return "model exhausted its output token budget before emitting any text or tool calls; not retrying (the same request would hit the same cap)"
}

func IsOutputBudgetExhaustedError(err error) bool {
	var exhausted OutputBudgetExhaustedError
	return errors.As(err, &exhausted)
}

func IsRetryableLlmError(err error) bool {
	_, ok := LlmRetryTierOf(err)
	return ok
}

// LlmRetryTierOf is how patiently err may be retried, or false when it may
// not be.
func LlmRetryTierOf(err error) (LlmRetryTier, bool) {
	if IsOutputBudgetExhaustedError(err) {
		return 0, false
	}
	// A per-day quota outranks every retry marker, including one a
	// mid-stream wrapper may have layered on top of it: waiting cannot
	// clear it, so retrying only converts a loud failure into a quiet one.
	if isFatalLlmQuotaError(err) {
		return 0, false
	}
	if isIncompleteStreamError(err) {
		return LlmRetryTierFast, true
	}
	var marker *RetryableLlmError
	if errors.As(err, &marker) {
		return marker.Tier(), true
	}
	return 0, false
}

const EmptyCompletionRetryReason = "no meaningful progress: empty completion (no text, no tool calls)"

func IsDegenerateEmptyCompletion(response LlmResponse) bool {
	switch response := response.(type) {
	case LlmResponseText:
		return strings.TrimSpace(response.Text) == ""
	case LlmResponseToolCalls:
		return strings.TrimSpace(response.Text) == "" && len(response.Calls) == 0
	}
	return false
}

// StreamChatNoVisibleOutputWithRetry retries a streamed LLM request when
// the caller guarantees streamed deltas are not user-visible. ctx is the
// caller's cancellation, also passed to every request.
func StreamChatNoVisibleOutputWithRetry(ctx context.Context, llm LlmBackend, operation string, buildRequest func() StreamChatRequest) (LlmResponse, error) {
	attempt := uint64(1)
	for {
		response, err := llm.StreamChat(ctx, buildRequest())
		if err == nil {
			if ctx.Err() == nil && IsDegenerateEmptyCompletion(response) && attempt < LlmRetryTierFast.MaxAttempts() {
				slog.Warn("retrying empty LLM completion with no visible output",
					"attempt", attempt, "max_attempts", LlmRetryTierFast.MaxAttempts(), "operation", operation)
				if err := SleepBeforeRetryForTier(ctx, operation, LlmRetryTierFast, attempt, EmptyCompletionRetryReason); err != nil {
					return nil, err
				}
				attempt++
				continue
			}
			return response, nil
		}
		if ctx.Err() == nil {
			if tier, ok := LlmRetryTierOf(err); ok && attempt < tier.MaxAttempts() {
				slog.Warn("retrying transient LLM stream failure with no visible output",
					"attempt", attempt, "max_attempts", tier.MaxAttempts(), "operation", operation)
				if err := SleepBeforeRetryForTier(ctx, operation, tier, attempt, err.Error()); err != nil {
					return nil, err
				}
				attempt++
				continue
			}
		}
		return nil, err
	}
}

// reasoningWire is how a client spells per-request reasoning effort on the
// wire. Different OpenAI-compatible servers differ here, so the client
// carries the dialect rather than sniffing the base URL at send time.
type reasoningWire int

const (
	// reasoningWireOff: don't forward reasoning effort.
	reasoningWireOff reasoningWire = iota
	// reasoningWireUnified: OpenRouter / Ollama `reasoning: { effort }`.
	reasoningWireUnified
	// reasoningWireKimi: `thinking: { type: "enabled", effort: "..." }`.
	reasoningWireKimi
	// reasoningWireDeepSeek: `thinking: { type: "enabled" }` + a top-level
	// `reasoning_effort` on DeepSeek's `high`/`max` scale.
	reasoningWireDeepSeek
)

type structuredOutputWire int

const (
	structuredOutputWireJSONSchema structuredOutputWire = iota
	structuredOutputWireJSONObject
)

type metadataWire int

const (
	metadataWireStandard metadataWire = iota
	metadataWireOpenRouter
	metadataWireDeepSeek
	metadataWireKimi
)

// Model discovery should fail fast: chat requests can legitimately run for
// minutes, setup refreshes should not inherit that wall-clock.
const modelDiscoveryTimeout = 12 * time.Second

// OpenAiClient is the OpenAI-compatible chat-completions client.
type OpenAiClient struct {
	baseURL string
	auth    BearerTokenProvider
	http    *http.Client
	headers http.Header
	// reasoningWire is how this client forwards reasoning effort.
	reasoningWire        reasoningWire
	structuredOutputWire structuredOutputWire
	metadataWire         metadataWire
}

func NewOpenAiClient(baseURL string, apiKey *string) *OpenAiClient {
	return NewOpenAiClientWithDefaultHeaders(baseURL, apiKey, http.Header{})
}

// NewOpenAiClientWithDefaultHeaders is NewOpenAiClient attaching
// defaultHeaders to every request.
func NewOpenAiClientWithDefaultHeaders(baseURL string, apiKey *string, defaultHeaders http.Header) *OpenAiClient {
	return NewOpenAiClientWithAuthProvider(baseURL, staticBearerToken{token: apiKey}, defaultHeaders)
}

func NewOpenAiClientWithAuthProvider(baseURL string, auth BearerTokenProvider, defaultHeaders http.Header) *OpenAiClient {
	baseURL = strings.TrimRight(baseURL, "/")
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = (&net.Dialer{Timeout: 10 * time.Second}).DialContext
	if strings.Contains(baseURL, "openrouter.ai") {
		// OpenRouter: HTTP/1.1 only and no idle pool, as Anvil configures it.
		transport.ForceAttemptHTTP2 = false
		transport.TLSNextProto = map[string]func(string, *tls.Conn) http.RoundTripper{}
		transport.DisableKeepAlives = true
	}
	return &OpenAiClient{
		baseURL: baseURL,
		auth:    auth,
		http:    &http.Client{Transport: transport, Timeout: 600 * time.Second},
		headers: defaultHeaders.Clone(),
		// Default to OpenAI's strict JSON Schema response_format for
		// OpenAI-compatible servers. Known exceptions override the dialect
		// in their constructor; hosted DeepSeek supports JSON mode only.
		structuredOutputWire: structuredOutputWireJSONSchema,
	}
}

// NewOpenAiClientWithReasoningSupport forwards reasoning effort via the
// OpenRouter/Ollama unified `reasoning: { effort }` object.
func NewOpenAiClientWithReasoningSupport(baseURL string, apiKey *string, defaultHeaders http.Header) *OpenAiClient {
	client := NewOpenAiClientWithDefaultHeaders(baseURL, apiKey, defaultHeaders)
	client.reasoningWire = reasoningWireUnified
	return client
}

// NewOpenAiClientWithKimiSupport is the Kimi Code client: Kimi's thinking
// dialect and model metadata. Kimi's tool-schema projection is not ported:
// Mjolnir never offers tools.
func NewOpenAiClientWithKimiSupport(baseURL string, auth BearerTokenProvider, defaultHeaders http.Header) *OpenAiClient {
	client := NewOpenAiClientWithAuthProvider(baseURL, auth, defaultHeaders)
	client.reasoningWire = reasoningWireKimi
	client.metadataWire = metadataWireKimi
	return client
}

// NewOpenAiClientWithDeepseekReasoningSupport constructs an OpenAiClient for
// the hosted DeepSeek API: it advertises DeepSeek's `high`/`max` reasoning
// levels and forwards effort in DeepSeek's dialect (`thinking` + a
// top-level `reasoning_effort`).
func NewOpenAiClientWithDeepseekReasoningSupport(baseURL string, apiKey *string, defaultHeaders http.Header) *OpenAiClient {
	client := NewOpenAiClientWithDefaultHeaders(baseURL, apiKey, defaultHeaders)
	client.reasoningWire = reasoningWireDeepSeek
	client.structuredOutputWire = structuredOutputWireJSONObject
	client.metadataWire = metadataWireDeepSeek
	return client
}

func (c *OpenAiClient) apiURL(path string) string {
	if strings.HasSuffix(c.baseURL, "/v1") {
		return c.baseURL + path
	}
	return c.baseURL + "/v1" + path
}

// newRequest builds a request carrying the client's default headers and,
// when there is one, the bearer token.
func (c *OpenAiClient) newRequest(ctx context.Context, method, url string, body []byte, token *string) (*http.Request, error) {
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	request, err := http.NewRequestWithContext(ctx, method, url, reader)
	if err != nil {
		return nil, err
	}
	for name, values := range c.headers {
		request.Header[name] = append([]string{}, values...)
	}
	if body != nil {
		request.Header.Set("Content-Type", "application/json")
	}
	if token != nil {
		request.Header.Set("Authorization", "Bearer "+*token)
	}
	return request, nil
}

// SupportsNativeStructuredOutput is the trait default: the schema is also
// sent in-band.
func (c *OpenAiClient) SupportsNativeStructuredOutput() bool { return false }

// modelEntry is one `/models` catalog entry.
type modelEntry struct {
	ID                   string             `json:"id"`
	SupportedParameters  []string           `json:"supported_parameters"`
	DefaultParameters    json.RawMessage    `json:"default_parameters"`
	Architecture         *modelArchitecture `json:"architecture"`
	ContextLength        *uint32            `json:"context_length"`
	Pricing              *modelPricingEntry `json:"pricing"`
	SupportsImageIn      *bool              `json:"supports_image_in"`
	SupportsReasoningRaw bool               `json:"supports_reasoning"`
	SupportsThinkingType *string            `json:"supports_thinking_type"`
	ThinkEfforts         *kimiThinkEfforts  `json:"think_efforts"`
}

func (e *modelEntry) UnmarshalJSON(data []byte) error {
	type plain modelEntry
	var value plain
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	if _, ok := fields["id"]; !ok {
		return errors.New("missing field `id`")
	}
	*e = modelEntry(value)
	return nil
}

type kimiThinkEfforts struct {
	Support       bool     `json:"support"`
	ValidEfforts  []string `json:"valid_efforts"`
	DefaultEffort *string  `json:"default_effort"`
}

type modelArchitecture struct {
	InputModalities *[]string `json:"input_modalities"`
}

type modelsResponse struct {
	Data []modelEntry `json:"data"`
}

type modelPricingEntry struct {
	Prompt     string `json:"prompt"`
	Completion string `json:"completion"`
}

func presets(pairs [][2]string) []ReasoningLevelPreset {
	levels := make([]ReasoningLevelPreset, 0, len(pairs))
	for _, pair := range pairs {
		levels = append(levels, ReasoningLevelPreset{Effort: pair[0], Description: pair[1]})
	}
	return levels
}

var openrouterReasoningPresets = [][2]string{
	{"minimal", "Basic reasoning with minimal computational effort"},
	{"low", "Light reasoning for simple problems"},
	{"medium", "Balanced reasoning for moderate complexity"},
	{"high", "Deep reasoning for complex problems"},
	{"xhigh", "Extra-high reasoning for the most difficult problems"},
}

// deepseekReasoningPresets are DeepSeek's two documented reasoning levels.
// DeepSeek's `/v1/models` is id-only, so the levels are declared here.
var deepseekReasoningPresets = [][2]string{
	{"high", "Deep reasoning (DeepSeek default)."},
	{"max", "Maximum reasoning for the hardest problems."},
}

// deepseekReasoningEffort clamps an effort string to a value DeepSeek
// accepts: `xhigh`/`max` -> `max`; everything else floors to `high`.
func deepseekReasoningEffort(effort string) string {
	switch asciiLower(strings.TrimSpace(effort)) {
	case "max", "xhigh":
		return "max"
	}
	return "high"
}

func reasoningEffortFromDefaultParameters(defaultParameters json.RawMessage) *string {
	if len(defaultParameters) == 0 {
		return nil
	}
	var parameters struct {
		Reasoning *struct {
			Effort  json.RawMessage `json:"effort"`
			Enabled json.RawMessage `json:"enabled"`
		} `json:"reasoning"`
	}
	if json.Unmarshal(defaultParameters, &parameters) != nil || parameters.Reasoning == nil {
		return nil
	}
	var effort string
	if json.Unmarshal(parameters.Reasoning.Effort, &effort) == nil {
		return &effort
	}
	var enabled bool
	if json.Unmarshal(parameters.Reasoning.Enabled, &enabled) == nil && enabled {
		medium := "medium"
		return &medium
	}
	return nil
}

func (e *modelEntry) pricing() *ModelPricing {
	if e.Pricing == nil {
		return nil
	}
	input, err := strconv.ParseFloat(e.Pricing.Prompt, 64)
	if err != nil {
		return nil
	}
	output, err := strconv.ParseFloat(e.Pricing.Completion, 64)
	if err != nil {
		return nil
	}
	return &ModelPricing{InputCostPerTokenUsd: input, OutputCostPerTokenUsd: output}
}

func (e *modelEntry) supportsImages() *bool {
	if e.Architecture == nil || e.Architecture.InputModalities == nil {
		return nil
	}
	supported := false
	for _, modality := range *e.Architecture.InputModalities {
		if asciiLower(modality) == "image" {
			supported = true
		}
	}
	return &supported
}

func (e *modelEntry) supportsReasoning() bool {
	for _, parameter := range e.SupportedParameters {
		if parameter == "reasoning" || parameter == "include_reasoning" {
			return true
		}
	}
	if len(e.DefaultParameters) == 0 {
		return false
	}
	var parameters map[string]json.RawMessage
	if json.Unmarshal(e.DefaultParameters, &parameters) != nil {
		return false
	}
	_, ok := parameters["reasoning"]
	return ok
}

func (e *modelEntry) toModelMetadata() ModelMetadata {
	metadata := ModelMetadata{
		ID:                       e.ID,
		SupportedReasoningLevels: []ReasoningLevelPreset{},
		ServiceTiers:             []ModelServiceTier{},
		SupportsImages:           e.supportsImages(),
		ContextLength:            e.ContextLength,
		Pricing:                  e.pricing(),
	}
	if !e.supportsReasoning() {
		return metadata
	}
	metadata.DefaultReasoningLevel = reasoningEffortFromDefaultParameters(e.DefaultParameters)
	if metadata.DefaultReasoningLevel == nil {
		medium := "medium"
		metadata.DefaultReasoningLevel = &medium
	}
	metadata.SupportedReasoningLevels = presets(openrouterReasoningPresets)
	return metadata
}

func (e *modelEntry) toKimiModelMetadata() ModelMetadata {
	var efforts *kimiThinkEfforts
	if e.ThinkEfforts != nil && e.ThinkEfforts.Support {
		efforts = e.ThinkEfforts
	}
	levels := []ReasoningLevelPreset{}
	if efforts != nil {
		for _, effort := range efforts.ValidEfforts {
			levels = append(levels, ReasoningLevelPreset{Effort: effort, Description: "Kimi thinking effort: " + effort})
		}
	}
	supportsReasoning := e.SupportsReasoningRaw ||
		e.SupportsThinkingType != nil && *e.SupportsThinkingType != "no" ||
		efforts != nil
	var defaultLevel *string
	if supportsReasoning && efforts != nil && efforts.DefaultEffort != nil {
		effort := *efforts.DefaultEffort
		defaultLevel = &effort
	}
	supportsImages := e.SupportsImageIn
	if supportsImages == nil {
		supportsImages = e.supportsImages()
	}
	return ModelMetadata{
		ID:                       e.ID,
		DefaultReasoningLevel:    defaultLevel,
		SupportedReasoningLevels: levels,
		ServiceTiers:             []ModelServiceTier{},
		SupportsImages:           supportsImages,
		ContextLength:            e.ContextLength,
		Pricing:                  e.pricing(),
	}
}

func (c *OpenAiClient) fetchModelsResponse(ctx context.Context) (*modelsResponse, error) {
	url := c.apiURL("/models")
	apiKey, err := c.auth.BearerToken(ctx)
	if err != nil {
		return nil, err
	}
	requestCtx, cancel := context.WithTimeout(ctx, modelDiscoveryTimeout)
	defer cancel()
	send := func(token *string) (*http.Response, error) {
		request, err := c.newRequest(requestCtx, http.MethodGet, url, nil, token)
		if err != nil {
			return nil, err
		}
		return c.http.Do(request)
	}
	response, err := send(apiKey)
	if err != nil {
		return nil, withContext(err, "GET "+url)
	}
	defer func() { response.Body.Close() }()
	if response.StatusCode == http.StatusUnauthorized && apiKey != nil {
		if rejecting, ok := c.auth.(RejectedBearerTokenProvider); ok {
			replacement, err := rejecting.RejectedBearerToken(ctx, *apiKey)
			if err != nil {
				return nil, err
			}
			if replacement != nil && *replacement != *apiKey {
				response.Body.Close()
				response, err = send(replacement)
				if err != nil {
					return nil, withContext(err, "retry model discovery with changed credential")
				}
			}
		}
	}
	if response.StatusCode < 200 || response.StatusCode > 299 {
		body, _ := io.ReadAll(response.Body)
		status := httpStatusText(response.StatusCode)
		slog.Warn(fmt.Sprintf("model discovery failed (HTTP %s): %s", status, body))
		return nil, fmt.Errorf("model discovery failed (HTTP %s)", status)
	}
	var parsed modelsResponse
	if err := json.NewDecoder(response.Body).Decode(&parsed); err != nil {
		return nil, withContext(err, "failed to parse models response")
	}
	return &parsed, nil
}

func (c *OpenAiClient) ListModels(ctx context.Context) ([]string, error) {
	models, err := c.fetchModelsResponse(ctx)
	if err != nil {
		return nil, err
	}
	ids := make([]string, 0, len(models.Data))
	for _, model := range models.Data {
		ids = append(ids, model.ID)
	}
	return ids, nil
}

func (c *OpenAiClient) ListModelMetadata(ctx context.Context) ([]ModelMetadata, error) {
	models, err := c.fetchModelsResponse(ctx)
	if err != nil {
		return nil, err
	}
	metadata := make([]ModelMetadata, 0, len(models.Data))
	for index := range models.Data {
		model := &models.Data[index]
		switch c.metadataWire {
		case metadataWireDeepSeek:
			// DeepSeek's `/models` is id-only; declare its levels (default
			// `high`) so the shared selection code can map requests to
			// availability.
			high := "high"
			metadata = append(metadata, ModelMetadata{
				ID:                       model.ID,
				DefaultReasoningLevel:    &high,
				SupportedReasoningLevels: presets(deepseekReasoningPresets),
				ServiceTiers:             []ModelServiceTier{},
				SupportsImages:           model.supportsImages(),
				ContextLength:            model.ContextLength,
				Pricing:                  model.pricing(),
			})
		case metadataWireKimi:
			metadata = append(metadata, model.toKimiModelMetadata())
		default:
			metadata = append(metadata, model.toModelMetadata())
		}
	}
	return metadata, nil
}

// chatCompletionMessageValue is a message as a chat-completions request
// carries it: a single text part is sent as a plain string.
func chatCompletionMessageValue(message *ChatMessage) (json.RawMessage, error) {
	object := jsonvalue.Object{{Key: "role", Value: message.Role}}
	if len(message.Content) != 0 {
		if text, ok := message.Content[0].(ChatContentPartText); ok && len(message.Content) == 1 {
			object = append(object, jsonvalue.Member{Key: "content", Value: text.Text})
		} else {
			parts, err := chatContentPartsValue(message.Content)
			if err != nil {
				return nil, err
			}
			object = append(object, jsonvalue.Member{Key: "content", Value: parts})
		}
	}
	return chatMessageTail(object, message)
}

type streamOptions struct {
	IncludeUsage bool `json:"include_usage"`
}

type reasoningConfig struct {
	Effort string `json:"effort"`
}

// thinkingConfig is DeepSeek's and Kimi's `thinking` object.
type thinkingConfig struct {
	Type   string  `json:"type"`
	Effort *string `json:"effort,omitempty"`
}

type nativeJSONSchemaFormat struct {
	Name   string          `json:"name"`
	Schema json.RawMessage `json:"schema"`
	Strict bool            `json:"strict"`
}

type chatCompletionResponseFormat struct {
	Type       string                  `json:"type"`
	JSONSchema *nativeJSONSchemaFormat `json:"json_schema,omitempty"`
}

type chatCompletionRequest struct {
	Model    string            `json:"model"`
	Messages []json.RawMessage `json:"messages"`
	Stream   bool              `json:"stream"`
	// StreamOptions opts in to a final `usage` payload on a streamed
	// response.
	StreamOptions   *streamOptions                `json:"stream_options,omitempty"`
	Temperature     *float64                      `json:"temperature,omitempty"`
	MaxTokens       *uint32                       `json:"max_tokens,omitempty"`
	Tools           []ToolDefinition              `json:"tools,omitempty"`
	ToolChoice      *string                       `json:"tool_choice,omitempty"`
	Reasoning       *reasoningConfig              `json:"reasoning,omitempty"`
	Thinking        *thinkingConfig               `json:"thinking,omitempty"`
	ReasoningEffort *string                       `json:"reasoning_effort,omitempty"`
	ResponseFormat  *chatCompletionResponseFormat `json:"response_format,omitempty"`
}

// marshalSerdeJSON encodes as serde_json does: `<`, `>` and `&` stay
// literal and there is no trailing newline.
func marshalSerdeJSON(value any) ([]byte, error) {
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(value); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(buffer.Bytes(), []byte("\n")), nil
}

func (c *OpenAiClient) StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error) {
	if c.reasoningWire == reasoningWireOff {
		request.ReasoningEffort = nil
	}
	url := c.apiURL("/chat/completions")
	messages := make([]json.RawMessage, 0, len(request.Messages))
	for index := range request.Messages {
		encoded, err := chatCompletionMessageValue(&request.Messages[index])
		if err != nil {
			return nil, err
		}
		messages = append(messages, encoded)
	}
	body := chatCompletionRequest{
		Model:         request.Model,
		Messages:      messages,
		Stream:        true,
		StreamOptions: &streamOptions{IncludeUsage: true},
		Temperature:   request.Temperature,
		Tools:         request.Tools,
	}
	if request.Tools != nil {
		auto := "auto"
		body.ToolChoice = &auto
	}
	// Spell reasoning effort in this client's dialect.
	if effort := request.ReasoningEffort; effort != nil {
		switch c.reasoningWire {
		case reasoningWireUnified:
			body.Reasoning = &reasoningConfig{Effort: *effort}
		case reasoningWireKimi:
			value := *effort
			body.Thinking = &thinkingConfig{Type: "enabled", Effort: &value}
		case reasoningWireDeepSeek:
			body.Thinking = &thinkingConfig{Type: "enabled"}
			clamped := deepseekReasoningEffort(*effort)
			body.ReasoningEffort = &clamped
		}
	}
	if structured := request.StructuredOutput; structured != nil {
		// A request can opt down to basic JSON mode regardless of the
		// client's default wire.
		wire := c.structuredOutputWire
		if structured.PreferJSONObject {
			wire = structuredOutputWireJSONObject
		}
		if wire == structuredOutputWireJSONSchema {
			format := nativeResponseFormat(structured)
			schema, err := jsonvalue.Marshal(format.Schema)
			if err != nil {
				return nil, err
			}
			body.ResponseFormat = &chatCompletionResponseFormat{Type: "json_schema", JSONSchema: &nativeJSONSchemaFormat{
				Name:   format.Name,
				Schema: schema,
				Strict: format.Strict,
			}}
		} else {
			body.ResponseFormat = &chatCompletionResponseFormat{Type: "json_object"}
		}
	}
	encoded, err := marshalSerdeJSON(body)
	if err != nil {
		return nil, err
	}
	apiKey, err := c.auth.BearerToken(ctx)
	if err != nil {
		return nil, err
	}
	response, err := sendWithRetries(ctx, "sending chat completion request", c.http, func(attemptCtx context.Context) (*http.Request, error) {
		return c.newRequest(attemptCtx, http.MethodPost, url, encoded, apiKey)
	}, request.IdleTimeouts.FirstProgress)
	if err != nil {
		return nil, err
	}
	defer func() { response.Body.Close() }()
	if response.StatusCode == http.StatusUnauthorized && apiKey != nil {
		if rejecting, ok := c.auth.(RejectedBearerTokenProvider); ok {
			replacement, err := rejecting.RejectedBearerToken(ctx, *apiKey)
			if err != nil {
				return nil, err
			}
			if replacement != nil && *replacement != *apiKey {
				// Exactly one attempt, only after an explicit rejection.
				// Neither a transport failure nor a lost acknowledgement
				// authorizes replay. reqwest's per-request timeout covers
				// the whole retried exchange.
				response.Body.Close()
				retryCtx, cancel := context.WithTimeout(ctx, request.IdleTimeouts.FirstProgress)
				defer cancel()
				retry, err := c.newRequest(retryCtx, http.MethodPost, url, encoded, replacement)
				if err != nil {
					return nil, err
				}
				response, err = c.http.Do(retry)
				if err != nil {
					if ctx.Err() != nil {
						return nil, errors.New("chat completion cancelled")
					}
					return nil, withContext(err, "retry chat completion with changed credential")
				}
			}
		}
	}
	if response.StatusCode < 200 || response.StatusCode > 299 {
		bodyText, _ := io.ReadAll(response.Body)
		return nil, retryableLlmErrorForBody(
			fmt.Sprintf("chat completion failed (HTTP %s): %s", httpStatusText(response.StatusCode), bodyText),
			string(bodyText))
	}
	return driveSSEStream(ctx, response.Body, request.OnToken, request.OnThought, request.IdleTimeouts)
}

// chatCompletionChunk is one streamed chat-completions event.
type chatCompletionChunk struct {
	Choices []chunkChoice `json:"choices"`
	// Usage: per OpenAI's `stream_options.include_usage`, the final SSE
	// chunk before `[DONE]` carries a populated usage block with empty
	// choices.
	Usage *usageChunk `json:"usage"`
}

// usageChunk is the OpenAI / OpenRouter / Ollama usage block.
type usageChunk struct {
	PromptTokens        uint64 `json:"prompt_tokens"`
	CompletionTokens    uint64 `json:"completion_tokens"`
	PromptTokensDetails *struct {
		CachedTokens uint64 `json:"cached_tokens"`
	} `json:"prompt_tokens_details"`
	CompletionTokensDetails *struct {
		ReasoningTokens uint64 `json:"reasoning_tokens"`
	} `json:"completion_tokens_details"`
}

// toUsage projects OpenAI's wire fields onto TokenUsage: prompt tokens
// include cached ones and completion tokens include reasoning, so both are
// subtracted to keep the totals from double-counting.
func (u *usageChunk) toUsage() TokenUsage {
	var cachedRead, reasoning uint64
	if u.PromptTokensDetails != nil {
		cachedRead = u.PromptTokensDetails.CachedTokens
	}
	if u.CompletionTokensDetails != nil {
		reasoning = u.CompletionTokensDetails.ReasoningTokens
	}
	sub := func(left, right uint64) uint64 {
		if right > left {
			return 0
		}
		return left - right
	}
	return TokenUsage{
		InputTokens:      sub(u.PromptTokens, cachedRead),
		OutputTokens:     sub(u.CompletionTokens, reasoning),
		ThoughtTokens:    reasoning,
		CachedReadTokens: cachedRead,
	}
}

type chunkChoice struct {
	Delta        *chunkDelta `json:"delta"`
	FinishReason *string     `json:"finish_reason"`
	// Usage: Kimi Code attaches usage to the final choice rather than
	// emitting a separate empty-choices usage chunk.
	Usage *usageChunk `json:"usage"`
}

type chunkDelta struct {
	Content          *string         `json:"content"`
	ReasoningContent *string         `json:"reasoning_content"`
	Reasoning        *string         `json:"reasoning"`
	ToolCalls        []chunkToolCall `json:"tool_calls"`
}

type chunkToolCall struct {
	Index    *uint64 `json:"index"`
	ID       *string `json:"id"`
	Function *struct {
		Name      *string `json:"name"`
		Arguments *string `json:"arguments"`
	} `json:"function"`
}

// parseChatCompletionChunk decodes one event, requiring each choice's
// delta and each tool call's index as serde does.
func parseChatCompletionChunk(data string) (*chatCompletionChunk, error) {
	var chunk chatCompletionChunk
	if err := json.Unmarshal([]byte(data), &chunk); err != nil {
		return nil, err
	}
	for _, choice := range chunk.Choices {
		if choice.Delta == nil {
			return nil, errors.New("missing field `delta`")
		}
		for _, call := range choice.Delta.ToolCalls {
			if call.Index == nil {
				return nil, errors.New("missing field `index`")
			}
		}
	}
	return &chunk, nil
}

// toolCallAccumulator assembles tool calls arriving as SSE fragments.
type toolCallAccumulator struct {
	calls map[uint64]*[3]string // index -> (id, name, arguments)
}

func (a *toolCallAccumulator) push(chunk *chunkToolCall) {
	if a.calls == nil {
		a.calls = map[uint64]*[3]string{}
	}
	entry, ok := a.calls[*chunk.Index]
	if !ok {
		entry = &[3]string{}
		a.calls[*chunk.Index] = entry
	}
	if chunk.ID != nil {
		entry[0] = *chunk.ID
	}
	if chunk.Function != nil {
		if chunk.Function.Name != nil {
			entry[1] = *chunk.Function.Name
		}
		if chunk.Function.Arguments != nil {
			entry[2] += *chunk.Function.Arguments
		}
	}
}

func (a *toolCallAccumulator) intoToolCalls() []ToolCall {
	indexes := make([]uint64, 0, len(a.calls))
	for index := range a.calls {
		indexes = append(indexes, index)
	}
	slices.Sort(indexes)
	calls := make([]ToolCall, 0, len(indexes))
	for _, index := range indexes {
		entry := a.calls[index]
		calls = append(calls, ToolCall{ID: entry[0], Type: "function", Function: FunctionCall{Name: entry[1], Arguments: entry[2]}})
	}
	return calls
}

func normalizeStreamToolCalls(calls []ToolCall, protocol string) ([]ToolCall, error) {
	normalized := make([]ToolCall, 0, len(calls))
	for _, call := range calls {
		arguments, err := normalizeStreamedToolArguments(call.ID, call.Function.Name, call.Function.Arguments, protocol)
		if err != nil {
			return nil, err
		}
		call.Function.Arguments = arguments
		normalized = append(normalized, call)
	}
	return normalized, nil
}

// streamRead is one read of a response body: bytes, or the read's error
// (io.EOF at the end).
type streamRead struct {
	data []byte
	err  error
}

// readStream reads body on its own goroutine until it fails or done is
// closed. The caller closes body to stop a read that is blocked.
func readStream(body io.Reader, done <-chan struct{}) <-chan streamRead {
	reads := make(chan streamRead)
	go func() {
		buffer := make([]byte, 32*1024)
		for {
			count, err := body.Read(buffer)
			if count > 0 {
				select {
				case reads <- streamRead{data: append([]byte(nil), buffer[:count]...)}:
				case <-done:
					return
				}
			}
			if err != nil {
				select {
				case reads <- streamRead{err: err}:
				case <-done:
				}
				return
			}
		}
	}()
	return reads
}

// driveSSEStream drives an SSE byte stream until the LLM emits `[DONE]` or
// ctx is cancelled. It aborts with a clear error if the stream ends before
// `[DONE]`, or if no meaningful progress (a parsed `data:` event
// contributing content or tool-call deltas, or `[DONE]`) is observed within
// the current first-progress or inter-chunk timeout. Keepalive comments,
// blank lines, and partial bytes that don't advance the parser do not
// reset the deadline.
func driveSSEStream(ctx context.Context, body io.ReadCloser, onToken, onThought TokenSink, idle IdleTimeouts) (LlmResponse, error) {
	done := make(chan struct{})
	defer close(done)
	defer body.Close()
	reads := readStream(body, done)
	var fullText, fullReasoning strings.Builder
	var tools toolCallAccumulator
	var rawBuffer []byte
	deadline := time.NewTimer(idle.FirstProgress)
	defer deadline.Stop()
	sawProgress := false
	usage := TokenUsage{}
	var lastFinishReason *string
	reasoningContent := func() *string {
		if fullReasoning.Len() == 0 {
			return nil
		}
		text := fullReasoning.String()
		return &text
	}

loop:
	for {
		select {
		case <-ctx.Done():
			slog.Info("streaming cancelled by client")
			break loop
		case <-deadline.C:
			if sawProgress {
				return nil, RetryableLlmErrorWith(
					fmt.Sprintf("LLM stream stalled mid-stream for %ds; aborting (server-side hang or keepalive-only flood)", int64(idle.InterChunk/time.Second)),
					RetryableLlmErrorFast("stream stalled mid-stream"))
			}
			return nil, RetryableLlmErrorWith(
				fmt.Sprintf("LLM stream made no first token for %ds; aborting (server-side hang or keepalive-only flood)", int64(idle.FirstProgress/time.Second)),
				RetryableLlmErrorFast("stream made no first token"))
		case read := <-reads:
			eofAfterBuffer := false
			switch {
			case read.err != nil && !errors.Is(read.err, io.EOF):
				return nil, retryableLlmContext(read.err, "stream read error", RetryableLlmErrorFast("stream read error"))
			case read.err == nil:
				rawBuffer = append(rawBuffer, read.data...)
			case len(rawBuffer) == 0:
				break loop
			default:
				rawBuffer = append(rawBuffer, '\n')
				eofAfterBuffer = true
			}

			madeProgress := false
			for {
				position := bytes.IndexByte(rawBuffer, '\n')
				if position < 0 {
					break
				}
				lineBytes := rawBuffer[:position+1]
				rawBuffer = rawBuffer[position+1:]
				line := strings.TrimSpace(strings.ToValidUTF8(string(lineBytes), "�"))
				if line == "" || strings.HasPrefix(line, ":") {
					continue
				}
				data, ok := strings.CutPrefix(line, "data: ")
				if !ok {
					continue
				}
				data = strings.TrimSpace(data)
				if data == "[DONE]" {
					outputBudgetExhausted := lastFinishReason != nil && *lastFinishReason == "length"
					if len(tools.calls) == 0 {
						if outputBudgetExhausted {
							if strings.TrimSpace(fullText.String()) == "" {
								return nil, OutputBudgetExhaustedError{}
							}
							slog.Warn("chat completions stream ended with finish_reason=length after emitting text; returning truncated content")
						}
						return LlmResponseText{Text: fullText.String(), ReasoningContent: reasoningContent(), Usage: usage}, nil
					}
					if outputBudgetExhausted {
						slog.Warn("chat completions stream ended with finish_reason=length after emitting tool calls; returning truncated content")
					}
					calls, err := normalizeStreamToolCalls(tools.intoToolCalls(), "chat completions SSE")
					if err != nil {
						return nil, err
					}
					return LlmResponseToolCalls{Text: fullText.String(), ReasoningContent: reasoningContent(), Calls: calls, Usage: usage}, nil
				}
				chunk, err := parseChatCompletionChunk(data)
				if err != nil {
					slog.Debug(fmt.Sprintf("skipping unparseable SSE chunk: %v", err))
					continue
				}
				for index := range chunk.Choices {
					choice := &chunk.Choices[index]
					if choice.FinishReason != nil {
						if reason := strings.TrimSpace(*choice.FinishReason); reason != "" {
							lastFinishReason = &reason
						}
					}
					if content := choice.Delta.Content; content != nil {
						madeProgress = true
						if onToken != nil {
							onToken(*content)
						}
						fullText.WriteString(*content)
					}
					reasoning := choice.Delta.ReasoningContent
					if reasoning == nil {
						reasoning = choice.Delta.Reasoning
					}
					if reasoning != nil {
						madeProgress = true
						if onThought != nil {
							onThought(*reasoning)
						}
						fullReasoning.WriteString(*reasoning)
					}
					if choice.Delta.ToolCalls != nil {
						if len(choice.Delta.ToolCalls) != 0 {
							madeProgress = true
						}
						for call := range choice.Delta.ToolCalls {
							tools.push(&choice.Delta.ToolCalls[call])
						}
					}
					if choice.Usage != nil {
						usage = choice.Usage.toUsage()
						madeProgress = true
					}
				}
				// Last chunk before [DONE]: the trailing usage block.
				if chunk.Usage != nil {
					usage = chunk.Usage.toUsage()
					madeProgress = true
				}
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

	// A cancelled stream's tool call fragments may be incomplete; return
	// only the text already streamed to the caller.
	if ctx.Err() != nil {
		return LlmResponseText{Text: fullText.String(), ReasoningContent: reasoningContent(), Usage: usage}, nil
	}
	return nil, NewIncompleteStreamError("chat completions SSE", "[DONE]")
}
