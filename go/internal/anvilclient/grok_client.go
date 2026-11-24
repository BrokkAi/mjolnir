// Port of brokk-anvil-client 0.28.6 src/grok_client.rs (the part Mjolnir uses).

package anvilclient

import (
	"context"
	"fmt"
	"io"
	"math"
	"net/http"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const grokAPIBaseURL = "https://cli-chat-proxy.grok.com/v1"

// GrokClient talks to the Grok CLI chat proxy with a Grok Build login.
type GrokClient struct {
	auth          *grokAuthManager
	http          *http.Client
	baseURL       string
	clientVersion string
}

type GrokClientConfig struct {
	AuthPath    string
	VersionPath *string
	BaseURL     string
}

// GrokClientConfigFromHome is the configuration of the Grok home home.
func GrokClientConfigFromHome(home string) GrokClientConfig {
	versionPath := filepath.Join(home, "version.json")
	return GrokClientConfig{
		AuthPath:    filepath.Join(home, "auth.json"),
		VersionPath: &versionPath,
		BaseURL:     grokAPIBaseURL,
	}
}

// GrokClientLoadWithConfig builds a client from config's login, or nil when
// there is no login.
func GrokClientLoadWithConfig(config GrokClientConfig) (LlmBackend, error) {
	auth, err := loadGrokAuthManager(config.AuthPath)
	if err != nil || auth == nil {
		return nil, err
	}
	return &GrokClient{
		auth:          auth,
		http:          &http.Client{},
		baseURL:       strings.TrimRight(config.BaseURL, "/"),
		clientVersion: grokClientVersionFromPath(config.VersionPath),
	}, nil
}

func (c *GrokClient) requestHeaders(request *http.Request, credential *grokCredential) {
	request.Header.Set("Authorization", "Bearer "+credential.accessToken)
	request.Header.Set("X-XAI-Token-Auth", "xai-grok-cli")
	request.Header.Set("x-authenticateresponse", "authenticate-response")
	request.Header.Set("x-grok-client-version", c.clientVersion)
	request.Header.Set("x-grok-client-identifier", "anvil")
	request.Header.Set("x-grok-client-mode", "headless")
	request.Header.Set("User-Agent", "anvil/"+anvilVersion)
	request.Header.Set("x-userid", credential.userID)
	if credential.email != nil && *credential.email != "" {
		request.Header.Set("x-email", *credential.email)
	}
}

func (c *GrokClient) modelsResponse(ctx context.Context) (*http.Response, error) {
	credential, err := c.auth.credential(ctx)
	if err != nil {
		return nil, err
	}
	send := func(credential *grokCredential, failure string) (*http.Response, error) {
		request, err := http.NewRequestWithContext(ctx, http.MethodGet, c.baseURL+"/models", nil)
		if err != nil {
			return nil, withContext(err, failure)
		}
		c.requestHeaders(request, credential)
		response, err := c.http.Do(request)
		if err != nil {
			return nil, withContext(err, failure)
		}
		return response, nil
	}
	response, err := send(&credential, "discovering Grok models")
	if err != nil || response.StatusCode != http.StatusUnauthorized {
		return response, err
	}
	response.Body.Close()
	refreshed, err := c.auth.refreshRejected(ctx, credential.accessToken)
	if err != nil {
		return nil, err
	}
	return send(&refreshed, "discovering Grok models after OAuth refresh")
}

func (c *GrokClient) discoverModels(ctx context.Context) ([]ModelMetadata, error) {
	response, err := c.modelsResponse(ctx)
	if err != nil {
		return nil, err
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		body, _ := io.ReadAll(response.Body)
		return nil, fmt.Errorf("Grok model discovery failed (HTTP %s): %s", httpStatusText(response.StatusCode), body)
	}
	raw, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, withContext(err, "parsing Grok model catalog")
	}
	payload, err := jsonvalue.Parse(raw)
	if err != nil {
		return nil, withContext(err, "parsing Grok model catalog")
	}
	if _, ok := payload.(jsonvalue.Object); !ok {
		return nil, withContext(fmt.Errorf("invalid type: expected struct ModelsResponse"), "parsing Grok model catalog")
	}
	var data []any
	if member, present := jsonMember(payload, "data"); present {
		list, ok := member.([]any)
		if !ok {
			return nil, withContext(fmt.Errorf("invalid type: expected a sequence"), "parsing Grok model catalog")
		}
		data = list
	}
	metadata := []ModelMetadata{}
	for _, value := range data {
		if model, ok := grokModelMetadata(value); ok {
			metadata = append(metadata, model)
		}
	}
	return metadata, nil
}

func (c *GrokClient) ListModels(ctx context.Context) ([]string, error) {
	metadata, err := c.discoverModels(ctx)
	if err != nil {
		return nil, err
	}
	ids := make([]string, 0, len(metadata))
	for _, model := range metadata {
		ids = append(ids, model.ID)
	}
	return ids, nil
}

func (c *GrokClient) ListModelMetadata(ctx context.Context) ([]ModelMetadata, error) {
	return c.discoverModels(ctx)
}

func (c *GrokClient) ResolveModelInfo(configuredModel string) ResolvedModelInfo {
	provider := "grok"
	return ResolvedModelInfo{ConfiguredModel: configuredModel, ResolvedProvider: &provider, ResolvedModel: configuredModel}
}

