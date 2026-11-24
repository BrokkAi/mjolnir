// Port of the tests in mj-accounts/src/power_ranking.rs.

package accounts

import (
	"context"
	"fmt"
	"io"
	"math"
	"net"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func powerRankingCloseEnough(actual *float64, expected float64) bool {
	return actual != nil && math.Abs(*actual-expected) < 0.001
}

func powerRankingText(value string) *string { return &value }

func powerRankingFloat(value float64) *float64 { return &value }

func TestPowerRankingCacheFreshnessAndCanonicalLookups(t *testing.T) {
	now := uint64(2_000_000)
	ctx := context.Background()
	offline := t.TempDir()
	baseline := PowerRankingStoreWithURL(offline, now, "http://127.0.0.1:1/power-ranking.json")
	opus := baseline.Lookup("opus-5.5", powerRankingText("xhigh"), core.HarnessKindClaude)
	if opus == nil {
		t.Fatal("baseline opus")
	}
	if !powerRankingCloseEnough(opus.TasksPer200Sub, 548.0) || opus.GeometricMean {
		t.Fatalf("%+v", opus)
	}
	assertCanonicalLookups(t, baseline)

	var hits atomic.Int64
	published := &powerRankingPublished{status: 200, body: powerRankingPublishedBody(t, 900.0)}
	url := spawnRankingServer(t, &hits, published)

	freshDir := t.TempDir()
	writePowerRankingCacheFile(t, freshDir, core.PackageVersion, now-60, powerRankingSentinelSnapshot(42.0))
	// Open reads the override; t.Setenv removes it when the test ends.
	t.Setenv("MJ_POWER_RANKING_URL", url)
	fresh := PowerRankingStoreOpen(freshDir, now)
	hit := fresh.Lookup("sentinel-model", powerRankingText("high"), core.HarnessKindCodex)
	if hit == nil || !powerRankingCloseEnough(hit.TasksPer200Sub, 42.0) {
		t.Fatalf("fresh cache: %+v", hit)
	}
	if err := fresh.Refresh(ctx, now); err != nil {
		t.Fatalf("fresh cache skips the network: %v", err)
	}
	if hits.Load() != 0 {
		t.Fatal("a fresh cache is not fetched")
	}

	staleDir := t.TempDir()
	writePowerRankingCacheFile(t, staleDir, core.PackageVersion, now-powerRankingFreshForSeconds, powerRankingSentinelSnapshot(7.0))
	stale := PowerRankingStoreWithURL(staleDir, now, url)
	if !powerRankingCloseEnough(stale.Lookup("sentinel-model", powerRankingText("high"), core.HarnessKindCodex).TasksPer200Sub, 7.0) {
		t.Fatal("stale cache lookup")
	}
	if err := stale.Refresh(ctx, now); err != nil {
		t.Fatalf("expired cache refreshes: %v", err)
	}
	if hits.Load() < 1 {
		t.Fatal("expired cache was not fetched")
	}
	if !powerRankingCloseEnough(stale.Lookup("sentinel-model", powerRankingText("high"), core.HarnessKindCodex).TasksPer200Sub, 900.0) {
		t.Fatal("refreshed lookup")
	}

	versionDir := t.TempDir()
	writePowerRankingCacheFile(t, versionDir, "0.0.0-not-this", now-10, powerRankingSentinelSnapshot(1234.0))
	invalidated := PowerRankingStoreWithURL(versionDir, now, url)
	if invalidated.Lookup("sentinel-model", powerRankingText("high"), core.HarnessKindCodex) != nil {
		t.Fatal("a different mj version does not keep the old snapshot")
	}
	if !powerRankingCloseEnough(invalidated.Lookup("opus-5.5", powerRankingText("xhigh"), core.HarnessKindClaude).TasksPer200Sub, 548.0) {
		t.Fatal("baseline after version mismatch")
	}
	before := hits.Load()
	if err := invalidated.Refresh(ctx, now); err != nil {
		t.Fatalf("version mismatch refreshes: %v", err)
	}
	if hits.Load() <= before {
		t.Fatal("version mismatch was not fetched")
	}
	if !powerRankingCloseEnough(invalidated.Lookup("sentinel-model", powerRankingText("high"), core.HarnessKindCodex).TasksPer200Sub, 900.0) {
		t.Fatal("refreshed lookup after version mismatch")
	}

	retainedDir := t.TempDir()
	writePowerRankingCacheFile(t, retainedDir, core.PackageVersion, now-powerRankingFreshForSeconds-5, powerRankingSentinelSnapshot(55.0))
	published.set(500, "nope")
	retained := PowerRankingStoreWithURL(retainedDir, now, url)
	err := retained.Refresh(ctx, now)
	if err == nil {
		t.Fatal("HTTP 500")
	}
	if !strings.Contains(err.Error(), "500") {
		t.Fatal(err)
	}
	if !powerRankingCloseEnough(retained.Lookup("sentinel-model", powerRankingText("high"), core.HarnessKindCodex).TasksPer200Sub, 55.0) {
		t.Fatal("retained lookup")
	}

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	refused := listener.Addr().String()
	listener.Close()
	unreachable := PowerRankingStoreWithURL(retainedDir, now, fmt.Sprintf("http://%s/power-ranking.json", refused))
	if unreachable.Refresh(ctx, now) == nil {
		t.Fatal("an unreachable table refreshed")
	}
	if !powerRankingCloseEnough(unreachable.Lookup("sentinel-model", powerRankingText("high"), core.HarnessKindCodex).TasksPer200Sub, 55.0) {
		t.Fatal("unreachable lookup")
	}
}

func assertCanonicalLookups(t *testing.T, store *PowerRankingStore) {
	t.Helper()
	lookup := func(model string, effort *string, harness core.HarnessKind) *PowerRankingLookup {
		t.Helper()
		found := store.Lookup(model, effort, harness)
		if found == nil {
			t.Fatalf("no ranking for %s", model)
		}
		return found
	}
	floatEqual := func(left, right *float64) bool {
		if left == nil || right == nil {
			return left == right
		}
		return *left == *right
	}
	opus := lookup("claude-opus-5-5", powerRankingText("xhigh"), core.HarnessKindClaude)
	if opus.Model != "claude-opus-5-5" || !powerRankingCloseEnough(opus.TasksPer200Sub, 548.0) ||
		!powerRankingCloseEnough(opus.WeeklyBudgetUsd, 2281.06) {
		t.Fatalf("%+v", opus)
	}
	dotted := lookup("opus-5.5", powerRankingText("xhigh"), core.HarnessKindClaude)
	if !floatEqual(dotted.TasksPer200Sub, opus.TasksPer200Sub) || !floatEqual(dotted.WeeklyBudgetUsd, opus.WeeklyBudgetUsd) {
		t.Fatalf("%+v", dotted)
	}
	hinted := lookup("claude-opus-5-5[1m]", powerRankingText("xhigh"), core.HarnessKindClaude)
	if hinted.Model != opus.Model || !floatEqual(hinted.WeeklyBudgetUsd, opus.WeeklyBudgetUsd) {
		t.Fatalf("%+v", hinted)
	}
	bare := lookup("opus", powerRankingText("xhigh"), core.HarnessKindClaude)
	if bare.Model != "claude-opus-5" || !powerRankingCloseEnough(bare.TasksPer200Sub, 190.09) {
		t.Fatalf("%+v", bare)
	}
	high := lookup("opus-5.5", nil, core.HarnessKindClaude)
	if high.Effort == nil || *high.Effort != "high" || !powerRankingCloseEnough(high.TasksPer200Sub, 899.59) {
		t.Fatalf("%+v", high)
	}

	sol := lookup("gpt-6.1-sol", powerRankingText("high"), core.HarnessKindCodex)
	if !powerRankingCloseEnough(sol.TasksPer200Sub, 828.2) || !powerRankingCloseEnough(sol.WeeklyBudgetUsd, 887.76) {
		t.Fatalf("%+v", sol)
	}
	neon := lookup("gemini-4-neon", powerRankingText("xhigh"), core.HarnessKindAntigravity)
	if !powerRankingCloseEnough(neon.TasksPer200Sub, 327.23) {
		t.Fatalf("%+v", neon)
	}
	kimi := lookup("k3.1-pro", powerRankingText("max"), core.HarnessKindKimi)
	if kimi.Model != "kimi-k3.1-pro" || !powerRankingCloseEnough(kimi.TasksPer200Sub, 100.16) {
		t.Fatalf("%+v", kimi)
	}
	kimiPrefixed := lookup("kimi-k3.1-pro", powerRankingText("max"), core.HarnessKindKimi)
	if !floatEqual(kimi.TasksPer200Sub, kimiPrefixed.TasksPer200Sub) {
		t.Fatalf("%+v", kimiPrefixed)
	}
	kimiNative := lookup("kimi-code/k3.1-pro", powerRankingText("max"), core.HarnessKindKimi)
	if kimiNative.Model != "kimi-k3.1-pro" || !floatEqual(kimi.TasksPer200Sub, kimiNative.TasksPer200Sub) {
		t.Fatalf("%+v", kimiNative)
	}
	grok := lookup("grok-4.7", powerRankingText("high"), core.HarnessKindGrok)
	if !powerRankingCloseEnough(grok.TasksPer200Sub, 236.65) {
		t.Fatalf("%+v", grok)
	}

	dsh := lookup("deepseek-flash", powerRankingText("max"), core.HarnessKindDsh)
	if dsh.Harness == nil || *dsh.Harness != "dsh" || !powerRankingCloseEnough(dsh.TasksPer200API, 438.78) {
		t.Fatalf("%+v", dsh)
	}
	if store.Lookup("deepseek-flash", powerRankingText("max"), core.HarnessKindClaude) != nil {
		t.Fatal("a dsh-only row is not a Claude row")
	}

	mean := lookup("kimi-k3", nil, core.HarnessKindKimi)
	if !mean.GeometricMean {
		t.Fatal("kimi-k3 is ranked only at max")
	}
	if mean.Basis != "geometric-mean" || !powerRankingCloseEnough(mean.TasksPer200Sub, 53.22) {
		t.Fatalf("%+v", mean)
	}
}

func powerRankingSentinelSnapshot(tasks float64) PowerRankingSnapshot {
	return PowerRankingSnapshot{
		SchemaVersion: 1,
		AsOf:          powerRankingText("2026-11-19"),
		SourceCommit:  powerRankingText("test"),
		Entries: []PowerRankingEntry{{
			Config:         "sentinel-model/high",
			Model:          "sentinel-model",
			Effort:         powerRankingText("high"),
			TasksPer200Sub: powerRankingFloat(tasks),
			TasksPer200API: powerRankingFloat(1.0),
			Basis:          "test",
			Minutes:        powerRankingFloat(1.0),
		}},
	}
}

func powerRankingPublishedBody(t *testing.T, tasks float64) string {
	t.Helper()
	body, err := powerRankingSentinelSnapshot(tasks).MarshalJSON()
	if err != nil {
		t.Fatal(err)
	}
	return string(body)
}

func writePowerRankingCacheFile(t *testing.T, dir, version string, fetchedAt uint64, snapshot PowerRankingSnapshot) {
	t.Helper()
	path := filepath.Join(dir, "power-ranking-cache.json")
	if err := writePowerRankingCache(path, &powerRankingCacheFile{
		MjVersion:             version,
		FetchedAtEpochSeconds: fetchedAt,
		Snapshot:              snapshot,
	}); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatal(err)
	}
}

// powerRankingPublished is the status and body the test server answers.
type powerRankingPublished struct {
	mu     sync.Mutex
	status int
	body   string
}

func (p *powerRankingPublished) set(status int, body string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.status, p.body = status, body
}

func (p *powerRankingPublished) get() (int, string) {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.status, p.body
}

func spawnRankingServer(t *testing.T, hits *atomic.Int64, published *powerRankingPublished) string {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { listener.Close() })
	go func() {
		for {
			stream, err := listener.Accept()
			if err != nil {
				return
			}
			buffer := make([]byte, 2048)
			_, _ = stream.Read(buffer)
			hits.Add(1)
			status, payload := published.get()
			reason := "ERR"
			if status == 200 {
				reason = "OK"
			}
			_, _ = io.WriteString(stream, fmt.Sprintf(
				"HTTP/1.1 %d %s\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s",
				status, reason, len(payload), payload))
			stream.Close()
		}
	}()
	return fmt.Sprintf("http://%s/power-ranking.json", listener.Addr())
}
