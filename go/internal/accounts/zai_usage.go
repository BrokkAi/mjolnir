// Port of mj-accounts/src/zai_usage.rs.

// GLM Coding Plan quota, read from the Z.ai provider's monitor endpoint.
//
// A Codex profile that names Z.ai as its model provider authenticates with
// a long-lived Coding Plan key from the profile's `environment`. The same
// key is a bearer token for the provider's quota endpoint, so quota
// reporting needs only the provider's host and that key.

package accounts

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"net/http"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	zaiQuotaPath             = "/api/monitor/usage/quota/limit"
	zaiUsageMaxResponseBytes = 1024 * 1024
)

// ServesQuota is whether host serves the Coding Plan quota endpoint. Only
// the Z.ai family of hosts does, so the answer follows the provider kind.
func ServesQuota(host string) bool {
	return core.CodexProviderKindFromHost(host) == core.CodexProviderKindZai
}

type ZaiUsageWindow struct {
	Label            string
	RemainingPercent uint8
	Used             *int64
	Limit            *int64
	ResetsAt         *int64
}

// zaiLimit is one entry of the response's `data.limits`.
type zaiLimit struct {
	Kind          string   `json:"type"`
	Number        *uint64  `json:"number"`
	Usage         *int64   `json:"usage"`
	CurrentValue  *int64   `json:"currentValue"`
	Remaining     *int64   `json:"remaining"`
	Percentage    *float64 `json:"percentage"`
	NextResetTime *int64   `json:"nextResetTime"`
}

func (l *zaiLimit) UnmarshalJSON(data []byte) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	if _, ok := fields["type"]; !ok {
		return errors.New("missing field `type`")
	}
	type plain zaiLimit
	return json.NewDecoder(bytes.NewReader(data)).Decode((*plain)(l))
}

// ZaiUsageQuery asks host (a bare host name such as `api.z.ai`) for the
// Coding Plan windows belonging to apiKey.
func ZaiUsageQuery(ctx context.Context, host, apiKey string) ([]ZaiUsageWindow, error) {
	panic("unported: mj-accounts/src/zai_usage.rs query")
}

func parseZaiLimit(limit zaiLimit) *ZaiUsageWindow {
	panic("unported: mj-accounts/src/zai_usage.rs parse_limit")
}

func zaiReadBounded(response *http.Response) ([]byte, error) {
	panic("unported: mj-accounts/src/zai_usage.rs read_bounded")
}
