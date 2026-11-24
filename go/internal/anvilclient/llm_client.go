// Port of brokk-anvil-client 0.28.6 src/llm_client.rs (the part Mjolnir uses).

package anvilclient

import (
	"context"
	"net/http"
	"time"
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

func (m ChatMessage) MarshalJSON() ([]byte, error) {
	panic("unported: brokk-anvil-client/src/llm_client.rs ChatMessage::serialize")
}

func (m *ChatMessage) UnmarshalJSON(data []byte) error {
	panic("unported: brokk-anvil-client/src/llm_client.rs ChatMessage::deserialize")
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

// OpenAiClient is the OpenAI-compatible chat-completions client.
type OpenAiClient struct {
	baseURL string
	auth    BearerTokenProvider
	http    *http.Client
	headers http.Header
}

// NewOpenAiClientWithDeepseekReasoningSupport constructs an OpenAiClient for
// the hosted DeepSeek API: it advertises DeepSeek's `high`/`max` reasoning
// levels and forwards effort in DeepSeek's dialect (`thinking` + a
// top-level `reasoning_effort`).
func NewOpenAiClientWithDeepseekReasoningSupport(baseURL string, apiKey *string, defaultHeaders http.Header) *OpenAiClient {
	panic("unported: brokk-anvil-client/src/llm_client.rs OpenAiClient::with_deepseek_reasoning_support")
}

func (c *OpenAiClient) ListModels(ctx context.Context) ([]string, error) {
	panic("unported: brokk-anvil-client/src/llm_client.rs OpenAiClient::list_models")
}

func (c *OpenAiClient) ListModelMetadata(ctx context.Context) ([]ModelMetadata, error) {
	panic("unported: brokk-anvil-client/src/llm_client.rs OpenAiClient::list_model_metadata")
}

func (c *OpenAiClient) SupportsNativeStructuredOutput() bool {
	panic("unported: brokk-anvil-client/src/llm_client.rs OpenAiClient::supports_native_structured_output")
}

func (c *OpenAiClient) StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error) {
	panic("unported: brokk-anvil-client/src/llm_client.rs OpenAiClient::stream_chat")
}
