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
	"io"
	"log/slog"
	"math"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

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

// MarshalJSON writes the row as serde does: fields in declaration order,
// absent options as null, floats in serde_json's form.
func (e PowerRankingEntry) MarshalJSON() ([]byte, error) {
	return jsonvalue.Marshal(e.value())
}

func (e PowerRankingEntry) value() jsonvalue.Object {
	optionalText := func(value *string) any {
		if value == nil {
			return nil
		}
		return *value
	}
	optionalFloat := func(value *float64) any {
		if value == nil {
			return nil
		}
		return *value
	}
	return jsonvalue.Object{
		{Key: "config", Value: e.Config},
		{Key: "model", Value: e.Model},
		{Key: "effort", Value: optionalText(e.Effort)},
		{Key: "harness", Value: optionalText(e.Harness)},
		{Key: "tasks_per_200_sub", Value: optionalFloat(e.TasksPer200Sub)},
		{Key: "tasks_per_200_api", Value: optionalFloat(e.TasksPer200API)},
		{Key: "weekly_budget_usd", Value: optionalFloat(e.WeeklyBudgetUsd)},
		{Key: "plan_usd_per_month", Value: optionalFloat(e.PlanUsdPerMonth)},
		{Key: "plan_assumed", Value: e.PlanAssumed},
		{Key: "basis", Value: e.Basis},
		{Key: "minutes", Value: optionalFloat(e.Minutes)},
	}
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
	return jsonvalue.Marshal(s.value())
}

func (s PowerRankingSnapshot) value() jsonvalue.Object {
	optionalText := func(value *string) any {
		if value == nil {
			return nil
		}
		return *value
	}
	entries := make([]any, 0, len(s.Entries))
	for _, entry := range s.Entries {
		entries = append(entries, entry.value())
	}
	return jsonvalue.Object{
		{Key: "schema_version", Value: s.SchemaVersion},
		{Key: "as_of", Value: optionalText(s.AsOf)},
		{Key: "source_commit", Value: optionalText(s.SourceCommit)},
		{Key: "entries", Value: entries},
	}
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
	var matched []PowerRankingEntry
	for _, alias := range modelAliases(modelIdentity) {
		for _, entry := range s.Entries {
			if powerRankingASCIILower(entry.Model) == alias {
				matched = append(matched, entry)
			}
		}
		if len(matched) > 0 {
			break
		}
	}
	if len(matched) == 0 {
		return nil
	}
	matched = entriesForHarness(matched, harness)
	if len(matched) == 0 {
		return nil
	}
	return pickEffort(matched, powerRankingLowerEffort(effort))
}

// powerRankingASCIILower is Rust's `str::to_ascii_lowercase`.
func powerRankingASCIILower(text string) string {
	lowered := []byte(text)
	for index, b := range lowered {
		if 'A' <= b && b <= 'Z' {
			lowered[index] = b + ('a' - 'A')
		}
	}
	return string(lowered)
}

// powerRankingLowerEffort is the effort lowercased as Rust's lookups ask.
func powerRankingLowerEffort(effort *string) *string {
	if effort == nil {
		return nil
	}
	lowered := powerRankingASCIILower(*effort)
	return &lowered
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
	s.mu.Lock()
	defer s.mu.Unlock()
	var entries []PowerRankingEntry
	found := false
	for _, alias := range modelAliases(modelIdentity) {
		if entries, found = s.state.ranked.byModel[alias]; found {
			break
		}
	}
	if !found {
		return nil
	}
	matched := entriesForHarness(entries, harness)
	if len(matched) == 0 {
		return nil
	}
	return pickEffort(matched, powerRankingLowerEffort(effort))
}

