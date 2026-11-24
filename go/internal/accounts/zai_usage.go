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
	"fmt"
	"io"
	"math"
	"net/http"
	"time"

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
	client := &http.Client{
		Timeout: 8 * time.Second,
		CheckRedirect: func(*http.Request, []*http.Request) error {
			return http.ErrUseLastResponse
		},
	}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, "https://"+host+zaiQuotaPath, nil)
	if err != nil {
		return nil, errors.New("Coding Plan quota request failed")
	}
	request.Header.Set("Authorization", "Bearer "+apiKey)
	request.Header.Set("Accept", "application/json")
	response, err := client.Do(request)
	if err != nil {
		if accountsIsTimeout(err) {
			return nil, errors.New("Coding Plan quota request timed out")
		}
		return nil, errors.New("Coding Plan quota request failed")
	}
	defer response.Body.Close()
	status := response.StatusCode
	if status == http.StatusUnauthorized || status == http.StatusForbidden {
		return nil, errors.New("Coding Plan API key was rejected")
	}
	if status < 200 || status > 299 {
		return nil, fmt.Errorf("Coding Plan quota request returned HTTP %s", reqwestStatusDisplay(status))
	}
	body, err := zaiReadBounded(response)
	if err != nil {
		return nil, err
	}
	decoder := json.NewDecoder(bytes.NewReader(body))
	decoder.UseNumber()
	var payload any
	if err := decoder.Decode(&payload); err != nil {
		return nil, fmt.Errorf("decode Coding Plan quota response: %w", err)
	}
	object, _ := payload.(map[string]any)
	if success, _ := object["success"].(bool); !success {
		return nil, errors.New("Coding Plan quota service rejected the request")
	}
	var limitsValue any
	if data, ok := object["data"].(map[string]any); ok {
		limitsValue = data["limits"]
	}
	if limitsValue == nil {
		return nil, errors.New("decode Coding Plan quota limits: invalid type: null, expected a sequence")
	}
	encoded, err := json.Marshal(limitsValue)
	if err != nil {
		return nil, fmt.Errorf("decode Coding Plan quota limits: %w", err)
	}
	var limits []zaiLimit
	if err := json.Unmarshal(encoded, &limits); err != nil {
		return nil, fmt.Errorf("decode Coding Plan quota limits: %w", err)
	}
	windows := []ZaiUsageWindow{}
	for _, limit := range limits {
		if window := parseZaiLimit(limit); window != nil {
			windows = append(windows, *window)
		}
	}
	if len(windows) == 0 {
		return nil, errors.New("Coding Plan quota response contained no inference windows")
	}
	return windows, nil
}

func parseZaiLimit(limit zaiLimit) *ZaiUsageWindow {
	if limit.Kind != "CREDIT_LIMIT" && limit.Kind != "TOKENS_LIMIT" {
		return nil
	}
	var (
		used, total      *int64
		remainingPercent uint8
	)
	switch {
	case limit.CurrentValue != nil && limit.Remaining != nil &&
		*limit.CurrentValue >= 0 && *limit.Remaining >= 0 &&
		zaiSaturatingAdd(*limit.CurrentValue, *limit.Remaining) > 0:
		sum := zaiSaturatingAdd(*limit.CurrentValue, *limit.Remaining)
		percent := saturatingU8(math.Round(float64(*limit.Remaining) / float64(sum) * 100.0))
		used, total, remainingPercent = limit.CurrentValue, &sum, min(percent, 100)
	case limit.CurrentValue != nil && limit.Remaining != nil &&
		*limit.CurrentValue >= 0 && limit.Usage != nil && *limit.Usage > 0:
		percent := saturatingU8(math.Round(100.0 - float64(*limit.CurrentValue)/float64(*limit.Usage)*100.0))
		used, total, remainingPercent = limit.CurrentValue, limit.Usage, min(percent, 100)
	default:
		if limit.Percentage == nil {
			return nil
		}
		usedPercent := *limit.Percentage
		if usedPercent < 0 {
			usedPercent = 0
		} else if usedPercent > 100 {
			usedPercent = 100
		}
		used, total, remainingPercent = limit.CurrentValue, limit.Usage, saturatingU8(math.Round(100.0-usedPercent))
	}
	var label string
	switch {
	case limit.Number == nil:
		label = "Credits"
	case *limit.Number == 5:
		label = "5H"
	case *limit.Number == 1 || *limit.Number == 7:
		label = "Week"
	default:
		label = fmt.Sprintf("%d", *limit.Number)
	}
	var resetsAt *int64
	if limit.NextResetTime != nil {
		seconds := *limit.NextResetTime / 1000
		resetsAt = &seconds
	}
	return &ZaiUsageWindow{
		Label:            label,
		RemainingPercent: remainingPercent,
		Used:             used,
		Limit:            total,
		ResetsAt:         resetsAt,
	}
}

func zaiReadBounded(response *http.Response) ([]byte, error) {
	body, err := io.ReadAll(io.LimitReader(response.Body, zaiUsageMaxResponseBytes+1))
	if err != nil {
		return nil, fmt.Errorf("read Coding Plan quota response: %w", err)
	}
	if len(body) > zaiUsageMaxResponseBytes {
		return nil, errors.New("Coding Plan quota response is too large")
	}
	return body, nil
}

// zaiSaturatingAdd is Rust's `i64::saturating_add`.
func zaiSaturatingAdd(a, b int64) int64 {
	sum := a + b
	if a > 0 && b > 0 && sum < 0 {
		return math.MaxInt64
	}
	if a < 0 && b < 0 && sum >= 0 {
		return math.MinInt64
	}
	return sum
}
