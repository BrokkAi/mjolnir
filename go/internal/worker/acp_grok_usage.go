// Port of mj-worker/src/acp/grok_usage.rs.

// Grok's prompt ledger, reported in completion metadata and notifications.

package worker

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"reflect"
	"slices"
	"sync"
	"time"

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
	var report Report
	if err := acpFeaturesDecodeValue(value, &report); err != nil {
		return core.TokenUsage{}, fmt.Errorf("decode Grok turn usage: %w", err)
	}
	return normalize(report, elapsedMs, true)
}

// normalize checks a report's counters and converts it, with its per-model
// rows when models is set.
func normalize(report Report, elapsedMs *uint64, models bool) (core.TokenUsage, error) {
	total := report.InputTokens + report.OutputTokens
	if total < report.InputTokens {
		return core.TokenUsage{}, errors.New("Grok usage total overflow")
	}
	if report.TotalTokens != nil && *report.TotalTokens != total {
		return core.TokenUsage{}, errors.New("Grok usage total differs from full input plus output")
	}
	if report.ReasoningTokens != nil && *report.ReasoningTokens > report.OutputTokens {
		return core.TokenUsage{}, errors.New("Grok reasoning exceeds output tokens")
	}
	read, creation := acpFeaturesU64Or(report.CachedReadTokens, 0), acpFeaturesU64Or(report.CacheCreationTokens, 0)
	cache := read + creation
	if cache < read {
		return core.TokenUsage{}, errors.New("Grok cache token overflow")
	}
	if cache > report.InputTokens {
		return core.TokenUsage{}, errors.New("Grok cache exceeds full input tokens")
	}
	if !models && len(report.ModelUsage) != 0 {
		return core.TokenUsage{}, errors.New("nested Grok model usage is unsupported")
	}
	modelUsage := map[string]core.TokenUsage{}
	names := make([]string, 0, len(report.ModelUsage))
	for name := range report.ModelUsage {
		names = append(names, name)
	}
	slices.Sort(names)
	for _, name := range names {
		var model Report
		if err := json.Unmarshal(report.ModelUsage[name], &model); err != nil {
			return core.TokenUsage{}, fmt.Errorf("decode Grok per-model usage: %w", err)
		}
		model.UsageIsIncomplete = model.UsageIsIncomplete || report.UsageIsIncomplete
		model.CostIsPartial = model.CostIsPartial || report.CostIsPartial
		usage, err := normalize(model, nil, false)
		if err != nil {
			return core.TokenUsage{}, err
		}
		modelUsage[name] = usage
	}
	scope := core.UsageScopeTurn
	if report.UsageIsIncomplete {
		scope = core.UsageScopeUnspecified
	}
	var cost *core.ProviderTurnCost
	if report.CostUsdTicks != nil {
		value := core.ProviderTurnCostFromUsdTicks(*report.CostUsdTicks, report.CostIsPartial)
		cost = &value
	}
	return core.TokenUsage{
		Scope:             scope,
		InputTokens:       report.InputTokens,
		OutputTokens:      report.OutputTokens,
		TotalTokens:       total,
		ThoughtTokens:     report.ReasoningTokens,
		CachedReadTokens:  report.CachedReadTokens,
		CachedWriteTokens: report.CacheCreationTokens,
		ProviderDetails: &core.ProviderTurnUsage{
			Cost:          cost,
			ModelCalls:    report.ModelCalls,
			APIDurationMs: report.APIDurationMs,
			ElapsedMs:     elapsedMs,
			ModelUsage:    modelUsage,
		},
	}, nil
}

// grokUsageEqual is TokenUsage's derived PartialEq.
func grokUsageEqual(a, b *core.TokenUsage) bool {
	if a == nil || b == nil {
		return a == nil && b == nil
	}
	return reflect.DeepEqual(*a, *b)
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
	kind, _ := acpFeaturesString(update, "sessionUpdate")
	if p.sessionID == nil || *p.sessionID != sessionID || kind != "turn_completed" {
		return false, nil
	}
	id, ok := acpFeaturesString(update, "prompt_id")
	if !ok {
		return false, errors.New("Grok turn completion lacks prompt_id")
	}
	usage, ok := acpFeaturesGet(update, "usage")
	if !ok {
		return false, errors.New("Grok completion lacks usage")
	}
	var elapsed *uint64
	if value, ok := acpFeaturesGet(update, "elapsed_ms"); ok {
		if ms, ok := acpFeaturesAsU64(value); ok {
			elapsed = &ms
		}
	}
	report, err := parse(usage, elapsed)
	if err != nil {
		return false, err
	}
	previous, known := p.reports[id]
	if !known && len(p.reports) >= 64 {
		return false, errors.New("too many unmatched Grok usage reports")
	}
	if known {
		if !grokUsageEqual(previous, &report) {
			p.reports[id] = nil
			return false, errors.New("conflicting Grok usage reports for one prompt")
		}
	} else {
		if p.reports == nil {
			p.reports = map[string]*core.TokenUsage{}
		}
		p.reports[id] = &report
	}
	return true, nil
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
	c.mu.Lock()
	defer c.mu.Unlock()
	observed, err := c.pending.Observe(sessionID, update)
	if err != nil {
		return err
	}
	if observed {
		c.changedWatch().SendReplace(struct{}{})
	}
	return nil
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
	var id *string
	var reported *core.TokenUsage
	var reportErr error
	if meta != nil {
		value, ok := meta.Get("promptId")
		if !ok {
			value, ok = meta.Get("requestId")
		}
		if text, isText := value.(string); ok && isText {
			id = &text
		}
		if usage, ok := meta.Get("usage"); ok {
			parsed, err := parse(usage, nil)
			if err != nil {
				reportErr = err
			} else {
				reported = &parsed
			}
		}
	}
	// Some versions send extension usage just after the ACP response. Bound
	// this wait; missing telemetry must never strand a completed prompt.
	if reportErr == nil && reported == nil && id != nil {
		c.awaitNotification(ctx, *id, time.Second)
	}
	c.mu.Lock()
	notification := c.pending.Finish(id)
	c.mu.Unlock()
	if reportErr != nil {
		return nil, reportErr
	}
	switch {
	case reported != nil && notification != nil:
		var elapsed *uint64
		if notification.ProviderDetails != nil {
			elapsed = notification.ProviderDetails.ElapsedMs
		}
		if reported.ProviderDetails != nil {
			details := *reported.ProviderDetails
			details.ElapsedMs = elapsed
			reported.ProviderDetails = &details
		}
		if !grokUsageEqual(reported, notification) {
			return nil, errors.New("Grok response and notification disagree about turn usage")
		}
		return reported, nil
	case reported != nil:
		return reported, nil
	case notification != nil:
		return notification, nil
	}
	return standard, nil
}

// awaitNotification waits up to limit for the notification of prompt id,
// or until ctx ends.
func (c *Collector) awaitNotification(ctx context.Context, id string, limit time.Duration) {
	deadline := time.NewTimer(limit)
	defer deadline.Stop()
	for {
		c.mu.Lock()
		changed := c.changedWatch().Changed()
		present := c.pending.Contains(id)
		c.mu.Unlock()
		if present {
			return
		}
		select {
		case <-changed:
		case <-deadline.C:
			return
		case <-ctx.Done():
			return
		}
	}
}
