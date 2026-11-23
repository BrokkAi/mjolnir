// Port of mj-github-relevance/src/github_item_glide.rs.

// Hosted GLiDE boundary for aliases600 evidence.

package githubrelevance

import (
	"context"
	"errors"
	"math"

	"github.com/BrokkAi/acp-go/jsonvalue"

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
	body, err := serdeJSON(evidence)
	if err != nil {
		return 0, err
	}
	response, err := PostBoundedJSON(ctx, endpoint, fastinoKey, body, core.GithubItemMaxBodyBytes, "request GitHub item GLiDE probability")
	if err != nil {
		return 0, err
	}
	var value any
	if object, ok := response.(jsonvalue.Object); ok {
		value, _ = object.Get("probability")
	}
	probability, ok := jsonF64(value)
	if !ok {
		return 0, errors.New("missing GitHub item GLiDE probability")
	}
	if math.IsNaN(probability) || math.IsInf(probability, 0) || probability < 0 || probability > 1 {
		return 0, errors.New("invalid GitHub item GLiDE probability")
	}
	return probability, nil
}

// jsonF64 is serde_json's Value::as_f64: any number as a float64.
func jsonF64(value any) (float64, bool) {
	switch number := value.(type) {
	case float64:
		return number, true
	case int64:
		return float64(number), true
	case uint64:
		return float64(number), true
	}
	return 0, false
}