func (c *GrokClient) postResponses(ctx context.Context, body []byte, model string, credential *grokCredential, request *StreamChatRequest) (*http.Response, error) {
	conversationID := newUUIDv4()
	requestID := newUUIDv4()
	return sendWithRetries(ctx, "posting Grok Responses API request", c.http, func(attemptCtx context.Context) (*http.Request, error) {
		outgoing, err := http.NewRequestWithContext(attemptCtx, http.MethodPost, c.baseURL+"/responses", strings.NewReader(string(body)))
		if err != nil {
			return nil, err
		}
		outgoing.Header.Set("Accept", "text/event-stream")
		outgoing.Header.Set("Content-Type", "application/json")
		outgoing.Header.Set("x-grok-conv-id", conversationID)
		outgoing.Header.Set("x-grok-req-id", requestID)
		outgoing.Header.Set("x-grok-model-override", model)
		outgoing.Header.Set("x-grok-session-id", conversationID)
		outgoing.Header.Set("x-grok-agent-id", "anvil")
		c.requestHeaders(outgoing, credential)
		return outgoing, nil
	}, request.IdleTimeouts.FirstProgress)
}

func (c *GrokClient) StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error) {
	body, err := buildResponsesRequest(request.Model, request.Messages, request.Tools, request.ReasoningEffort,
		request.StructuredOutput, responsesRequestOptions{})
	if err != nil {
		return nil, err
	}
	concise := "concise"
	if body.Reasoning != nil {
		body.Reasoning.Summary = &concise
	} else {
		body.Reasoning = &responsesReasoningConfig{Summary: &concise}
	}
	body.Include = []string{"reasoning.encrypted_content"}
	encoded, err := marshalSerdeJSON(body)
	if err != nil {
		return nil, err
	}
	credential, err := c.auth.credential(ctx)
	if err != nil {
		return nil, err
	}
	response, err := c.postResponses(ctx, encoded, body.Model, &credential, &request)
	if err != nil {
		return nil, err
	}
	if response.StatusCode == http.StatusUnauthorized {
		response.Body.Close()
		refreshed, err := c.auth.refreshRejected(ctx, credential.accessToken)
		if err != nil {
			return nil, err
		}
		response, err = c.postResponses(ctx, encoded, body.Model, &refreshed, &request)
		if err != nil {
			return nil, err
		}
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		text, _ := io.ReadAll(response.Body)
		return nil, retryableLlmErrorForStatusAndBody(
			fmt.Sprintf("Grok Responses API failed (HTTP %s): %s", httpStatusText(response.StatusCode), text),
			response.StatusCode, string(text))
	}
	outcome, err := driveResponsesSSEStream(ctx, response.Body, request.OnToken, request.OnThought, request.IdleTimeouts)
	if err != nil {
		return nil, err
	}
	return outcome.response, nil
}

// grokModelMetadata reads one catalog entry, whose fields may sit at the
// top level or under `_meta` under several spellings.
func grokModelMetadata(value any) (ModelMetadata, bool) {
	meta, _ := jsonMember(value, "_meta")
	field := func(names ...string) any {
		for _, name := range names {
			if member, ok := jsonMember(value, name); ok {
				return member
			}
		}
		if meta == nil {
			return nil
		}
		for _, name := range names {
			if member, ok := jsonMember(meta, name); ok {
				return member
			}
		}
		return nil
	}
	var idValue any
	found := false
	for _, source := range []struct {
		value any
		key   string
	}{{value, "id"}, {value, "model"}, {value, "modelId"}, {meta, "model"}, {meta, "modelId"}} {
		if member, ok := jsonMember(source.value, source.key); ok {
			idValue, found = member, true
			break
		}
	}
	id, ok := idValue.(string)
	if !found || !ok {
		return ModelMetadata{}, false
	}
	var contextLength *uint32
	if length, ok := jsonU64(field("context_window", "context_length", "contextWindow", "totalContextTokens")); ok && length <= math.MaxUint32 {
		value := uint32(length)
		contextLength = &value
	}
	reasoning, _ := field("reasoning_efforts", "reasoningEfforts", "supported_reasoning_levels").([]any)
	var defaultLevel *string
	if level, ok := field("reasoning_effort", "reasoningEffort", "default_reasoning_effort", "default_reasoning_level").(string); ok {
		defaultLevel = &level
	}
	levels := []ReasoningLevelPreset{}
	for _, preset := range reasoning {
		effortValue, ok := jsonMember(preset, "value")
		if !ok {
			effortValue, _ = jsonMember(preset, "effort")
		}
		effort, ok := effortValue.(string)
		if !ok {
			continue
		}
		if jsonField(preset, "default") == true {
			level := effort
			defaultLevel = &level
		}
		description, _ := jsonField(preset, "description").(string)
		levels = append(levels, ReasoningLevelPreset{Effort: effort, Description: description})
	}
	var supportsImages *bool
	if supported, ok := field("supports_images", "supportsImages").(bool); ok {
		supportsImages = &supported
	} else if modalities, ok := field("input_modalities", "inputModalities").([]any); ok {
		image := false
		for _, modality := range modalities {
			if modality == "image" {
				image = true
			}
		}
		supportsImages = &image
	}
	return ModelMetadata{
		ID:                       id,
		DefaultReasoningLevel:    defaultLevel,
		SupportedReasoningLevels: levels,
		ServiceTiers:             []ModelServiceTier{},
		SupportsImages:           supportsImages,
		ContextLength:            contextLength,
	}, true
}

// jsonU64 is serde_json's `as_u64`.
func jsonU64(value any) (uint64, bool) {
	switch number := value.(type) {
	case int64:
		return uint64(number), number >= 0
	case uint64:
		return number, true
	}
	return 0, false
}
