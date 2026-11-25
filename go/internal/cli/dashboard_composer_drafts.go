// Port of mj-cli/src/dashboard/composer_drafts.rs.
//
// Process-local composer ownership and ordered, coalescing durability.

package cli

import (
	"encoding/json"
	"maps"
	"math"
	"slices"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// composerDraftCache is this terminal's composer text for each session and
// the durability of each. The zero value is Rust's Default. It is part of
// the upgrade handoff, so its JSON is serde's for the Rust struct:
// {"drafts": {...}, "persistence": {...}}, with persistence optional when
// read (older handoffs carry only drafts).
type composerDraftCache struct {
	drafts      map[string]core.DetachedSessionDraft
	persistence map[string]*draftPersistence
}

type draftPersistence struct {
	desired uint64
	saved   uint64
	// inFlight and failed are serde(skip): a resumed handoff starts with
	// nothing in flight and nothing failed.
	inFlight *uint64
	failed   bool
}

// composerDraftCacheJSON and draftPersistenceJSON are the serde shapes.
type composerDraftCacheJSON struct {
	Drafts      map[string]core.DetachedSessionDraft `json:"drafts"`
	Persistence map[string]draftPersistenceJSON      `json:"persistence"`
}

type draftPersistenceJSON struct {
	Desired uint64 `json:"desired"`
	Saved   uint64 `json:"saved"`
}

func (c composerDraftCache) MarshalJSON() ([]byte, error) {
	wire := composerDraftCacheJSON{
		Drafts:      map[string]core.DetachedSessionDraft{},
		Persistence: map[string]draftPersistenceJSON{},
	}
	maps.Copy(wire.Drafts, c.drafts)
	for id, state := range c.persistence {
		wire.Persistence[id] = draftPersistenceJSON{Desired: state.desired, Saved: state.saved}
	}
	return json.Marshal(wire)
}

func (c *composerDraftCache) UnmarshalJSON(data []byte) error {
	panic("unported: mj-cli/src/dashboard/composer_drafts.rs ComposerDraftCache (serde Deserialize)")
}

func (c *composerDraftCache) remember(sessionID string) {
	draft := c.drafts[sessionID]
	if c.persistence == nil {
		c.persistence = map[string]*draftPersistence{}
	}
	if _, ok := c.persistence[sessionID]; ok {
		return
	}
	// Older upgrade handoffs contain only drafts. An adopted seed still
	// needs retirement even if its composer has already been cleared.
	var desired uint64
	if draft.Text != "" || (draft.InheritedInput != nil && *draft.InheritedInput != "") {
		desired = 1
	}
	c.persistence[sessionID] = &draftPersistence{desired: desired}
}

// open is the session's draft, seeding it from inheritedInput the first
// time. The returned copy shares only the immutable inherited string.
func (c *composerDraftCache) open(sessionID string, inheritedInput string) core.DetachedSessionDraft {
	if c.drafts == nil {
		c.drafts = map[string]core.DetachedSessionDraft{}
	}
	if _, ok := c.drafts[sessionID]; !ok {
		inherited := inheritedInput
		c.drafts[sessionID] = core.DetachedSessionDraft{Text: inheritedInput, InheritedInput: &inherited}
	}
	c.remember(sessionID)
	return c.drafts[sessionID]
}

func (c *composerDraftCache) capture(sessionID string, text string, inheritedInput string) core.DetachedSessionDraft {
	c.open(sessionID, inheritedInput)
	entry := c.drafts[sessionID]
	if entry.Text != text {
		entry.Text = text
		c.drafts[sessionID] = entry
		state := c.persistence[sessionID]
		if state.desired == math.MaxUint64 {
			panic("terminal draft revision exhausted")
		}
		state.desired++
		state.failed = false
	}
	return entry
}

// discard: removed sessions retain their empty local composer but have no
// writes to retry. Their records and physical persistence belong to
// lifecycle.
func (c *composerDraftCache) discard(sessionID string) {
	if c.drafts == nil {
		c.drafts = map[string]core.DetachedSessionDraft{}
	}
	c.drafts[sessionID] = core.DetachedSessionDraft{}
	// Keep the in-flight revision until its completion arrives, so quit
	// cannot lose a request whose tracker guard has just been released.
	if state, ok := c.persistence[sessionID]; ok {
		state.saved = state.desired
		state.failed = false
	}
}

func (c *composerDraftCache) get(sessionID string) (core.DetachedSessionDraft, bool) {
	draft, ok := c.drafts[sessionID]
	return draft, ok
}

// start begins a save of the session's latest revision, when one is unsaved,
// none is in flight, and the last attempt did not fail (unless retry).
func (c *composerDraftCache) start(sessionID string, retry bool) (uint64, core.DetachedSessionDraft, bool) {
	if _, ok := c.drafts[sessionID]; !ok {
		return 0, core.DetachedSessionDraft{}, false
	}
	c.remember(sessionID)
	state := c.persistence[sessionID]
	if state.inFlight != nil {
		return 0, core.DetachedSessionDraft{}, false
	}
	if state.desired <= state.saved || (state.failed && !retry) {
		return 0, core.DetachedSessionDraft{}, false
	}
	state.failed = false
	revision := state.desired
	state.inFlight = &revision
	return revision, c.drafts[sessionID], true
}

func (c *composerDraftCache) complete(sessionID string, revision uint64, succeeded bool) {
	state, ok := c.persistence[sessionID]
	if !ok {
		return
	}
	if state.inFlight == nil || *state.inFlight != revision {
		return
	}
	state.inFlight = nil
	if succeeded {
		state.saved = max(state.saved, revision)
		draft, ok := c.drafts[sessionID]
		if !ok {
			panic("composer draft persistence without a draft for " + sessionID)
		}
		draft.InheritedInput = nil
		c.drafts[sessionID] = draft
	}
	// A new edit is its own retry trigger; a failed unchanged revision
	// stays dirty until an explicit flush, without a polling retry loop.
	state.failed = !succeeded && state.desired == revision
}

func (c *composerDraftCache) sessionIDs() []string {
	return slices.Sorted(maps.Keys(c.drafts))
}

func (c *composerDraftCache) isSaving() bool {
	for _, state := range c.persistence {
		if state.inFlight != nil {
			return true
		}
	}
	return false
}
