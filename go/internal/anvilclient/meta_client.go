// Port of brokk-anvil-client 0.28.6 src/meta_client.rs (the part Mjolnir uses).

package anvilclient

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

const (
	metaAPIBaseURL     = "https://api.meta.ai/v1"
	metaMintBaseURL    = "https://api.meta.ai"
	museModelPrefix    = "muse-spark-"
	metaAuthReadLimit  = 1024 * 1024
	metaAuthReadTime   = 3 * time.Second
	metaMintTimeout    = 20 * time.Second
	metaErrorBodyLimit = 64 * 1024
)

// MetaClientConfig is the configuration for a native Muse Code profile.
type MetaClientConfig struct {
	AuthPath    string
	BaseURL     string
	MintBaseURL string
}

// MetaClientConfigFromHome builds a profile-local configuration from a Muse
// Code home directory.
func MetaClientConfigFromHome(home string) MetaClientConfig {
	return MetaClientConfig{
		AuthPath:    filepath.Join(home, "auth.json"),
		BaseURL:     metaAPIBaseURL,
		MintBaseURL: metaMintBaseURL,
	}
}

// cachedAPIKey is a minted API key and the access token it was minted from.
type cachedAPIKey struct {
	accessToken string
	apiKey      string
}

// MetaClient talks to Meta's API with a Muse Code login, minting an API key
// from its access token.
type MetaClient struct {
	authPath string
	// mintMu guards cached; it is held across the cache check and the mint
	// request, never across inference I/O.
	mintMu      sync.Mutex
	cached      *cachedAPIKey
	http        *http.Client
	baseURL     string
	mintBaseURL string
}

// MetaClientLoadWithConfig builds a client from config's login, or nil when
// the login file is missing or holds no access token.
func MetaClientLoadWithConfig(config MetaClientConfig) (LlmBackend, error) {
	if info, err := os.Stat(config.AuthPath); err != nil || !info.Mode().IsRegular() {
		return nil, nil
	}
	hasToken, err := metaAuthFileHasAccessToken(config.AuthPath)
	if err != nil || !hasToken {
		return nil, err
	}
	transport := http.DefaultTransport.(*http.Transport).Clone()
	transport.DialContext = (&net.Dialer{Timeout: metaMintTimeout}).DialContext
	return &MetaClient{
		authPath: config.AuthPath,
		http: &http.Client{
			Transport: transport,
			CheckRedirect: func(*http.Request, []*http.Request) error {
				return http.ErrUseLastResponse
			},
		},
		baseURL:     strings.TrimRight(config.BaseURL, "/"),
		mintBaseURL: strings.TrimRight(config.MintBaseURL, "/"),
	}, nil
}

func (c *MetaClient) mintAPIKey(ctx context.Context, accessToken string) (string, error) {
	mintCtx, cancel := context.WithTimeout(ctx, metaMintTimeout)
	defer cancel()
	request, err := http.NewRequestWithContext(mintCtx, http.MethodPost, c.mintBaseURL+"/muse-code/key", strings.NewReader(`{"onboard":false}`))
	if err != nil {
		return "", withContext(err, "minting Meta API key")
	}
	request.Header.Set("Authorization", "Bearer "+accessToken)
	request.Header.Set("Content-Type", "application/json")
	response, err := c.http.Do(request)
	if err != nil {
		if mintCtx.Err() == context.DeadlineExceeded {
			return "", errors.New("timed out minting Meta API key")
		}
		return "", withContext(err, "minting Meta API key")
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		// Do not include the response body: Meta may echo request or
		// account details, and this path must never expose credentials.
		body := readMetaErrorBody(response)
		return "", retryableLlmErrorForStatusAndBody(
			fmt.Sprintf("Meta API-key minting failed (HTTP %s)", httpStatusText(response.StatusCode)),
			response.StatusCode, body)
	}
	raw, err := io.ReadAll(response.Body)
	if err != nil {
		return "", withContext(err, "parsing Meta API-key response")
	}
	payload, err := jsonvalue.Parse(raw)
	if err != nil {
		return "", withContext(err, "parsing Meta API-key response")
	}
	key, _ := jsonField(payload, "api_key").(string)
	if strings.TrimSpace(key) == "" {
		return "", errors.New("Meta API-key response did not contain api_key")
	}
	return key, nil
}

// apiKey resolves the current API key, minting one when the login's
// access token changed.
func (c *MetaClient) apiKey(ctx context.Context) (string, error) {
	c.mintMu.Lock()
	defer c.mintMu.Unlock()
	accessToken, err := readMetaAccessTokenBounded(c.authPath)
	if err != nil {
		return "", err
	}
	if c.cached != nil && c.cached.accessToken == accessToken {
		return c.cached.apiKey, nil
	}
	apiKey, err := c.mintAPIKey(ctx, accessToken)
	if err != nil {
		return "", err
	}
	c.cached = &cachedAPIKey{accessToken: accessToken, apiKey: apiKey}
	return apiKey, nil
}

