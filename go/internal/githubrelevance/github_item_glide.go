// Port of mj-github-relevance/src/github_item_glide.rs.

// Hosted GLiDE boundary for aliases600 evidence.

package githubrelevance

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const endpoint = "https://mj-jev-proxy.eng-admin-a63.workers.dev/v1/github-item-glide"

// ClassifyProbability is the hosted GLiDE probability that the item is
// relevant to the session. fastinoKey is the user's own Fastino key, nil to
// use the shared proxy key.
func ClassifyProbability(ctx context.Context, evidence *core.GithubItemEvidence, fastinoKey *string) (float64, error) {
	return classifyAt(ctx, evidence, endpoint, fastinoKey)
}

func classifyAt(ctx context.Context, evidence *core.GithubItemEvidence, endpoint string, fastinoKey *string) (float64, error) {
	panic("unported: mj-github-relevance/src/github_item_glide.rs classify_at")
}
