// Port of brokk-anvil-client 0.28.6 src/codex_client.rs (the part Mjolnir uses).

package anvilclient

import (
	"context"
	"net/http"
	"sync"
)

// CodexClient talks to the ChatGPT Responses API with a Codex login.
type CodexClient struct {
	http     *http.Client
	authPath string
	// refreshMu serializes concurrent token-refresh attempts. Without it,
	// two in-flight prompts hitting a 401 race each other into the refresh
	// endpoint and one of the resulting refresh tokens is invalidated by the
	// server's rotation policy.
	refreshMu         sync.Mutex
	noticesMu         sync.Mutex
	discoveryNotices  []ModelDiscoveryNotice
	modelCatalogCache any
}

// NewCodexClientWithAuthPath builds a client that reads and refreshes the
// login at authPath. The ChatGPT backend sits behind Cloudflare, so the
// client keeps a per-client cookie jar.
func NewCodexClientWithAuthPath(authPath string) *CodexClient {
	panic("unported: brokk-anvil-client/src/codex_client.rs CodexClient::with_auth_path")
}

func (c *CodexClient) ListModels(ctx context.Context) ([]string, error) {
	panic("unported: brokk-anvil-client/src/codex_client.rs CodexClient::list_models")
}

func (c *CodexClient) ListModelMetadata(ctx context.Context) ([]ModelMetadata, error) {
	panic("unported: brokk-anvil-client/src/codex_client.rs CodexClient::list_model_metadata")
}

func (c *CodexClient) SupportsNativeStructuredOutput() bool {
	panic("unported: brokk-anvil-client/src/codex_client.rs CodexClient::supports_native_structured_output")
}

func (c *CodexClient) TakeModelDiscoveryNotices() []ModelDiscoveryNotice {
	panic("unported: brokk-anvil-client/src/codex_client.rs CodexClient::take_model_discovery_notices")
}

func (c *CodexClient) StreamChat(ctx context.Context, request StreamChatRequest) (LlmResponse, error) {
	panic("unported: brokk-anvil-client/src/codex_client.rs CodexClient::stream_chat")
}
