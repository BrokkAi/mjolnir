// Port of mj-accounts/src/power_ranking.rs.

// Compiled-in power ranking, plus a 24-hour disk cache of the published
// table.
//
// Selection never waits on the network. The daemon loads this store at
// startup and refreshes it on a background task.

package accounts

import (
	"bytes"
	"context"
	_ "embed"
	"encoding/json"
	"fmt"
	"log/slog"
	"math"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const (
	powerRankingDefaultURL      = "https://slopcop.com/power-ranking.json"
	powerRankingFreshForSeconds = 86_400
	powerRankingFetchTimeout    = 5 * time.Second
)

//go:embed assets/power-ranking-baseline.json
var powerRankingBaselineJSON []byte

// PowerRankingEntry is one ranked `(model, effort, harness)` row from the
// published table.
type PowerRankingEntry struct {
	Config          string   `json:"config"`
	Model           string   `json:"model"`
	Effort          *string  `json:"effort"`
	Harness         *string  `json:"harness"`
	TasksPer200Sub  *float64 `json:"tasks_per_200_sub"`
	TasksPer200API  *float64 `json:"tasks_per_200_api"`
	WeeklyBudgetUsd *float64 `json:"weekly_budget_usd"`
	PlanUsdPerMonth *float64 `json:"plan_usd_per_month"`
	PlanAssumed     bool     `json:"plan_assumed"`
	Basis           string   `json:"basis"`
	Minutes         *float64 `json:"minutes"`
}

func (e *PowerRankingEntry) UnmarshalJSON(data []byte) error {
	type plain PowerRankingEntry
	return powerRankingDecode(data, (*plain)(e), "config", "model", "basis")
}

// PowerRankingSnapshot is the published table. `schema_version` 1 is the
// only shape this build reads.
type PowerRankingSnapshot struct {
	SchemaVersion uint32 `json:"schema_version"`
	// AsOf is the publication date. `null` or a missing field is absent,
	// not an error.
	AsOf *string `json:"as_of"`
	// SourceCommit is the commit that produced the table. `null` or a
	// missing field is absent.
	SourceCommit *string             `json:"source_commit"`
	Entries      []PowerRankingEntry `json:"entries"`
}

func (s PowerRankingSnapshot) MarshalJSON() ([]byte, error) {
	type plain PowerRankingSnapshot
	value := plain(s)
	if value.Entries == nil {
		value.Entries = []PowerRankingEntry{}
	}
	return json.Marshal(value)
}

func (s *PowerRankingSnapshot) UnmarshalJSON(data []byte) error {
	type plain PowerRankingSnapshot
	return powerRankingDecode(data, (*plain)(s), "schema_version", "entries")
}

// powerRankingDecode decodes data into v after checking the fields serde
// requires (neither an Option nor `#[serde(default)]`) are present.
func powerRankingDecode(data []byte, v any, required ...string) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range required {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return json.NewDecoder(bytes.NewReader(data)).Decode(v)
}

// PowerRankingSnapshotBaseline is the compiled-in baseline snapshot,
// available offline and in isolated tests.
func PowerRankingSnapshotBaseline() *PowerRankingSnapshot {
	return powerRankingBaseline()
}

// Lookup matches modelIdentity and effort for harness against this
// snapshot. Nil when nothing matches.
func (s *PowerRankingSnapshot) Lookup(modelIdentity string, effort *string, harness core.HarnessKind) *PowerRankingLookup {
	panic("unported: mj-accounts/src/power_ranking.rs PowerRankingSnapshot::lookup")
}

// PowerRankingLookup is a row chosen for a model, or the geometric mean of
// its ranked efforts.
type PowerRankingLookup struct {
	Model           string
	Effort          *string
	Harness         *string
	TasksPer200Sub  *float64
	TasksPer200API  *float64
	WeeklyBudgetUsd *float64
	PlanUsdPerMonth *float64
	PlanAssumed     bool
	Basis           string
	Minutes         *float64
	// GeometricMean is set when no ranked effort matched and the numbers are
	// a geometric mean.
	GeometricMean bool
}

type powerRankingCacheFile struct {
	MjVersion             string               `json:"mj_version"`
	FetchedAtEpochSeconds uint64               `json:"fetched_at_epoch_seconds"`
	Snapshot              PowerRankingSnapshot `json:"snapshot"`
}

func (f *powerRankingCacheFile) UnmarshalJSON(data []byte) error {
	type plain powerRankingCacheFile
	return powerRankingDecode(data, (*plain)(f), "mj_version", "fetched_at_epoch_seconds", "snapshot")
}

type powerRankingRanked struct {
	snapshot PowerRankingSnapshot
	byModel  map[string][]PowerRankingEntry
}

type powerRankingState struct {
	ranked powerRankingRanked
	// fetchedAtEpochSeconds is nil when the in-memory table is the compiled
	// baseline, not a cache.
	fetchedAtEpochSeconds *uint64
	mjVersion             *string
}

// PowerRankingStore is the ranking table selection reads. Share it by
// pointer; the table sits behind one lock.
type PowerRankingStore struct {
	mu        sync.Mutex
	state     powerRankingState
	cachePath string
	url       string
}

// PowerRankingStoreOpen loads a fresh same-version disk cache, or the
// compiled-in baseline. Does not use the network. cacheDir is the directory
// that holds `power-ranking-cache.json` (the daemon passes core.DataDir).
func PowerRankingStoreOpen(cacheDir string, nowSeconds uint64) *PowerRankingStore {
	url, ok := os.LookupEnv("MJ_POWER_RANKING_URL")
	if !ok {
		url = powerRankingDefaultURL
	}
	return PowerRankingStoreWithURL(cacheDir, nowSeconds, url)
}

// PowerRankingStoreWithURL is PowerRankingStoreOpen with an explicit fetch
// URL, for tests that must not read the process environment.
func PowerRankingStoreWithURL(cacheDir string, nowSeconds uint64, url string) *PowerRankingStore {
	cachePath := filepath.Join(cacheDir, "power-ranking-cache.json")
	return &PowerRankingStore{
		state:     loadPowerRankingState(cachePath, nowSeconds),
		cachePath: cachePath,
		url:       url,
	}
}

func (s *PowerRankingStore) Snapshot() PowerRankingSnapshot {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.state.ranked.snapshot
}

// Lookup matches modelIdentity and effort for harness.
//
// A harness-tagged row wins over an untagged row for the same model. An
// untagged row is not replaced by a different harness's tag. A missing or
// unranked effort falls back to `high`, then `medium`, then the geometric
// mean of the ranked efforts for that model and harness.
func (s *PowerRankingStore) Lookup(modelIdentity string, effort *string, harness core.HarnessKind) *PowerRankingLookup {
	panic("unported: mj-accounts/src/power_ranking.rs PowerRankingStore::lookup")
}

// Refresh fetches when the disk cache is missing, older than 24 hours, or
// from another `mj` version. A failed fetch keeps the current snapshot.
func (s *PowerRankingStore) Refresh(ctx context.Context, nowSeconds uint64) error {
	panic("unported: mj-accounts/src/power_ranking.rs PowerRankingStore::refresh")
}

func (s *powerRankingState) isFresh(nowSeconds uint64) bool {
	if s.mjVersion == nil || *s.mjVersion != core.PackageVersion || s.fetchedAtEpochSeconds == nil {
		return false
	}
	fetched := *s.fetchedAtEpochSeconds
	age := uint64(0)
	if nowSeconds > fetched {
		age = nowSeconds - fetched
	}
	return age < powerRankingFreshForSeconds
}

func newPowerRankingRanked(snapshot PowerRankingSnapshot) powerRankingRanked {
	panic("unported: mj-accounts/src/power_ranking.rs Ranked::new")
}

var powerRankingBaseline = sync.OnceValue(func() *PowerRankingSnapshot {
	var snapshot PowerRankingSnapshot
	if err := json.Unmarshal(powerRankingBaselineJSON, &snapshot); err != nil {
		panic(fmt.Sprintf("compiled power-ranking baseline is invalid: %v", err))
	}
	return &snapshot
})

func loadPowerRankingState(cachePath string, nowSeconds uint64) powerRankingState {
	panic("unported: mj-accounts/src/power_ranking.rs load_state")
}

func readPowerRankingCache(path string) *powerRankingCacheFile {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil
	}
	var cache powerRankingCacheFile
	if err := json.Unmarshal(data, &cache); err != nil {
		slog.Warn("ignoring unreadable power-ranking cache", "path", path, "error", err)
		return nil
	}
	return &cache
}