// Refresh fetches when the disk cache is missing, older than 24 hours, or
// from another `mj` version. A failed fetch keeps the current snapshot.
func (s *PowerRankingStore) Refresh(ctx context.Context, nowSeconds uint64) error {
	s.mu.Lock()
	fresh := s.state.isFresh(nowSeconds)
	s.mu.Unlock()
	if fresh {
		return nil
	}
	fetched, err := fetchPowerRankingSnapshot(ctx, s.url)
	if err != nil {
		slog.Warn("power-ranking refresh failed; keeping the current snapshot", "error", err)
		return err
	}
	cache := powerRankingCacheFile{
		MjVersion:             core.PackageVersion,
		FetchedAtEpochSeconds: nowSeconds,
		Snapshot:              fetched,
	}
	writeErr := writePowerRankingCache(s.cachePath, &cache)
	s.mu.Lock()
	fetchedAt := nowSeconds
	version := cache.MjVersion
	s.state.fetchedAtEpochSeconds = &fetchedAt
	s.state.mjVersion = &version
	s.state.ranked = newPowerRankingRanked(cache.Snapshot)
	s.mu.Unlock()
	if writeErr != nil {
		slog.Warn("power-ranking cache write failed; updated in-memory snapshot",
			"error", writeErr, "path", s.cachePath)
		return writeErr
	}
	return nil
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
	byModel := map[string][]PowerRankingEntry{}
	for _, entry := range snapshot.Entries {
		byModel[entry.Model] = append(byModel[entry.Model], entry)
	}
	// `opus/xhigh` is published as model `claude-opus-5`. Index that shorter
	// config spelling too, unless it already names another model.
	for _, entry := range snapshot.Entries {
		stem, _, _ := strings.Cut(entry.Config, "/")
		if stem == "" || stem == entry.Model {
			continue
		}
		bucket, exists := byModel[stem]
		if !exists {
			bucket = []PowerRankingEntry{}
		}
		other := false
		for _, existing := range bucket {
			if existing.Model != entry.Model {
				other = true
				break
			}
		}
		if !other {
			bucket = append(bucket, entry)
		}
		byModel[stem] = bucket
	}
	return powerRankingRanked{snapshot: snapshot, byModel: byModel}
}

var powerRankingBaseline = sync.OnceValue(func() *PowerRankingSnapshot {
	var snapshot PowerRankingSnapshot
	if err := json.Unmarshal(powerRankingBaselineJSON, &snapshot); err != nil {
		panic(fmt.Sprintf("compiled power-ranking baseline is invalid: %v", err))
	}
	return &snapshot
})

func loadPowerRankingState(cachePath string, nowSeconds uint64) powerRankingState {
	if cache := readPowerRankingCache(cachePath); cache != nil && cache.MjVersion == core.PackageVersion {
		// A stale same-version cache stays in memory so a failed refresh
		// keeps the last good table. A timestamp in the future is clamped so
		// a skewed clock cannot keep the cache fresh forever.
		fetchedAt := min(cache.FetchedAtEpochSeconds, nowSeconds)
		version := cache.MjVersion
		return powerRankingState{
			ranked:                newPowerRankingRanked(cache.Snapshot),
			fetchedAtEpochSeconds: &fetchedAt,
			mjVersion:             &version,
		}
	}
	return powerRankingState{ranked: newPowerRankingRanked(*powerRankingBaseline())}
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
	parent := filepath.Dir(path)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return fmt.Errorf("create %s: %w", parent, err)
	}
	body, err := jsonvalue.Marshal(jsonvalue.Object{
		{Key: "mj_version", Value: cache.MjVersion},
		{Key: "fetched_at_epoch_seconds", Value: cache.FetchedAtEpochSeconds},
		{Key: "snapshot", Value: cache.Snapshot.value()},
	})
	if err != nil {
		return fmt.Errorf("encode power-ranking cache: %w", err)
	}
	// Rust's `Path::with_extension("json.tmp")`.
	temporary := strings.TrimSuffix(path, filepath.Ext(path)) + ".json.tmp"
	if err := os.WriteFile(temporary, body, 0o666); err != nil {
		return fmt.Errorf("write %s: %w", temporary, err)
	}
	if err := os.Rename(temporary, path); err != nil {
		return fmt.Errorf("replace %s: %w", path, err)
	}
	return nil
}

