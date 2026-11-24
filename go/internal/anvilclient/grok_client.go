// Port of brokk-anvil-client 0.28.6 src/grok_client.rs (the part Mjolnir uses).

package anvilclient

import (
	"context"
	"net/http"
	"path/filepath"
)

const grokAPIBaseURL = "https://cli-chat-proxy.grok.com/v1"

// GrokClient talks to the Grok CLI chat proxy with a Grok Build login.
type GrokClient struct {
	auth          any
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
	panic("unported: brokk-anvil-client/src/grok_client.rs GrokClient::load_with_config")
}

func (c *GrokClient) ListModels(ctx context.Context) ([]string, error) {
	panic("unported: brokk-anvil-client/src/grok_client.rs GrokClient::list_models")
}

func (c *GrokClient) ListModelMetadata(ctx context.Context) ([]ModelMetadata, error) {
	panic("unported: brokk-anvil-client/src/grok_client.rs GrokClient::list_model_metadata")
}

func (c *GrokClient) StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error) {
	panic("unported: brokk-anvil-client/src/grok_client.rs GrokClient::stream_chat")
}