func writePowerRankingCache(path string, cache *powerRankingCacheFile) error {
	panic("unported: mj-accounts/src/power_ranking.rs write_cache")
}

func fetchPowerRankingSnapshot(ctx context.Context, url string) (PowerRankingSnapshot, error) {
	panic("unported: mj-accounts/src/power_ranking.rs fetch_snapshot")
}

func entriesForHarness(entries []PowerRankingEntry, harness core.HarnessKind) []PowerRankingEntry {
	tag := harness.ID()
	var tagged, untagged []PowerRankingEntry
	for _, entry := range entries {
		switch {
		case entry.Harness != nil && *entry.Harness == tag:
			tagged = append(tagged, entry)
		case entry.Harness == nil:
			untagged = append(untagged, entry)
		}
	}
	if len(tagged) > 0 {
		return tagged
	}
	return untagged
}

func pickEffort(entries []PowerRankingEntry, effort *string) *PowerRankingLookup {
	panic("unported: mj-accounts/src/power_ranking.rs pick_effort")
}

func lookupFrom(entry *PowerRankingEntry, geometricMean bool) PowerRankingLookup {
	return PowerRankingLookup{
		Model:           entry.Model,
		Effort:          entry.Effort,
		Harness:         entry.Harness,
		TasksPer200Sub:  entry.TasksPer200Sub,
		TasksPer200API:  entry.TasksPer200API,
		WeeklyBudgetUsd: entry.WeeklyBudgetUsd,
		PlanUsdPerMonth: entry.PlanUsdPerMonth,
		PlanAssumed:     entry.PlanAssumed,
		Basis:           entry.Basis,
		Minutes:         entry.Minutes,
		GeometricMean:   geometricMean,
	}
}