func fetchPowerRankingSnapshot(ctx context.Context, url string) (PowerRankingSnapshot, error) {
	client := &http.Client{Timeout: powerRankingFetchTimeout}
	request, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return PowerRankingSnapshot{}, fmt.Errorf("fetch %s: %w", url, err)
	}
	request.Header.Set("User-Agent", "mj/"+core.PackageVersion)
	response, err := client.Do(request)
	if err != nil {
		return PowerRankingSnapshot{}, fmt.Errorf("fetch %s: %w", url, err)
	}
	defer response.Body.Close()
	if response.StatusCode < 200 || response.StatusCode > 299 {
		return PowerRankingSnapshot{}, fmt.Errorf("power-ranking fetch returned %s", reqwestStatusDisplay(response.StatusCode))
	}
	body, err := io.ReadAll(response.Body)
	if err != nil {
		return PowerRankingSnapshot{}, fmt.Errorf("decode power-ranking JSON: %w", err)
	}
	var snapshot PowerRankingSnapshot
	if err := json.Unmarshal(body, &snapshot); err != nil {
		return PowerRankingSnapshot{}, fmt.Errorf("decode power-ranking JSON: %w", err)
	}
	if snapshot.SchemaVersion != 1 {
		return PowerRankingSnapshot{}, fmt.Errorf("power-ranking schema_version %d is not supported", snapshot.SchemaVersion)
	}
	return snapshot, nil
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
	find := func(wanted string) *PowerRankingEntry {
		for index := range entries {
			if entries[index].Effort != nil && *entries[index].Effort == wanted {
				return &entries[index]
			}
		}
		return nil
	}
	if effort != nil {
		if found := find(*effort); found != nil {
			lookup := lookupFrom(found, false)
			return &lookup
		}
	}
	for _, fallback := range []string{"high", "medium"} {
		if found := find(fallback); found != nil {
			lookup := lookupFrom(found, false)
			return &lookup
		}
	}
	return powerRankingGeometricMean(entries)
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
	if len(entries) == 0 {
		return nil
	}
	first := &entries[0]
	values := func(field func(*PowerRankingEntry) *float64) []float64 {
		collected := []float64{}
		for index := range entries {
			if value := field(&entries[index]); value != nil {
				collected = append(collected, *value)
			}
		}
		return collected
	}
	planAssumed := false
	for _, entry := range entries {
		planAssumed = planAssumed || entry.PlanAssumed
	}
	return &PowerRankingLookup{
		Model:           first.Model,
		Harness:         first.Harness,
		TasksPer200Sub:  meanOf(values(func(entry *PowerRankingEntry) *float64 { return entry.TasksPer200Sub })),
		TasksPer200API:  meanOf(values(func(entry *PowerRankingEntry) *float64 { return entry.TasksPer200API })),
		WeeklyBudgetUsd: meanOf(values(func(entry *PowerRankingEntry) *float64 { return entry.WeeklyBudgetUsd })),
		PlanUsdPerMonth: meanOf(values(func(entry *PowerRankingEntry) *float64 { return entry.PlanUsdPerMonth })),
		PlanAssumed:     planAssumed,
		Basis:           "geometric-mean",
		Minutes:         meanOf(values(func(entry *PowerRankingEntry) *float64 { return entry.Minutes })),
		GeometricMean:   true,
	}
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
	normalized := stripContextHints(core.NormalizeModel(model))
	// Kimi advertises `kimi-code/k3.1-pro`; the table stores `kimi-k3.1-pro`.
	normalized = strings.TrimPrefix(normalized, "kimi-code/")
	stripped := normalized
	if trimmed, ok := strings.CutSuffix(normalized, "-dsh"); ok {
		stripped = trimmed
	} else if trimmed, ok := strings.CutSuffix(normalized, "-mj"); ok {
		stripped = trimmed
	}
	keys := []string{}
	push := func(key string) {
		if key == "" {
			return
		}
		for _, existing := range keys {
			if existing == key {
				return
			}
		}
		keys = append(keys, key)
	}
	push(stripped)
	if body, ok := claudeBody(stripped); ok {
		for _, form := range []string{body, swapDigitSeparators(body)} {
			push("claude-" + form)
			push(form)
		}
	}
	if isKimi(stripped) {
		body := strings.TrimPrefix(stripped, "kimi-")
		for _, form := range []string{body, swapDigitSeparators(body)} {
			push("kimi-" + form)
			push(form)
		}
	}
	return keys
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
	chars := []rune(model)
	isDigit := func(ch rune) bool { return '0' <= ch && ch <= '9' }
	var swapped strings.Builder
	swapped.Grow(len(model))
	for index, ch := range chars {
		betweenDigits := index > 0 && index+1 < len(chars) && isDigit(chars[index-1]) && isDigit(chars[index+1])
		switch {
		case ch == '.' && betweenDigits:
			swapped.WriteRune('-')
		case ch == '-' && betweenDigits:
			swapped.WriteRune('.')
		default:
			swapped.WriteRune(ch)
		}
	}
	return swapped.String()
}
