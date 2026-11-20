// Port of mj-worker/src/acp/grok_usage.rs.

// Grok's prompt ledger, reported in completion metadata and notifications.

package worker

import (
	"context"
	"encoding/json"
	"sync"

	"github.com/BrokkAi/acp-go/schema"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Report is Grok's usage report, as completion metadata and turn-completed
// notifications carry it.
type Report struct {
	InputTokens         uint64                     `json:"inputTokens"`
	OutputTokens        uint64                     `json:"outputTokens"`
	TotalTokens         *uint64                    `json:"totalTokens"`
	ReasoningTokens     *uint64                    `json:"reasoningTokens"`
	CachedReadTokens    *uint64                    `json:"cachedReadTokens"`
	CacheCreationTokens *uint64                    `json:"cacheCreationTokens"`
	ModelCalls          *uint64                    `json:"modelCalls"`
	APIDurationMs       *uint64                    `json:"apiDurationMs"`
	CostUsdTicks        *uint64                    `json:"costUsdTicks"`
	CostIsPartial       bool                       `json:"costIsPartial"`
	UsageIsIncomplete   bool                       `json:"usageIsIncomplete"`
	ModelUsage          map[string]json.RawMessage `json:"modelUsage"`
}

func (r *Report) UnmarshalJSON(data []byte) error {
	if err := acpFeaturesRequireFields(data, "inputTokens", "outputTokens"); err != nil {
		return err
	}
	type plain Report
	return json.Unmarshal(data, (*plain)(r))
}

// parse decodes one Grok usage report (a JSON value) into a turn's usage.
func parse(value any, elapsedMs *uint64) (core.TokenUsage, error) {
	panic("unported: mj-worker/src/acp/grok_usage.rs parse")
}

// normalize checks a report's counters and converts it, with its per-model
// rows when models is set.
func normalize(report Report, elapsedMs *uint64, models bool) (core.TokenUsage, error) {
	panic("unported: mj-worker/src/acp/grok_usage.rs normalize")
}

// PendingUsage holds the notifications observed during a live prompt. Only
// those are candidates. Correlate their native prompt IDs with the response,
// never with a previous prompt.
type PendingUsage struct {
	sessionID *string
	// reports maps a prompt ID to its report; nil marks conflicting reports.
	reports map[string]*core.TokenUsage
}

func (p *PendingUsage) Begin(sessionID string) {
	p.sessionID = &sessionID
	clear(p.reports)
}

// Observe records a turn-completed notification for the live prompt, and
// reports whether it was one.
func (p *PendingUsage) Observe(sessionID string, update any) (bool, error) {
	panic("unported: mj-worker/src/acp/grok_usage.rs PendingUsage::observe")
}

func (p *PendingUsage) Contains(id string) bool {
	_, ok := p.reports[id]
	return ok
}

// Finish ends the prompt and returns the report for id, if one arrived and
// none conflicted. A nil id is no prompt ID.
func (p *PendingUsage) Finish(id *string) *core.TokenUsage {
	p.sessionID = nil
	var result *core.TokenUsage
	if id != nil {
		result = p.reports[*id]
	}
	clear(p.reports)
	return result
}

// Collector is the bridge's Grok usage ledger. Share it as *Collector (Rust
// clones its Arcs); the zero value is ready to use (Rust Default).
type Collector struct {
	mu      sync.Mutex
	pending PendingUsage
	// changed is woken whenever a notification is recorded (Rust Notify).
	// Created on first use under mu.
	changed *core.Watch[struct{}]
}

// changedWatch is the change notifier, created on first use. The caller
// holds c.mu.
func (c *Collector) changedWatch() *core.Watch[struct{}] {
	if c.changed == nil {
		c.changed = core.NewWatch(struct{}{})
	}
	return c.changed
}

func (c *Collector) Begin(sessionID string) {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.pending.Begin(sessionID)
}

func (c *Collector) Observe(sessionID string, update any) error {
	panic("unported: mj-worker/src/acp/grok_usage.rs Collector::observe")
}

func (c *Collector) Clear() {
	c.mu.Lock()
	defer c.mu.Unlock()
	c.pending.Finish(nil)
}

// Complete returns the prompt's usage: the response's `_meta` report, the
// notification for its prompt, or standard when Grok reported neither. A nil
// meta is an absent `_meta`; a nil standard is no standard usage.
func (c *Collector) Complete(ctx context.Context, meta schema.Meta, standard *core.TokenUsage) (*core.TokenUsage, error) {
	panic("unported: mj-worker/src/acp/grok_usage.rs Collector::complete")
}