func powerRankingGeometricMean(entries []PowerRankingEntry) *PowerRankingLookup {
	panic("unported: mj-accounts/src/power_ranking.rs geometric_mean")
}

// meanOf is the geometric mean of the finite positive values, nil when
// there are none.
func meanOf(values []float64) *float64 {
	count, logSum := 0, 0.0
	for _, value := range values {
		if !math.IsInf(value, 0) && !math.IsNaN(value) && value > 0 {
			logSum += math.Log(value)
			count++
		}
	}
	if count == 0 {
		return nil
	}
	mean := math.Exp(logSum / float64(count))
	return &mean
}

// modelAliases is the query forms that should hit one stored model id.
func modelAliases(model string) []string {
	panic("unported: mj-accounts/src/power_ranking.rs model_aliases")
}

// stripContextHints drops context-window hints such as `[1m]` from
// `claude-opus-5-5[1m]`.
func stripContextHints(model string) string {
	var out strings.Builder
	rest := model
	for {
		start := strings.IndexByte(rest, '[')
		if start < 0 {
			break
		}
		out.WriteString(rest[:start])
		end := strings.IndexByte(rest[start+1:], ']')
		if end < 0 {
			return out.String()
		}
		rest = rest[start+1+end+1:]
	}
	out.WriteString(rest)
	return out.String()
}

func claudeBody(model string) (string, bool) {
	body := strings.TrimPrefix(model, "claude-")
	family := body
	if index := strings.IndexAny(body, "-."); index >= 0 {
		family = body[:index]
	}
	switch family {
	case "opus", "sonnet", "haiku", "fable", "mythos":
		return body, true
	}
	return "", false
}

func isKimi(model string) bool {
	body := strings.TrimPrefix(model, "kimi-")
	runes := []rune(body)
	return len(runes) > 1 && runes[0] == 'k' && runes[1] >= '0' && runes[1] <= '9'
}

// swapDigitSeparators: `5.5` and `5-5` name the same Claude or Kimi version.
// Other hyphens stay.
func swapDigitSeparators(model string) string {
	panic("unported: mj-accounts/src/power_ranking.rs swap_digit_separators")
}
