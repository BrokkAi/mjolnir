// Port of brokk-anvil-client 0.28.6 src/meta_client.rs (the part Mjolnir uses).

package anvilclient

import (
	"context"
	"net/http"
	"path/filepath"
)

const (
	metaAPIBaseURL  = "https://api.meta.ai/v1"
	metaMintBaseURL = "https://api.meta.ai"
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

// MetaClient talks to Meta's API with a Muse Code login, minting an API key
// from its access token.
type MetaClient struct {
	auth        any
	http        *http.Client
	baseURL     string
	mintBaseURL string
}

// MetaClientLoadWithConfig builds a client from config's login, or nil when
// the login file is missing or holds no access token.
func MetaClientLoadWithConfig(config MetaClientConfig) (LlmBackend, error) {
	panic("unported: brokk-anvil-client/src/meta_client.rs MetaClient::load_with_config")
}

func (c *MetaClient) ListModels(ctx context.Context) ([]string, error) {
	panic("unported: brokk-anvil-client/src/meta_client.rs MetaClient::list_models")
}

func (c *MetaClient) ListModelMetadata(ctx context.Context) ([]ModelMetadata, error) {
	panic("unported: brokk-anvil-client/src/meta_client.rs MetaClient::list_model_metadata")
}

func (c *MetaClient) StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error) {
	panic("unported: brokk-anvil-client/src/meta_client.rs MetaClient::stream_chat")
}