func (c *MetaClient) apiKeyFor(ctx context.Context) (string, error) {
	cancelled := errors.New("Meta Responses request was cancelled while acquiring an API key")
	if ctx.Err() != nil {
		return "", cancelled
	}
	key, err := c.apiKey(ctx)
	if err != nil && ctx.Err() != nil {
		return "", cancelled
	}
	return key, err
}

// invalidateRejectedKey evicts a key only if it is still the key the
// server rejected: a concurrent request may already have minted a newer
// one.
func (c *MetaClient) invalidateRejectedKey(rejectedKey string) {
	c.mintMu.Lock()
	defer c.mintMu.Unlock()
	if c.cached != nil && c.cached.apiKey == rejectedKey {
		c.cached = nil
	}
}

func (c *MetaClient) modelsResponse(ctx context.Context) (*http.Response, func(), error) {
	send := func(apiKey, timeoutMessage, failure string) (*http.Response, func(), error) {
		requestCtx, cancel := context.WithTimeout(ctx, metaMintTimeout)
		request, err := http.NewRequestWithContext(requestCtx, http.MethodGet, c.baseURL+"/models", nil)
		if err != nil {
			cancel()
			return nil, nil, withContext(err, failure)
		}
		request.Header.Set("Authorization", "Bearer "+apiKey)
		response, err := c.http.Do(request)
		if err != nil {
			cancel()
			if requestCtx.Err() == context.DeadlineExceeded {
				return nil, nil, errors.New(timeoutMessage)
			}
			return nil, nil, withContext(err, failure)
		}
		return response, cancel, nil
	}
	apiKey, err := c.apiKey(ctx)
	if err != nil {
		return nil, nil, err
	}
	response, cancel, err := send(apiKey, "timed out discovering Meta models", "discovering Meta models")
	if err != nil || response.StatusCode != http.StatusUnauthorized {
		return response, cancel, err
	}
	response.Body.Close()
	cancel()
	c.invalidateRejectedKey(apiKey)
	apiKey, err = c.apiKey(ctx)
	if err != nil {
		return nil, nil, err
	}
	return send(apiKey, "timed out discovering Meta models after API-key renewal", "discovering Meta models after API-key renewal")
}

func (c *MetaClient) discoverModels(ctx context.Context) ([]ModelMetadata, error) {
	response, cancel, err := c.modelsResponse(ctx)
	if err != nil {
		return nil, err
	}
	defer cancel()
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		body := readMetaErrorBody(response)
		return nil, retryableLlmErrorForStatusAndBody(
			fmt.Sprintf("Meta model discovery failed (HTTP %s)", httpStatusText(response.StatusCode)),
			response.StatusCode, body)
	}
	raw, err := io.ReadAll(response.Body)
	if err != nil {
		return nil, withContext(err, "parsing Meta model catalog")
	}
	payload, err := jsonvalue.Parse(raw)
	if err != nil {
		return nil, withContext(err, "parsing Meta model catalog")
	}
	if _, ok := payload.(jsonvalue.Object); !ok {
		return nil, withContext(errors.New("invalid type: expected struct ModelsResponse"), "parsing Meta model catalog")
	}
	var data []any
	if member, present := jsonMember(payload, "data"); present {
		list, ok := member.([]any)
		if !ok {
			return nil, withContext(errors.New("invalid type: expected a sequence"), "parsing Meta model catalog")
		}
		data = list
	}
	models := []ModelMetadata{}
	for _, model := range data {
		if id, ok := jsonField(model, "id").(string); ok && strings.HasPrefix(id, museModelPrefix) {
			models = append(models, ModelMetadataIDOnly(id))
		}
	}
	return models, nil
}

func (c *MetaClient) ListModels(ctx context.Context) ([]string, error) {
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

func (c *MetaClient) ListModelMetadata(ctx context.Context) ([]ModelMetadata, error) {
	return c.discoverModels(ctx)
}

func (c *MetaClient) ResolveModelInfo(configuredModel string) ResolvedModelInfo {
	provider := "meta"
	return ResolvedModelInfo{ConfiguredModel: configuredModel, ResolvedProvider: &provider, ResolvedModel: configuredModel}
}

func (c *MetaClient) postResponses(ctx context.Context, body []byte, apiKey string, firstProgress time.Duration) (*http.Response, error) {
	return sendWithRetries(ctx, "posting Meta Responses API request", c.http, func(attemptCtx context.Context) (*http.Request, error) {
		request, err := http.NewRequestWithContext(attemptCtx, http.MethodPost, c.baseURL+"/responses", strings.NewReader(string(body)))
		if err != nil {
			return nil, err
		}
		request.Header.Set("Authorization", "Bearer "+apiKey)
		request.Header.Set("Accept", "text/event-stream")
		request.Header.Set("Content-Type", "application/json")
		return request, nil
	}, firstProgress)
}

func (c *MetaClient) StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error) {
	body, err := buildResponsesRequest(request.Model, request.Messages, request.Tools, request.ReasoningEffort,
		request.StructuredOutput, responsesRequestOptions{})
	if err != nil {
		return nil, err
	}
	encoded, err := marshalSerdeJSON(body)
	if err != nil {
		return nil, err
	}
	apiKey, err := c.apiKeyFor(ctx)
	if err != nil {
		return nil, err
	}
	response, err := c.postResponses(ctx, encoded, apiKey, request.IdleTimeouts.FirstProgress)
	if err != nil {
		return nil, err
	}
	if response.StatusCode == http.StatusUnauthorized {
		response.Body.Close()
		if ctx.Err() != nil {
			return nil, errors.New("Meta Responses request was cancelled")
		}
		c.invalidateRejectedKey(apiKey)
		renewedKey, err := c.apiKeyFor(ctx)
		if err != nil {
			return nil, err
		}
		response, err = c.postResponses(ctx, encoded, renewedKey, request.IdleTimeouts.FirstProgress)
		if err != nil {
			return nil, err
		}
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		body := readMetaErrorBody(response)
		return nil, retryableLlmErrorForStatusAndBody(
			fmt.Sprintf("Meta Responses API failed (HTTP %s)", httpStatusText(response.StatusCode)),
			response.StatusCode, body)
	}
	outcome, err := driveResponsesSSEStream(ctx, response.Body, request.OnToken, request.OnThought, request.IdleTimeouts)
	if err != nil {
		return nil, err
	}
	return outcome.response, nil
}

// readMetaErrorBody reads at most the error-body limit, within the auth
// read time; whatever arrived in time is returned.
func readMetaErrorBody(response *http.Response) string {
	done := make(chan []byte, 1)
	go func() {
		body, _ := io.ReadAll(io.LimitReader(response.Body, metaErrorBodyLimit))
		done <- body
	}()
	timer := time.NewTimer(metaAuthReadTime)
	defer timer.Stop()
	select {
	case body := <-done:
		return strings.ToValidUTF8(string(body), "�")
	case <-timer.C:
		response.Body.Close()
		return ""
	}
}

// metaAccessTokenFromBytes is `providers.meta.access_token`, trimmed, or
// false when the file holds none.
func metaAccessTokenFromBytes(data []byte) (string, bool, error) {
	root, err := jsonvalue.Parse(data)
	if err != nil {
		return "", false, withContext(err, "parsing Meta auth file")
	}
	token, ok := jsonField(jsonField(jsonField(root, "providers"), "meta"), "access_token").(string)
	token = strings.TrimSpace(token)
	return token, ok && token != "", nil
}

func readMetaAuthFile(path string) ([]byte, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, fmt.Errorf("reading Meta auth file %s: %w", path, err)
	}
	defer file.Close()
	data, err := io.ReadAll(io.LimitReader(file, metaAuthReadLimit+1))
	if err != nil {
		return nil, withContext(err, "reading Meta auth file")
	}
	if len(data) > metaAuthReadLimit {
		return nil, errors.New("Meta auth file is too large")
	}
	return data, nil
}

func metaAuthFileHasAccessToken(path string) (bool, error) {
	data, err := readMetaAuthFile(path)
	if err != nil {
		return false, err
	}
	_, ok, err := metaAccessTokenFromBytes(data)
	return ok, err
}

// readMetaAccessTokenBounded reads the login's access token, bounded in
// size and time.
func readMetaAccessTokenBounded(path string) (string, error) {
	type result struct {
		token string
		err   error
	}
	done := make(chan result, 1)
	go func() {
		data, err := readMetaAuthFile(path)
		if err != nil {
			done <- result{err: err}
			return
		}
		token, ok, err := metaAccessTokenFromBytes(data)
		if err == nil && !ok {
			err = errors.New("Meta auth file has no providers.meta.access_token")
		}
		done <- result{token: token, err: err}
	}()
	timer := time.NewTimer(metaAuthReadTime)
	defer timer.Stop()
	select {
	case read := <-done:
		return read.token, read.err
	case <-timer.C:
		return "", errors.New("timed out reading Meta auth file")
	}
}
