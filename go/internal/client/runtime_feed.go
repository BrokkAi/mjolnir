// Port of mj-client/src/runtime_feed.rs.

// Keyed runtime publications. A cursor belongs to one daemon incarnation.
//
// This file also holds the JSON helpers the client-rest files share
// (feedMarshalJSON, feedObjectFields, feedDecodeStruct, sortedStringSet):
// serde's wire forms that encoding/json does not produce by itself.

package client

import (
	"bytes"
	"cmp"
	"encoding/json"
	"errors"
	"fmt"
	"iter"
	"maps"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// feedMarshalJSON encodes v as serde_json does for the shapes here: without
// encoding/json's HTML escaping of <, > and &.
func feedMarshalJSON(v any) ([]byte, error) {
	var b bytes.Buffer
	encoder := json.NewEncoder(&b)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(v); err != nil {
		return nil, err
	}
	return bytes.TrimSuffix(b.Bytes(), []byte("\n")), nil
}

// feedObjectFields reads a JSON object's members, as a serde struct or
// internally tagged enum reads them.
func feedObjectFields(data []byte) (map[string]json.RawMessage, error) {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return nil, err
	}
	if fields == nil {
		return nil, errors.New("invalid type: null, expected a JSON object")
	}
	return fields, nil
}

// feedDecodeStruct decodes data into v, a plain alias of a struct without
// its custom methods, as serde derives a struct's Deserialize: each of
// required must be present (serde's "missing field" for a field that is
// neither an Option nor #[serde(default)]), and with denyUnknown a field v
// does not declare is refused (#[serde(deny_unknown_fields)]).
func feedDecodeStruct(data []byte, v any, denyUnknown bool, required ...string) error {
	fields, err := feedObjectFields(data)
	if err != nil {
		return err
	}
	for _, name := range required {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	if denyUnknown {
		decoder.DisallowUnknownFields()
	}
	return decoder.Decode(v)
}

// sortedStringSet is a BTreeSet<String>: a set of names that serializes as
// a sorted JSON array.
type sortedStringSet map[string]struct{}

func (s sortedStringSet) MarshalJSON() ([]byte, error) {
	names := slices.Sorted(maps.Keys(s))
	if names == nil {
		names = []string{}
	}
	return feedMarshalJSON(names)
}

func (s *sortedStringSet) UnmarshalJSON(data []byte) error {
	var names []string
	if err := json.Unmarshal(data, &names); err != nil {
		return err
	}
	if names == nil {
		return errors.New("invalid type: null, expected a sequence")
	}
	set := make(sortedStringSet, len(names))
	for _, name := range names {
		set[name] = struct{}{}
	}
	*s = set
	return nil
}

// RuntimeCursor is a client's position in one daemon incarnation's feed.
type RuntimeCursor struct {
	// AlertSequence is the durable alert frontier acknowledged by this
	// client.
	AlertSequence uint64 `json:"alert_sequence"`
	Incarnation   string `json:"incarnation"`
	Sequence      uint64 `json:"sequence"`
	// SubagentParents is the families whose historical session rows belong
	// to this cursor's view.
	SubagentParents sortedStringSet `json:"subagent_parents,omitempty"`
}

func (c RuntimeCursor) MarshalJSON() ([]byte, error) {
	type plain RuntimeCursor
	return feedMarshalJSON(plain(c))
}

func (c *RuntimeCursor) UnmarshalJSON(data []byte) error {
	type plain RuntimeCursor
	var value plain
	if err := feedDecodeStruct(data, &value, false, "incarnation", "sequence"); err != nil {
		return err
	}
	*c = RuntimeCursor(value)
	return nil
}

// Equal is Rust's derived PartialEq.
func (c RuntimeCursor) Equal(other RuntimeCursor) bool {
	return c.AlertSequence == other.AlertSequence && c.Incarnation == other.Incarnation &&
		c.Sequence == other.Sequence && maps.Equal(c.SubagentParents, other.SubagentParents)
}

// RuntimeBootstrap is one atomic runtime snapshot used to start a replica
// without another download. Unknown fields are refused.
type RuntimeBootstrap struct {
	Cursor     RuntimeCursor     `json:"cursor"`
	Projection RuntimeProjection `json:"projection"`
}

func (b *RuntimeBootstrap) UnmarshalJSON(data []byte) error {
	type plain RuntimeBootstrap
	return feedDecodeStruct(data, (*plain)(b), true, "cursor", "projection")
}

func (b RuntimeBootstrap) IntoReplica() RuntimeReplica {
	cursor := b.Cursor
	return RuntimeReplica{Cursor: &cursor, Projection: b.Projection}
}

// SessionVerdict is session facts and action offers from the daemon's
// capture. Every field defaults when absent (serde default).
type SessionVerdict struct {
	Transition         *core.SessionTransitionKind `json:"transition"`
	TransitionFailure  *core.SessionTransitionKind `json:"transition_failure"`
	LiveDashboard      bool                        `json:"live_dashboard"`
	NeedsPerson        bool                        `json:"needs_person"`
	MoveQueueHeld      bool                        `json:"move_queue_held"`
	Idle               bool                        `json:"idle"`
	Working            bool                        `json:"working"`
	ActiveWork         bool                        `json:"active_work"`
	Unfinished         bool                        `json:"unfinished"`
	Interrupt          bool                        `json:"interrupt"`
	InterruptShell     bool                        `json:"interrupt_shell,omitempty"`
	InterruptMember    bool                        `json:"interrupt_member"`
	InterruptAll       bool                        `json:"interrupt_all"`
	InterruptCount     int                         `json:"interrupt_count"`
	Stop               SessionOffer                `json:"stop"`
	Restart            SessionOffer                `json:"restart"`
	MoveSession        SessionOffer                `json:"move_session"`
	ChangeWorkspace    SessionOffer                `json:"change_workspace"`
	ContainerSettings  SessionOffer                `json:"container_settings"`
	Resume             SessionOffer                `json:"resume"`
	WorkingChildren    int                         `json:"working_children"`
	UnfinishedChildren int                         `json:"unfinished_children"`
}

func (v SessionVerdict) MarshalJSON() ([]byte, error) {
	type plain SessionVerdict
	return feedMarshalJSON(plain(v))
}

// Equal is Rust's derived PartialEq.
func (v SessionVerdict) Equal(other SessionVerdict) bool {
	left, right := v, other
	left.Transition, right.Transition = nil, nil
	left.TransitionFailure, right.TransitionFailure = nil, nil
	return left == right && quotaOptionalEqual(v.Transition, other.Transition) &&
		quotaOptionalEqual(v.TransitionFailure, other.TransitionFailure)
}

// SessionOffer is whether a session action is offered, and why not.
// Serialized snake_case; the zero value is the default, Hidden.
type SessionOffer int

const (
	SessionOfferHidden SessionOffer = iota
	SessionOfferReady
	SessionOfferMoveQueueHeld
	SessionOfferTransitioning
	SessionOfferTransitionFailed
	SessionOfferOperationInProgress
	SessionOfferNoRecoveryCopy
	SessionOfferNoOtherWorkspace
	SessionOfferActive
	SessionOfferMoveEnvironmentRetained
)

var sessionOfferNames = [...]string{
	"hidden", "ready", "move_queue_held", "transitioning", "transition_failed",
	"operation_in_progress", "no_recovery_copy", "no_other_workspace", "active",
	"move_environment_retained",
}

func (o SessionOffer) MarshalText() ([]byte, error) {
	if o < 0 || int(o) >= len(sessionOfferNames) {
		return nil, fmt.Errorf("unknown session offer %d", o)
	}
	return []byte(sessionOfferNames[o]), nil
}

func (o *SessionOffer) UnmarshalText(text []byte) error {
	index := slices.Index(sessionOfferNames[:], string(text))
	if index < 0 {
		quoted := make([]string, len(sessionOfferNames))
		for i, name := range sessionOfferNames {
			quoted[i] = "`" + name + "`"
		}
		return fmt.Errorf("unknown variant `%s`, expected one of %s", text, strings.Join(quoted, ", "))
	}
	*o = SessionOffer(index)
	return nil
}

// Offered is whether the action is offered.
func (o SessionOffer) Offered() bool { return o == SessionOfferReady }

// RuntimeProjection is the daemon's runtime publication.
type RuntimeProjection struct {
	SubagentParents sortedStringSet `json:"subagent_parents"`
	// Revision is the legacy ordering shared with one-shot management
	// responses.
	Revision          uint64                                            `json:"revision"`
	Records           core.SnapshotMap[string, core.SessionRecord]      `json:"records"`
	EnvironmentOwners core.SnapshotMap[string, string]                  `json:"environment_owners"`
	Subagents         core.SnapshotMap[string, core.SubagentMetadata]   `json:"subagents"`
	Sessions          core.SnapshotMap[string, RuntimeSessionView]      `json:"sessions"`
	Verdicts          core.SnapshotMap[string, SessionVerdict]          `json:"verdicts"`
	Moves             core.SnapshotMap[string, core.MoveOperation]      `json:"moves"`
	NativeAgents      core.SnapshotMap[string, core.NativeAgentSummary] `json:"native_agents"`
	SessionCPU        core.SnapshotMap[string, SessionCpuView]          `json:"session_cpu"`
	Metadata          RuntimeMetadata                                   `json:"metadata"`
	// Transcripts is each live session's transcript tail. Never part of a
	// snapshot frame: sixty tails can be hundreds of megabytes. A client
	// fetches the tails it needs at its cursor, then follows them through
	// deltas.
	Transcripts core.SnapshotMap[string, SessionTail] `json:"-"`
}

func (p RuntimeProjection) MarshalJSON() ([]byte, error) {
	type plain RuntimeProjection
	value := plain(p)
	if value.SubagentParents == nil {
		value.SubagentParents = sortedStringSet{}
	}
	encoded, err := feedMarshalJSON(value)
	if err != nil {
		return nil, err
	}
	return feedOmitEmptyMembers(encoded, "environment_owners")
}

// feedOmitEmptyMembers removes each named member whose value is an empty
// object or list: serde's skip_serializing_if = "is_empty" on a map that Go
// encodes as a struct, where omitempty does not apply.
func feedOmitEmptyMembers(encoded []byte, names ...string) ([]byte, error) {
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		return nil, err
	}
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return encoded, nil
	}
	changed := false
	for _, name := range names {
		member, found := object.Get(name)
		if !found {
			continue
		}
		empty := false
		switch typed := member.(type) {
		case jsonvalue.Object:
			empty = len(typed) == 0
		case []any:
			empty = len(typed) == 0
		}
		if empty {
			object.Delete(name)
			changed = true
		}
	}
	if !changed {
		return encoded, nil
	}
	return jsonvalue.Marshal(object)
}

// Equal is Rust's derived PartialEq.
func (p *RuntimeProjection) Equal(other *RuntimeProjection) bool {
	return maps.Equal(p.SubagentParents, other.SubagentParents) && p.Revision == other.Revision &&
		p.Records.Equal(other.Records, feedJSONEqual[core.SessionRecord]) &&
		p.EnvironmentOwners.Equal(other.EnvironmentOwners, feedComparableEqual[string]) &&
		p.Subagents.Equal(other.Subagents, feedJSONEqual[core.SubagentMetadata]) &&
		p.Sessions.Equal(other.Sessions, runtimeSessionViewEqual) &&
		p.Verdicts.Equal(other.Verdicts, sessionVerdictEqual) &&
		p.Moves.Equal(other.Moves, feedJSONEqual[core.MoveOperation]) &&
		p.NativeAgents.Equal(other.NativeAgents, feedJSONEqual[core.NativeAgentSummary]) &&
		p.SessionCPU.Equal(other.SessionCPU, feedJSONEqual[SessionCpuView]) &&
		p.Metadata.Equal(&other.Metadata) &&
		p.Transcripts.Equal(other.Transcripts, sessionTailEqual)
}

// feedJSONEqual is a type's PartialEq read as the values' serialized forms,
// for the record types the feed only compares.
func feedJSONEqual[T any](a, b *T) bool {
	if a == b {
		return true
	}
	left, leftErr := settingsToValue(a)
	right, rightErr := settingsToValue(b)
	return leftErr == nil && rightErr == nil && settingsEqual(left, right)
}

func feedComparableEqual[T comparable](a, b *T) bool { return *a == *b }

func sessionVerdictEqual(a, b *SessionVerdict) bool { return a.Equal(*b) }

// runtimeSessionViewEqual is RuntimeSessionView's PartialEq: its serialized
// fields and the in-process ones serde skips.
func runtimeSessionViewEqual(a, b *RuntimeSessionView) bool {
	if (a.Materialized == nil) != (b.Materialized == nil) {
		return false
	}
	if a.Materialized != nil && !a.Materialized.Equal(*b.Materialized, feedJSONEqual[RuntimeMaterializedSession]) {
		return false
	}
	return a.MaterializedOnly == b.MaterializedOnly && feedJSONEqual(a, b)
}

func (p *RuntimeProjection) UnmarshalJSON(data []byte) error {
	type plain RuntimeProjection
	var value plain
	if err := feedDecodeStruct(data, &value, false,
		"revision", "records", "subagents", "sessions", "moves", "native_agents", "metadata"); err != nil {
		return err
	}
	*p = RuntimeProjection(value)
	return nil
}

// ForSubagentParents publishes row detail for live work and the families
// the client has opened. Compact relationship metadata remains available for
// counts/navigation.
func (p *RuntimeProjection) ForSubagentParents(parents map[string]struct{}) RuntimeProjection {
	return NewScopedProjection(p, maps.Clone(parents)).Projection
}

// ScopedProjection is a projection scoped to opened families, with the
// inputs its selection read. Advance scopes a later publication from what
// changed between the two, so a terminal's poll costs its changes rather
// than the whole history.
type ScopedProjection struct {
	Projection RuntimeProjection
	selection  scopeSelection
}

// scopeSelection is what decides membership besides each session's own
// record and relation.
type scopeSelection struct {
	// operations is the sessions with an active lifecycle or move operation.
	operations map[string]struct{}
	// ancestors is the opened parents and all of their ancestors.
	ancestors map[string]struct{}
}

func scopeSelectionOf(full *RuntimeProjection, parents map[string]struct{}) scopeSelection {
	operations := map[string]struct{}{}
	for _, operation := range full.Metadata.Lifecycles {
		operations[operation.SessionID] = struct{}{}
	}
	for id, operation := range full.Moves.All() {
		if operation.IsActive() {
			operations[id] = struct{}{}
		}
	}
	ancestors := maps.Clone(parents)
	if ancestors == nil {
		ancestors = map[string]struct{}{}
	}
	for parent := range parents {
		id := parent
		seen := map[string]struct{}{}
		for {
			if _, repeated := seen[id]; repeated {
				break
			}
			seen[id] = struct{}{}
			relation := full.Subagents.GetShared(id)
			if relation == nil {
				break
			}
			ancestors[relation.ParentSessionID] = struct{}{}
			id = relation.ParentSessionID
		}
	}
	return scopeSelection{operations: operations, ancestors: ancestors}
}

func (s *scopeSelection) keeps(full *RuntimeProjection, parents map[string]struct{}, id string) bool {
	record := full.Records.GetShared(id)
	if record == nil {
		return false
	}
	relation := full.Subagents.GetShared(id)
	inFamily := false
	if relation != nil {
		_, inFamily = parents[relation.ParentSessionID]
	}
	var live bool
	if relation == nil {
		live = record.State != core.SessionStateStopped
	} else {
		live = record.State != core.SessionStateStopped && record.State != core.SessionStateParked
	}
	// Notification membership is the daemon's served answer, including
	// stopped sessions that have no live Sessions-pane row.
	notification := false
	if relation == nil {
		if attention := full.Metadata.SessionAttention.GetShared(id); attention != nil {
			switch *attention {
			case core.SessionAttentionNeedsInput, core.SessionAttentionImportant,
				core.SessionAttentionFailed, core.SessionAttentionUnreachable:
				notification = true
			}
		}
	}
	_, operation := s.operations[id]
	_, ancestor := s.ancestors[id]
	return notification || operation || (!record.Archived && (live || inFamily || ancestor))
}

// keepsRelation: relationships stay for counts and navigation unless their
// session is archived. A lifecycle can retain an archived row; its
// relationship stays too, so clients still recognize it as a child
// throughout cleanup.
func keepsRelation(full *RuntimeProjection, id string, rowKept bool) bool {
	if rowKept {
		return true
	}
	record := full.Records.GetShared(id)
	return record != nil && !record.Archived
}

// NewScopedProjection scopes full to the families parents opened.
func NewScopedProjection(full *RuntimeProjection, parents map[string]struct{}) ScopedProjection {
	if parents == nil {
		parents = map[string]struct{}{}
	}
	selection := scopeSelectionOf(full, parents)
	result := *full
	result.Records = full.Records.Filtered(func(id string, _ *core.SessionRecord) bool {
		return selection.keeps(full, parents, id)
	})
	result.SubagentParents = sortedStringSet(parents)
	kept := result.Records
	result.EnvironmentOwners = full.EnvironmentOwners.Filtered(func(id string, _ *string) bool {
		return kept.ContainsKey(id)
	})
	result.Subagents.Retain(func(id string, _ *core.SubagentMetadata) bool {
		return keepsRelation(full, id, kept.ContainsKey(id))
	})
	result.Sessions = full.Sessions.Filtered(func(id string, view *RuntimeSessionView) bool {
		return kept.ContainsKey(id) && !view.MaterializedOnly
	})
	result.Verdicts = full.Verdicts.Filtered(func(id string, _ *SessionVerdict) bool {
		if kept.ContainsKey(id) {
			return true
		}
		agent := full.NativeAgents.GetShared(id)
		return agent != nil && kept.ContainsKey(agent.Agent.OwnerSessionID)
	})
	result.Moves = full.Moves.Filtered(func(id string, _ *core.MoveOperation) bool { return kept.ContainsKey(id) })
	result.SessionCPU = full.SessionCPU.Filtered(func(id string, _ *SessionCpuView) bool { return kept.ContainsKey(id) })
	result.Transcripts = full.Transcripts.Filtered(func(id string, _ *SessionTail) bool { return kept.ContainsKey(id) })
	result.NativeAgents = full.NativeAgents.Filtered(func(_ string, agent *core.NativeAgentSummary) bool {
		return kept.ContainsKey(agent.Agent.OwnerSessionID)
	})
	return ScopedProjection{Projection: result, selection: selection}
}

// Advance scopes after given that s scopes before. Only sessions whose rows
// or selection inputs changed are visited, and unchanged rows keep their
// shared records, so a delta from s costs what changed.
func (s *ScopedProjection) Advance(before, after *RuntimeProjection) ScopedProjection {
	parents := map[string]struct{}(s.Projection.SubagentParents)
	if parents == nil {
		parents = map[string]struct{}{}
	}
	selection := scopeSelectionOf(after, parents)
	result := s.Projection
	result.Revision = after.Revision
	result.Metadata = after.Metadata

	candidates := map[string]struct{}{}
	for id := range before.Records.Changes(after.Records, feedJSONEqual[core.SessionRecord]) {
		candidates[id] = struct{}{}
	}
	for id := range before.Subagents.Changes(after.Subagents, feedJSONEqual[core.SubagentMetadata]) {
		candidates[id] = struct{}{}
	}
	for id := range before.Metadata.SessionAttention.Changes(after.Metadata.SessionAttention, feedComparableEqual[core.SessionAttention]) {
		candidates[id] = struct{}{}
	}
	for _, sets := range [][2]map[string]struct{}{
		{s.selection.operations, selection.operations},
		{s.selection.ancestors, selection.ancestors},
	} {
		for id := range sets[0] {
			if _, both := sets[1][id]; !both {
				candidates[id] = struct{}{}
			}
		}
		for id := range sets[1] {
			if _, both := sets[0][id]; !both {
				candidates[id] = struct{}{}
			}
		}
	}
	moved := map[string]struct{}{}
	for _, id := range slices.Sorted(maps.Keys(candidates)) {
		keep := selection.keeps(after, parents, id)
		if result.Records.ContainsKey(id) != keep {
			moved[id] = struct{}{}
		}
		result.Records.CopyEntry(after.Records, id, keep)
		result.Subagents.CopyEntry(after.Subagents, id, keepsRelation(after, id, keep))
	}

	records := result.Records
	followRecords(&result.EnvironmentOwners, before.EnvironmentOwners, after.EnvironmentOwners, records, moved, feedComparableEqual[string])
	sessionIDs := maps.Clone(moved)
	for id := range before.Sessions.Changes(after.Sessions, runtimeSessionViewEqual) {
		sessionIDs[id] = struct{}{}
	}
	for _, id := range slices.Sorted(maps.Keys(sessionIDs)) {
		view := after.Sessions.GetShared(id)
		keep := records.ContainsKey(id) && view != nil && !view.MaterializedOnly
		result.Sessions.CopyEntry(after.Sessions, id, keep)
	}
	followRecords(&result.Moves, before.Moves, after.Moves, records, moved, feedJSONEqual[core.MoveOperation])
	followRecords(&result.SessionCPU, before.SessionCPU, after.SessionCPU, records, moved, feedJSONEqual[SessionCpuView])
	followRecords(&result.Transcripts, before.Transcripts, after.Transcripts, records, moved, sessionTailEqual)
	var owned []string
	for owner := range moved {
		owned = append(owned, core.OwnerViewIDPrefix(owner))
	}
	agents := map[string]struct{}{}
	for id := range before.NativeAgents.Changes(after.NativeAgents, feedJSONEqual[core.NativeAgentSummary]) {
		agents[id] = struct{}{}
	}
	for _, prefix := range owned {
		for id := range core.SnapshotMapKeysWithPrefix(after.NativeAgents, prefix) {
			agents[id] = struct{}{}
		}
	}
	for _, id := range slices.Sorted(maps.Keys(agents)) {
		agent := after.NativeAgents.GetShared(id)
		keep := agent != nil && records.ContainsKey(agent.Agent.OwnerSessionID)
		result.NativeAgents.CopyEntry(after.NativeAgents, id, keep)
	}
	verdicts := maps.Clone(moved)
	for id := range before.Verdicts.Changes(after.Verdicts, sessionVerdictEqual) {
		verdicts[id] = struct{}{}
	}
	for _, prefix := range owned {
		for id := range core.SnapshotMapKeysWithPrefix(after.Verdicts, prefix) {
			verdicts[id] = struct{}{}
		}
		for id := range core.SnapshotMapKeysWithPrefix(before.Verdicts, prefix) {
			verdicts[id] = struct{}{}
		}
	}
	for _, id := range slices.Sorted(maps.Keys(verdicts)) {
		keep := records.ContainsKey(id)
		if !keep {
			agent := after.NativeAgents.GetShared(id)
			keep = agent != nil && records.ContainsKey(agent.Agent.OwnerSessionID)
		}
		result.Verdicts.CopyEntry(after.Verdicts, id, keep)
	}
	return ScopedProjection{Projection: result, selection: selection}
}

// followRecords brings a per-session map to after for sessions whose entry
// changed or whose record entered or left the scope. eq is T's PartialEq.
func followRecords[T any](scoped *core.SnapshotMap[string, T], before, after core.SnapshotMap[string, T], records core.SnapshotMap[string, core.SessionRecord], moved map[string]struct{}, eq func(a, b *T) bool) {
	changed := maps.Clone(moved)
	if changed == nil {
		changed = map[string]struct{}{}
	}
	for id := range before.Changes(after, eq) {
		changed[id] = struct{}{}
	}
	for _, id := range slices.Sorted(maps.Keys(changed)) {
		scoped.CopyEntry(after, id, records.ContainsKey(id))
	}
}

// RuntimeContainerCapacity is the daemon's hard container budget for one
// probed deployment host.
type RuntimeContainerCapacity struct {
	TargetIDs []string                   `json:"target_ids"`
	Capacity  core.HostContainerCapacity `json:"capacity"`
}

func (c RuntimeContainerCapacity) MarshalJSON() ([]byte, error) {
	type plain RuntimeContainerCapacity
	value := plain(c)
	if value.TargetIDs == nil {
		value.TargetIDs = []string{}
	}
	return feedMarshalJSON(value)
}

func (c *RuntimeContainerCapacity) UnmarshalJSON(data []byte) error {
	type plain RuntimeContainerCapacity
	return feedDecodeStruct(data, (*plain)(c), false, "target_ids", "capacity")
}

// NotificationCard is one session's notification card.
type NotificationCard struct {
	// SinceMs is the time of the oldest visible item.
	SinceMs int64                 `json:"since_ms"`
	Badge   core.NotificationKind `json:"badge"`
	// Entries is the visible entries, oldest first.
	Entries []core.NotificationStackEntry `json:"entries"`
	// Dismissal is nil when absent.
	Dismissal core.NotificationDismissal `json:"dismissal"`
	Holder    *string                    `json:"holder"`
	Form      *CardForm                  `json:"form"`
}

func (c NotificationCard) MarshalJSON() ([]byte, error) {
	type plain NotificationCard
	value := plain(c)
	if value.Entries == nil {
		value.Entries = []core.NotificationStackEntry{}
	}
	return feedMarshalJSON(value)
}

func (c *NotificationCard) UnmarshalJSON(data []byte) error {
	type plain NotificationCard
	var value plain
	aux := struct {
		*plain
		Dismissal json.RawMessage `json:"dismissal"`
	}{plain: &value}
	if err := feedDecodeStruct(data, &aux, false, "badge", "entries"); err != nil {
		return err
	}
	value.Dismissal = nil
	if len(aux.Dismissal) > 0 && string(aux.Dismissal) != "null" {
		dismissal, err := core.UnmarshalNotificationDismissal(aux.Dismissal)
		if err != nil {
			return err
		}
		value.Dismissal = dismissal
	}
	*c = NotificationCard(value)
	return nil
}

// ItemDestination: stack messages belong to the card session; only the
// live form uses its holder. item is the oldest-first entry index, with the
// form after the entries.
func (c *NotificationCard) ItemDestination(session string, item int) string {
	if c.Form != nil && item >= len(c.Entries) && c.Holder != nil {
		return *c.Holder
	}
	return session
}

// ItemIdentity is the durable notify-once key shared by the ledger and
// delayed alerts.
func (c *NotificationCard) ItemIdentity(session string, item int) (string, bool) {
	var identity []any
	switch {
	case item < len(c.Entries):
		entry := &c.Entries[item]
		switch message := entry.Message.(type) {
		case core.EntryMessageFound:
			identity = []any{"message", session, message.Value}
		case core.EntryMessageNone:
			identity = []any{"completion", session, entry.CompletedOrdinal}
		default:
			return "", false
		}
	case item == len(c.Entries):
		if c.Form == nil {
			return "", false
		}
		// An unknown raise time is not an identity: keying it with 0 would
		// make a just-answered form look like a new one and alert again.
		if c.Form.RaisedAtMs == 0 {
			return "", false
		}
		identity = []any{"form", c.ItemDestination(session, item), c.Form.RequestID, c.Form.RaisedAtMs}
	default:
		return "", false
	}
	encoded, err := jsonvalue.Marshal(identity)
	if err != nil {
		panic(fmt.Sprintf("a notification identity serializes: %v", err))
	}
	return string(encoded), true
}

// ShownQuestion is the newest shown stack question, even when a live form
// shares the card.
func (c *NotificationCard) ShownQuestion() *core.NotificationStackEntry {
	if _, stack := c.Dismissal.(core.NotificationDismissalStack); !stack {
		return nil
	}
	for i := len(c.Entries) - 1; i >= 0; i-- {
		if c.Entries[i].Kind == core.NotificationKindNeedsInput {
			return &c.Entries[i]
		}
	}
	return nil
}

// CardFormSource is a form's provenance, explicit because harness request
// IDs are opaque. Serialized snake_case; the zero value is the default,
// Unknown.
type CardFormSource int

const (
	// CardFormSourceUnknown: older publishers did not distinguish requests
	// from notification projections.
	CardFormSourceUnknown CardFormSource = iota
	CardFormSourceElicitation
	CardFormSourceNative
	CardFormSourceReviewFindings
)

var cardFormSourceNames = [...]string{"unknown", "elicitation", "native", "review_findings"}

func (s CardFormSource) MarshalText() ([]byte, error) {
	if s < 0 || int(s) >= len(cardFormSourceNames) {
		return nil, fmt.Errorf("unknown card form source %d", s)
	}
	return []byte(cardFormSourceNames[s]), nil
}

func (s *CardFormSource) UnmarshalText(text []byte) error {
	index := slices.Index(cardFormSourceNames[:], string(text))
	if index < 0 {
		return fmt.Errorf("unknown variant `%s`, expected one of `unknown`, `elicitation`, `native`, `review_findings`", text)
	}
	*s = CardFormSource(index)
	return nil
}

// CardForm is the live form a notification card offers.
type CardForm struct {
	Source     CardFormSource `json:"source"`
	RaisedAtMs int64          `json:"raised_at_ms"`
	RequestID  string         `json:"request_id"`
	Message    string         `json:"message"`
}

func (f *CardForm) UnmarshalJSON(data []byte) error {
	type plain CardForm
	return feedDecodeStruct(data, (*plain)(f), false, "request_id", "message")
}

func CardFormElicitation(request *core.ElicitationRequest, raisedAtMs int64) CardForm {
	return CardForm{
		Source:     CardFormSourceElicitation,
		RaisedAtMs: raisedAtMs,
		RequestID:  request.ID,
		Message:    request.Message,
	}
}

// CardFormNative: native input is a notification projection served through
// its owner's quick reply, rather than an elicitation on the owner chat.
func CardFormNative(source string, generation uint64, request string, raisedAtMs int64, message string) CardForm {
	return CardForm{
		Source:     CardFormSourceNative,
		RaisedAtMs: raisedAtMs,
		RequestID:  fmt.Sprintf("native.%d:%s.%d.%s", len(source), source, generation, request),
		Message:    message,
	}
}

func CardFormReviewFindings(reviewer string, raisedAtMs int64, message string) CardForm {
	return CardForm{
		Source:     CardFormSourceReviewFindings,
		RaisedAtMs: raisedAtMs,
		RequestID:  "review-verdict." + reviewer + ".findings",
		Message:    message,
	}
}

// RequiresElicitation: native input and completed findings are notification
// projections, rather than an elicitation that the content session's chat
// can answer.
func (f CardForm) RequiresElicitation() bool { return f.Source == CardFormSourceElicitation }

func (f CardForm) IsNative() bool { return f.Source == CardFormSourceNative }

// AlertEvent is a notify-once decision persisted alongside the Web Push
// item identity.
type AlertEvent struct {
	Sequence uint64 `json:"sequence"`
	// Identity is the durable item key, used to verify it is still
	// published after a delay.
	Identity     string                `json:"identity"`
	Destination  string                `json:"destination"`
	SessionTitle string                `json:"session_title"`
	Text         string                `json:"text"`
	Kind         core.NotificationKind `json:"kind"`
}

func (e *AlertEvent) UnmarshalJSON(data []byte) error {
	type plain AlertEvent
	return feedDecodeStruct(data, (*plain)(e), false, "sequence", "destination", "session_title", "text", "kind")
}

// RuntimeMetadata is configuration and bounded active-operation views,
// separate from history.
type RuntimeMetadata struct {
	// AlertEvents is the events after the requesting client cursor; empty on
	// initial attach.
	AlertEvents       []AlertEvent `json:"alert_events"`
	AlertHighSequence uint64       `json:"alert_high_sequence"`
	// NotificationCards is nil on daemons that do not publish cards.
	NotificationCards      *core.SnapshotMap[string, NotificationCard]          `json:"notification_cards,omitempty"`
	UnreadFrontiers        core.SnapshotMap[string, uint64]                     `json:"unread_frontiers"`
	NotificationDismissals core.SnapshotMap[string, core.NotificationDismissal] `json:"notification_dismissals"`
	// FormHolders is whose form the person answers for each row: the
	// session itself takes precedence over a descendant holding a
	// person-owned form.
	FormHolders         core.SnapshotMap[string, string]                      `json:"form_holders"`
	SessionAttention    core.SnapshotMap[string, core.SessionAttention]       `json:"session_attention"`
	OperatorPresence    core.OperatorPresence                                 `json:"operator_presence"`
	ProfileCapabilities core.SharedSnapshot[core.ProfileCapabilitiesSnapshot] `json:"profile_capabilities"`
	Config              core.SharedSnapshot[core.Config]                      `json:"config"`
	// Availability is the daemon's verdict on every profile and target.
	// Absent from a daemon that predates it, which reads as nothing known.
	Availability core.SharedSnapshot[RuntimeAvailability] `json:"availability"`
	// DefaultTargets is the ids in Config.Targets that the daemon supplied
	// as default candidates rather than the user's file naming them. Config
	// never serializes this set (it is not file content), so the feed
	// carries it beside the config. The daemon owns the origin; a client
	// installs it with InstalledConfig and never recomputes it. A daemon
	// that predates the field sends none, which leaves every target looking
	// user-written.
	DefaultTargets sortedStringSet `json:"default_targets"`
	// LastSubagentPolicies is also read from the legacy
	// `last_subagent_policy` (core.DeserializePolicyMap).
	LastSubagentPolicies map[string]core.SubagentPolicy                   `json:"last_subagent_policies"`
	WorkspaceNames       map[string]string                                `json:"workspace_names"`
	Workspaces           []core.WorkspaceRecord                           `json:"workspaces,omitempty"`
	ContainerSizes       core.SnapshotMap[string, core.HostContainerSize] `json:"container_sizes"`
	LastOracleChoices    map[string]bool                                  `json:"last_oracle_choices,omitempty"`
	// MountHistory is read by the browser directly from the capture. Native
	// paths may contain bytes JSON cannot represent, so it is never
	// serialized.
	MountHistory core.SnapshotMap[string, []string] `json:"-"`
	Lifecycles   []RuntimeLifecycleView             `json:"lifecycles"`
	Reviews      []RuntimeReviewView                `json:"reviews"`
	Notices      []RuntimeNotice                    `json:"notices"`
	// UIRequests is requests to change the dashboard made on the person's
	// behalf, newest last. Bounded like Notices; see ui_request.go.
	UIRequests []UiRequest `json:"ui_requests,omitempty"`
	// Quotas is the daemon's quota reports. The daemon is the only prober.
	Quotas core.SharedSnapshot[QuotaSnapshot] `json:"quotas"`
	// Storage is the free space on each target host, and whether it is
	// full. The daemon's storage board is the only judge; a daemon that
	// predates the field sends none.
	Storage core.SharedSnapshot[[]core.TargetStorageView] `json:"storage"`
	// HostCapacities is the CPU, memory and filesystem readings from the
	// daemon capacity service.
	HostCapacities core.SharedSnapshot[[]RuntimeHostCapacity] `json:"host_capacities"`
	// ContainerCapacities is the hard container headroom for each probed
	// deployment host. The daemon includes in-memory resume and move
	// reservations; terminal surfaces must not recompute this from records
	// they happen to have loaded.
	ContainerCapacities core.SharedSnapshot[map[string]RuntimeContainerCapacity] `json:"container_capacities"`
	// LaunchRecency is what new-session defaults are chosen from. The feed
	// carries only live sessions, but the defaults follow every session,
	// stopped ones too.
	LaunchRecency core.SharedSnapshot[[]LaunchRecency] `json:"launch_recency"`
	// RecentLaunch is the latest interactive launch on the daemon host,
	// independent of session history.
	RecentLaunch *core.RecentLaunch `json:"recent_launch"`
	// ContainerUpgradeSessions is the daemon's Restart verdict; surfaces
	// warn before replacing these containers.
	ContainerUpgradeSessions sortedStringSet `json:"container_upgrade_sessions"`
	// FailedLaunchRetries is the sessions whose stored launch request can be
	// retried through Resume.
	FailedLaunchRetries sortedStringSet `json:"failed_launch_retries,omitempty"`
}

// MarshalJSON writes the metadata as serde does: absent lists and maps as
// [] and {}, including inside the shared snapshots.
func (m RuntimeMetadata) MarshalJSON() ([]byte, error) {
	type plain RuntimeMetadata
	value := plain(m)
	if value.AlertEvents == nil {
		value.AlertEvents = []AlertEvent{}
	}
	if value.DefaultTargets == nil {
		value.DefaultTargets = sortedStringSet{}
	}
	if value.LastSubagentPolicies == nil {
		value.LastSubagentPolicies = map[string]core.SubagentPolicy{}
	}
	if value.WorkspaceNames == nil {
		value.WorkspaceNames = map[string]string{}
	}
	if value.Lifecycles == nil {
		value.Lifecycles = []RuntimeLifecycleView{}
	}
	if value.Reviews == nil {
		value.Reviews = []RuntimeReviewView{}
	}
	if value.Notices == nil {
		value.Notices = []RuntimeNotice{}
	}
	if value.ContainerUpgradeSessions == nil {
		value.ContainerUpgradeSessions = sortedStringSet{}
	}
	// An unset configuration is Rust's Default for SharedSnapshot<Config>:
	// Config::default().
	if value.Config.PtrEq(core.SharedSnapshot[core.Config]{}) {
		value.Config = core.NewSharedSnapshot(core.DefaultConfig())
	}
	if *value.Storage.Get() == nil {
		value.Storage = core.NewSharedSnapshot([]core.TargetStorageView{})
	}
	if *value.HostCapacities.Get() == nil {
		value.HostCapacities = core.NewSharedSnapshot([]RuntimeHostCapacity{})
	}
	if *value.ContainerCapacities.Get() == nil {
		value.ContainerCapacities = core.NewSharedSnapshot(map[string]RuntimeContainerCapacity{})
	}
	if *value.LaunchRecency.Get() == nil {
		value.LaunchRecency = core.NewSharedSnapshot([]LaunchRecency{})
	}
	encoded, err := feedMarshalJSON(value)
	if err != nil {
		return nil, err
	}
	return feedOmitEmptyMembers(encoded, "container_sizes")
}

// Equal is Rust's derived PartialEq: the serialized fields and the mount
// history serde skips.
func (m *RuntimeMetadata) Equal(other *RuntimeMetadata) bool {
	if !m.MountHistory.Equal(other.MountHistory, func(a, b *[]string) bool { return slices.Equal(*a, *b) }) {
		return false
	}
	if !m.Availability.Equal(other.Availability, func(a, b *RuntimeAvailability) bool {
		return feedJSONEqual(&a.Browser, &b.Browser)
	}) {
		return false
	}
	return feedJSONEqual(m, other)
}

// RuntimeHostCapacity is one host or fleet and whether the daemon owes it a
// manual refresh.
type RuntimeHostCapacity struct {
	Reading        core.HostCapacityRow        `json:"reading"`
	Refreshing     bool                        `json:"refreshing"`
	DiskTotalBytes *uint64                     `json:"disk_total_bytes,omitempty"`
	Last1h         *core.CapacityWindowSummary `json:"last_1h,omitempty"`
}

func (c *RuntimeHostCapacity) UnmarshalJSON(data []byte) error {
	type plain RuntimeHostCapacity
	return feedDecodeStruct(data, (*plain)(c), false, "reading", "refreshing")
}

// UnmarshalJSON applies serde's defaults, reads the policies through
// core.DeserializePolicyMap under either name, and the dismissals through
// their own decoder.
func (m *RuntimeMetadata) UnmarshalJSON(data []byte) error {
	type plain RuntimeMetadata
	var value plain
	aux := struct {
		*plain
		NotificationDismissals json.RawMessage `json:"notification_dismissals"`
		LastSubagentPolicies   json.RawMessage `json:"last_subagent_policies"`
		LastSubagentPolicy     json.RawMessage `json:"last_subagent_policy"`
	}{plain: &value}
	if err := feedDecodeStruct(data, &aux, false,
		"session_attention", "config", "workspace_names", "lifecycles", "reviews", "notices"); err != nil {
		return err
	}
	value.NotificationDismissals = core.NewSnapshotMap[string, core.NotificationDismissal]()
	if len(aux.NotificationDismissals) > 0 {
		var entries map[string]json.RawMessage
		if err := json.Unmarshal(aux.NotificationDismissals, &entries); err != nil {
			return err
		}
		if entries == nil {
			return errors.New("invalid type: null, expected a map")
		}
		for id, raw := range entries {
			dismissal, err := core.UnmarshalNotificationDismissal(raw)
			if err != nil {
				return err
			}
			value.NotificationDismissals.Insert(id, dismissal)
		}
	}
	policies := aux.LastSubagentPolicies
	if len(aux.LastSubagentPolicy) > 0 {
		if len(policies) > 0 {
			return errors.New("duplicate field `last_subagent_policies`")
		}
		policies = aux.LastSubagentPolicy
	}
	value.LastSubagentPolicies = nil
	if len(policies) > 0 {
		decoded, err := core.DeserializePolicyMap(policies)
		if err != nil {
			return err
		}
		value.LastSubagentPolicies = decoded
	}
	*m = RuntimeMetadata(value)
	return nil
}

// InstalledConfig is the configuration a client installs: the published
// config with the daemon's default-candidate set.
func (m *RuntimeMetadata) InstalledConfig() core.Config {
	config := core.DefaultConfig()
	if !m.Config.PtrEq(core.SharedSnapshot[core.Config]{}) {
		config = *m.Config.Get()
	}
	config.DefaultTargets = maps.Clone(map[string]struct{}(m.DefaultTargets))
	if config.DefaultTargets == nil {
		config.DefaultTargets = map[string]struct{}{}
	}
	return config
}

// LaunchRecency is the newest creation time among the sessions launched with
// one project, profile and target.
type LaunchRecency struct {
	BundleID         string `json:"bundle_id"`
	LastProfile      string `json:"last_profile"`
	TargetTemplateID string `json:"target_template_id"`
	NewestCreatedAt  string `json:"newest_created_at"`
}

func (r *LaunchRecency) UnmarshalJSON(data []byte) error {
	type plain LaunchRecency
	return feedDecodeStruct(data, (*plain)(r), false,
		"bundle_id", "last_profile", "target_template_id", "newest_created_at")
}

// LaunchRecencyOf is Rust's launch_recency: one LaunchRecency per project,
// profile and target that any session used. A creation time that does not
// parse ranks below every other.
func LaunchRecencyOf(records core.SnapshotMap[string, core.SessionRecord]) []LaunchRecency {
	// A creation time that does not parse is Rust's None, which orders
	// below every Some.
	created := func(record *core.SessionRecord) (int64, bool) {
		timestamp, err := time.Parse(time.RFC3339Nano, record.CreatedAt)
		if err != nil {
			return 0, false
		}
		return timestamp.UnixMilli(), true
	}
	type key struct{ bundle, profile, target string }
	newest := map[key]*core.SessionRecord{}
	for record := range records.Values() {
		k := key{record.BundleID, record.LastProfile, record.TargetTemplateID}
		held, found := newest[k]
		if !found {
			newest[k] = record
			continue
		}
		recordTime, recordOK := created(record)
		heldTime, heldOK := created(held)
		if recordOK && (!heldOK || recordTime > heldTime) {
			newest[k] = record
		}
	}
	keys := slices.SortedFunc(maps.Keys(newest), func(a, b key) int {
		return cmp.Or(strings.Compare(a.bundle, b.bundle), strings.Compare(a.profile, b.profile), strings.Compare(a.target, b.target))
	})
	recency := make([]LaunchRecency, 0, len(keys))
	for _, k := range keys {
		recency = append(recency, LaunchRecency{
			BundleID:         k.bundle,
			LastProfile:      k.profile,
			TargetTemplateID: k.target,
			NewestCreatedAt:  newest[k].CreatedAt,
		})
	}
	return recency
}

// TailKey is where an item sits in a transcript: its creating ordinal, then
// its stable id. The store reads transcripts in this order. It serializes as
// the Rust tuple does: [position, "stable_id"].
type TailKey struct {
	Position uint64
	StableID string
}

func (k TailKey) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON([]any{k.Position, k.StableID})
}

func (k *TailKey) UnmarshalJSON(data []byte) error {
	var parts []json.RawMessage
	if err := json.Unmarshal(data, &parts); err != nil {
		return err
	}
	if len(parts) != 2 {
		return fmt.Errorf("invalid length %d, expected a tuple of size 2", len(parts))
	}
	if err := json.Unmarshal(parts[0], &k.Position); err != nil {
		return err
	}
	return json.Unmarshal(parts[1], &k.StableID)
}

// TailMapKey is a TailKey encoded so that ordering the strings orders the
// keys (a fixed-width position, then the stable id): core.SnapshotMap needs
// an ordered key, and Go tuples are not.
type TailMapKey string

// tailPositionDigits is the width of u64::MAX in decimal.
const tailPositionDigits = 20

func (k TailKey) MapKey() TailMapKey {
	return TailMapKey(fmt.Sprintf("%0*d", tailPositionDigits, k.Position) + k.StableID)
}

func (k TailMapKey) TailKey() TailKey {
	position, err := strconv.ParseUint(string(k[:tailPositionDigits]), 10, 64)
	if err != nil {
		panic(fmt.Sprintf("malformed tail map key %q: %v", string(k), err))
	}
	return TailKey{Position: position, StableID: string(k[tailPositionDigits:])}
}

// SessionTail is the newest part of one session's transcript, as the daemon
// publishes it.
//
// Items are shared with the daemon's live projection: an item that did not
// change keeps its pointer from one version to the next, so comparing two
// versions, applying a change, and dropping an old version all cost what
// changed rather than the whole tail.
type SessionTail struct {
	// LatestUserPosition is the latest person-authored transcript position,
	// retained before trimming items or clearing the header. Clients consume
	// served attention, not this watermark.
	LatestUserPosition *uint64
	// Header is the session's projection without its transcript.
	Header core.MaterializedSession
	Window core.ProjectionWindow
	Items  core.SnapshotMap[TailMapKey, *core.TranscriptItem]
}

// SessionTailOf is the newest limit items of a published projection. window
// describes what materialized.Transcript already leaves out.
func SessionTailOf(materialized *core.MaterializedSession, window *core.ProjectionWindow, limit int) SessionTail {
	tail := SessionTail{
		Header: transcriptHeader(materialized),
		Window: *window,
	}
	if position, ok := materialized.LatestPersonPosition(); ok {
		tail.LatestUserPosition = &position
	}
	tail.Publish(materialized, window, limit)
	return tail
}

// Publish brings this tail to a newly published projection. Items whose
// pointer is unchanged are not touched; an item with a new pointer but equal
// content keeps the old one, so a projection reloaded from the store reports
// only the items whose content differs.
func (t *SessionTail) Publish(materialized *core.MaterializedSession, window *core.ProjectionWindow, limit int) {
	t.LatestUserPosition = nil
	if position, ok := materialized.LatestPersonPosition(); ok {
		t.LatestUserPosition = &position
	}
	start := max(len(materialized.Transcript)-limit, 0)
	// The daemon calls this for every view it publishes, so it walks the
	// held items and the published ones side by side in key order, and
	// allocates only for what changed. The published transcript is already
	// in this order but for ties, which the sort settles.
	kept := slices.Clone(materialized.Transcript[start:])
	slices.SortStableFunc(kept, func(left, right *core.TranscriptItem) int {
		return compareTailKeys(tailKey(left), tailKey(right))
	})
	held := slices.Collect(t.Items.Keys())
	next := 0
	var gone []TailMapKey
	var changed []*core.TranscriptItem
	for _, item := range kept {
		key := tailKey(item)
		for next < len(held) && compareTailKeys(held[next].TailKey(), key) < 0 {
			gone = append(gone, held[next])
			next++
		}
		if next < len(held) && compareTailKeys(held[next].TailKey(), key) == 0 {
			heldItem := t.Items.GetShared(held[next])
			next++
			// Pointer equality first, then contents.
			if *heldItem == item || transcriptItemEqual(*heldItem, item) {
				continue
			}
		}
		changed = append(changed, item)
	}
	gone = append(gone, held[next:]...)
	for _, key := range gone {
		t.Items.Remove(key)
	}
	for _, item := range changed {
		t.Items.Insert(tailKey(item).MapKey(), item)
	}
	if !sameHeader(&t.Header, materialized) {
		t.Header = transcriptHeader(materialized)
	}
	published := *window
	published.OmittedItems = window.OmittedItems + start
	if !projectionWindowEqual(&t.Window, &published) {
		t.Window = published
	}
}

// compareTailKeys orders tail keys as Rust orders the (u64, String) tuple.
func compareTailKeys(a, b TailKey) int {
	return cmp.Or(cmp.Compare(a.Position, b.Position), strings.Compare(a.StableID, b.StableID))
}

// projectionWindowEqual is ProjectionWindow's PartialEq.
func projectionWindowEqual(a, b *core.ProjectionWindow) bool {
	return quotaOptionalEqual(a.LatestUnreadEventOrdinal, b.LatestUnreadEventOrdinal) &&
		a.OmittedItems == b.OmittedItems &&
		quotaOptionalEqual(a.ProvisionalTitle, b.ProvisionalTitle) &&
		quotaOptionalEqual(a.LatestTurnStartPosition, b.LatestTurnStartPosition)
}

// Materialized is the projection this tail stands for, transcript included.
func (t *SessionTail) Materialized() core.MaterializedSession {
	materialized := t.Header
	materialized.Transcript = slices.Collect(t.itemValues())
	return materialized
}

// itemValues is the tail's items in key order.
func (t *SessionTail) itemValues() iter.Seq[*core.TranscriptItem] {
	return func(yield func(*core.TranscriptItem) bool) {
		for item := range t.Items.Values() {
			if !yield(*item) {
				return
			}
		}
	}
}

// ChangeFrom is what turns before into this tail. A receiver that holds
// nothing yet is never assumed by a delta: see TranscriptChangeRefetch.
func (t *SessionTail) ChangeFrom(before *SessionTail) TranscriptChange {
	var items TranscriptItems
	for key, entry := range before.Items.Changes(t.Items, transcriptItemPointerEqual) {
		if entry != nil {
			items.Upserts = append(items.Upserts, *entry)
		} else {
			items.Removes = append(items.Removes, key.TailKey())
		}
	}
	if !materializedEqual(&before.Header, &t.Header) {
		header := t.Header
		items.Header = &header
	}
	if !projectionWindowEqual(&before.Window, &t.Window) {
		window := t.Window
		items.Window = &window
	}
	return TranscriptChange{Kind: TranscriptChangeItems, Items: items}
}

// transcriptItemPointerEqual is Arc<TranscriptItem>'s PartialEq: the same
// pointer, or equal contents.
func transcriptItemPointerEqual(a, b **core.TranscriptItem) bool {
	return *a == *b || transcriptItemEqual(*a, *b)
}

// materializedEqual is MaterializedSession's PartialEq, read as the two
// projections' serialized forms.
func materializedEqual(a, b *core.MaterializedSession) bool {
	return feedJSONEqual(a, b)
}

func (t *SessionTail) apply(change TranscriptItems) {
	if change.Header != nil {
		t.Header = *change.Header
	}
	for _, key := range change.Removes {
		t.Items.Remove(key.MapKey())
	}
	for _, item := range change.Upserts {
		t.Items.Insert(tailKey(item).MapKey(), item)
	}
	if change.Window != nil {
		t.Window = *change.Window
	}
}

// sessionTailEqual is SessionTail's PartialEq.
func sessionTailEqual(a, b *SessionTail) bool {
	return quotaOptionalEqual(a.LatestUserPosition, b.LatestUserPosition) &&
		materializedEqual(&a.Header, &b.Header) &&
		projectionWindowEqual(&a.Window, &b.Window) &&
		a.Items.Equal(b.Items, transcriptItemPointerEqual)
}

func tailKey(item *core.TranscriptItem) TailKey {
	return TailKey{Position: item.Position, StableID: item.StableID}
}

// transcriptHeader is the parts of a projection a tail keeps besides its
// items.
func transcriptHeader(materialized *core.MaterializedSession) core.MaterializedSession {
	header := *materialized
	header.Transcript = nil
	return header
}

func sameHeader(header, materialized *core.MaterializedSession) bool {
	left, right := *header, *materialized
	left.Transcript, right.Transcript = nil, nil
	return materializedEqual(&left, &right)
}

// SessionTailReplyKind is which answer a tail request got.
type SessionTailReplyKind int

const (
	SessionTailReplyTail SessionTailReplyKind = iota
	// SessionTailReplyNoTail: the session has no projection at that cursor.
	SessionTailReplyNoTail
	// SessionTailReplyResetRequired: the daemon no longer holds that cursor;
	// take a new snapshot first.
	SessionTailReplyResetRequired
)

// SessionTailReply is a session's whole tail at one feed cursor, answered on
// request. Internally tagged on `kind`, snake_case. One struct with a kind:
// Header, Window and Items belong to Tail.
type SessionTailReply struct {
	Kind   SessionTailReplyKind
	Header *core.MaterializedSession
	Window core.ProjectionWindow
	// Items is oldest first.
	Items []*core.TranscriptItem
}

// SessionTailReplyOf is Rust's SessionTailReply::of.
func SessionTailReplyOf(tail *SessionTail) SessionTailReply {
	header := tail.Header
	items := slices.Collect(tail.itemValues())
	if items == nil {
		items = []*core.TranscriptItem{}
	}
	return SessionTailReply{Kind: SessionTailReplyTail, Header: &header, Window: tail.Window, Items: items}
}

func (r SessionTailReply) MarshalJSON() ([]byte, error) {
	switch r.Kind {
	case SessionTailReplyTail:
		if r.Header == nil {
			return nil, errors.New("a tail reply has no header")
		}
		header, err := serializeClientProjection(r.Header)
		if err != nil {
			return nil, err
		}
		items := r.Items
		if items == nil {
			items = []*core.TranscriptItem{}
		}
		return feedMarshalJSON(struct {
			Kind   string                 `json:"kind"`
			Header json.RawMessage        `json:"header"`
			Window core.ProjectionWindow  `json:"window"`
			Items  []*core.TranscriptItem `json:"items"`
		}{"tail", header, r.Window, items})
	case SessionTailReplyNoTail:
		return []byte(`{"kind":"no_tail"}`), nil
	case SessionTailReplyResetRequired:
		return []byte(`{"kind":"reset_required"}`), nil
	}
	return nil, fmt.Errorf("unknown session tail reply kind %d", r.Kind)
}

func (r *SessionTailReply) UnmarshalJSON(data []byte) error {
	kind, err := feedTag(data, "kind")
	if err != nil {
		return err
	}
	switch kind {
	case "tail":
		var value struct {
			Header *core.MaterializedSession `json:"header"`
			Window core.ProjectionWindow     `json:"window"`
			Items  []*core.TranscriptItem    `json:"items"`
		}
		if err := feedDecodeStruct(data, &value, false, "header", "window", "items"); err != nil {
			return err
		}
		*r = SessionTailReply{Kind: SessionTailReplyTail, Header: value.Header, Window: value.Window, Items: value.Items}
	case "no_tail":
		*r = SessionTailReply{Kind: SessionTailReplyNoTail}
	case "reset_required":
		*r = SessionTailReply{Kind: SessionTailReplyResetRequired}
	default:
		return fmt.Errorf("unknown variant `%s`, expected one of `tail`, `no_tail`, `reset_required`", kind)
	}
	return nil
}

// feedTag is the string tag of an internally tagged enum.
func feedTag(data []byte, tag string) (string, error) {
	fields, err := feedObjectFields(data)
	if err != nil {
		return "", err
	}
	raw, ok := fields[tag]
	if !ok {
		return "", fmt.Errorf("missing field `%s`", tag)
	}
	var name string
	if err := json.Unmarshal(raw, &name); err != nil {
		return "", err
	}
	return name, nil
}

// SessionTailFromParts is a tail received whole, keeping each item's
// pointer.
func SessionTailFromParts(header core.MaterializedSession, window core.ProjectionWindow, items []*core.TranscriptItem) SessionTail {
	var tail SessionTail
	for _, item := range items {
		tail.Items.Insert(tailKey(item).MapKey(), item)
	}
	tail.Header = header
	tail.Window = window
	return tail
}

// TranscriptItems is the item changes to one session's tail between two feed
// versions.
type TranscriptItems struct {
	// Header is serialized through serializeClientProjection.
	Header  *core.MaterializedSession
	Window  *core.ProjectionWindow
	Upserts []*core.TranscriptItem
	Removes []TailKey
}

func (i TranscriptItems) MarshalJSON() ([]byte, error) {
	var header json.RawMessage
	if i.Header != nil {
		var err error
		if header, err = serializeClientProjection(i.Header); err != nil {
			return nil, err
		}
	}
	return feedMarshalJSON(struct {
		Header  json.RawMessage        `json:"header,omitempty"`
		Window  *core.ProjectionWindow `json:"window,omitempty"`
		Upserts []*core.TranscriptItem `json:"upserts,omitempty"`
		Removes []TailKey              `json:"removes,omitempty"`
	}{header, i.Window, i.Upserts, i.Removes})
}

func (i *TranscriptItems) UnmarshalJSON(data []byte) error {
	var value struct {
		Header  *core.MaterializedSession `json:"header"`
		Window  *core.ProjectionWindow    `json:"window"`
		Upserts []*core.TranscriptItem    `json:"upserts"`
		Removes []TailKey                 `json:"removes"`
	}
	if err := feedDecodeStruct(data, &value, false); err != nil {
		return err
	}
	*i = TranscriptItems{Header: value.Header, Window: value.Window, Upserts: value.Upserts, Removes: value.Removes}
	return nil
}

// TranscriptChangeKind is what a delta says about one session's transcript
// tail.
type TranscriptChangeKind int

const (
	// TranscriptChangeItems: apply Items to the tail held at the delta's
	// starting cursor.
	TranscriptChangeItems TranscriptChangeKind = iota
	// TranscriptChangeRefetch: the change was too large to send: drop the
	// held tail and fetch it again.
	TranscriptChangeRefetch
	// TranscriptChangeRemoved: the session no longer publishes a tail.
	TranscriptChangeRemoved
)

// TranscriptChange is internally tagged on `kind`, snake_case; Items's
// fields follow the tag. One struct with a kind: Items belongs to Items.
type TranscriptChange struct {
	Kind  TranscriptChangeKind
	Items TranscriptItems
}

func (c TranscriptChange) MarshalJSON() ([]byte, error) {
	switch c.Kind {
	case TranscriptChangeItems:
		items, err := c.Items.MarshalJSON()
		if err != nil {
			return nil, err
		}
		if string(items) == "{}" {
			return []byte(`{"kind":"items"}`), nil
		}
		return append([]byte(`{"kind":"items",`), items[1:]...), nil
	case TranscriptChangeRefetch:
		return []byte(`{"kind":"refetch"}`), nil
	case TranscriptChangeRemoved:
		return []byte(`{"kind":"removed"}`), nil
	}
	return nil, fmt.Errorf("unknown transcript change kind %d", c.Kind)
}

func (c *TranscriptChange) UnmarshalJSON(data []byte) error {
	kind, err := feedTag(data, "kind")
	if err != nil {
		return err
	}
	switch kind {
	case "items":
		var items TranscriptItems
		if err := items.UnmarshalJSON(data); err != nil {
			return err
		}
		*c = TranscriptChange{Kind: TranscriptChangeItems, Items: items}
	case "refetch":
		*c = TranscriptChange{Kind: TranscriptChangeRefetch}
	case "removed":
		*c = TranscriptChange{Kind: TranscriptChangeRemoved}
	default:
		return fmt.Errorf("unknown variant `%s`, expected one of `items`, `refetch`, `removed`", kind)
	}
	return nil
}

func (c TranscriptChange) IsEmpty() bool {
	return c.Kind == TranscriptChangeItems && c.Items.Header == nil && c.Items.Window == nil &&
		len(c.Items.Upserts) == 0 && len(c.Items.Removes) == 0
}

// SessionCpuViewState is which answer a session's CPU view holds.
type SessionCpuViewState int

const (
	SessionCpuViewMeasured SessionCpuViewState = iota
	SessionCpuViewUnavailable
)

// SessionCpuView is internally tagged on `state`, snake_case. One struct
// with a kind: Usage belongs to Measured, Reason to Unavailable.
type SessionCpuView struct {
	State  SessionCpuViewState
	Usage  core.SessionCpuUsage
	Reason string
}

func (v SessionCpuView) MarshalJSON() ([]byte, error) {
	switch v.State {
	case SessionCpuViewMeasured:
		return feedMarshalJSON(struct {
			State string               `json:"state"`
			Usage core.SessionCpuUsage `json:"usage"`
		}{"measured", v.Usage})
	case SessionCpuViewUnavailable:
		return feedMarshalJSON(struct {
			State  string `json:"state"`
			Reason string `json:"reason"`
		}{"unavailable", v.Reason})
	}
	return nil, fmt.Errorf("unknown session CPU view state %d", v.State)
}

func (v *SessionCpuView) UnmarshalJSON(data []byte) error {
	state, err := feedTag(data, "state")
	if err != nil {
		return err
	}
	switch state {
	case "measured":
		var value struct {
			Usage core.SessionCpuUsage `json:"usage"`
		}
		if err := feedDecodeStruct(data, &value, false, "usage"); err != nil {
			return err
		}
		*v = SessionCpuView{State: SessionCpuViewMeasured, Usage: value.Usage}
	case "unavailable":
		var value struct {
			Reason string `json:"reason"`
		}
		if err := feedDecodeStruct(data, &value, false, "reason"); err != nil {
			return err
		}
		*v = SessionCpuView{State: SessionCpuViewUnavailable, Reason: value.Reason}
	default:
		return fmt.Errorf("unknown variant `%s`, expected `measured` or `unavailable`", state)
	}
	return nil
}

// KeyChange is one (String, Option<T>) of a KeyChanges: the record at ID
// now, or nil when it was removed. It serializes as the tuple:
// ["id", value-or-null].
type KeyChange[T any] struct {
	ID    string
	Value *T
}

func (c KeyChange[T]) MarshalJSON() ([]byte, error) {
	var value any
	if c.Value != nil {
		value = c.Value
	}
	return feedMarshalJSON([]any{c.ID, value})
}

func (c *KeyChange[T]) UnmarshalJSON(data []byte) error {
	var parts []json.RawMessage
	if err := json.Unmarshal(data, &parts); err != nil {
		return err
	}
	if len(parts) != 2 {
		return fmt.Errorf("invalid length %d, expected a tuple of size 2", len(parts))
	}
	if err := json.Unmarshal(parts[0], &c.ID); err != nil {
		return err
	}
	c.Value = nil
	if string(bytes.TrimSpace(parts[1])) == "null" {
		return nil
	}
	var value T
	if err := json.Unmarshal(parts[1], &value); err != nil {
		return err
	}
	c.Value = &value
	return nil
}

// KeyChanges is Rust's Vec<(String, Option<T>)>.
type KeyChanges[T any] = []KeyChange[T]

// KeyedTranscriptChange is one (String, TranscriptChange) of
// RuntimeDelta.Transcripts, serialized as the tuple.
type KeyedTranscriptChange struct {
	ID     string
	Change TranscriptChange
}

func (c KeyedTranscriptChange) MarshalJSON() ([]byte, error) {
	return feedMarshalJSON([]any{c.ID, c.Change})
}

func (c *KeyedTranscriptChange) UnmarshalJSON(data []byte) error {
	var parts []json.RawMessage
	if err := json.Unmarshal(data, &parts); err != nil {
		return err
	}
	if len(parts) != 2 {
		return fmt.Errorf("invalid length %d, expected a tuple of size 2", len(parts))
	}
	if err := json.Unmarshal(parts[0], &c.ID); err != nil {
		return err
	}
	return json.Unmarshal(parts[1], &c.Change)
}

// RuntimeDelta is what changed between two feed versions.
type RuntimeDelta struct {
	Revision          *uint64                             `json:"revision"`
	Records           KeyChanges[core.SessionRecord]      `json:"records"`
	EnvironmentOwners KeyChanges[string]                  `json:"environment_owners,omitempty"`
	Subagents         KeyChanges[core.SubagentMetadata]   `json:"subagents"`
	Sessions          KeyChanges[RuntimeSessionView]      `json:"sessions"`
	Verdicts          KeyChanges[SessionVerdict]          `json:"verdicts"`
	Moves             KeyChanges[core.MoveOperation]      `json:"moves"`
	NativeAgents      KeyChanges[core.NativeAgentSummary] `json:"native_agents"`
	SessionCPU        KeyChanges[SessionCpuView]          `json:"session_cpu"`
	Metadata          *RuntimeMetadata                    `json:"metadata"`
	Transcripts       []KeyedTranscriptChange             `json:"transcripts,omitempty"`
}

func (d RuntimeDelta) MarshalJSON() ([]byte, error) {
	type plain RuntimeDelta
	value := plain(d)
	if value.Records == nil {
		value.Records = KeyChanges[core.SessionRecord]{}
	}
	if value.Verdicts == nil {
		value.Verdicts = KeyChanges[SessionVerdict]{}
	}
	if value.Subagents == nil {
		value.Subagents = KeyChanges[core.SubagentMetadata]{}
	}
	if value.Sessions == nil {
		value.Sessions = KeyChanges[RuntimeSessionView]{}
	}
	if value.Moves == nil {
		value.Moves = KeyChanges[core.MoveOperation]{}
	}
	if value.NativeAgents == nil {
		value.NativeAgents = KeyChanges[core.NativeAgentSummary]{}
	}
	if value.SessionCPU == nil {
		value.SessionCPU = KeyChanges[SessionCpuView]{}
	}
	return feedMarshalJSON(value)
}

func (d *RuntimeDelta) UnmarshalJSON(data []byte) error {
	type plain RuntimeDelta
	var value plain
	if err := feedDecodeStruct(data, &value, false,
		"records", "subagents", "sessions", "moves", "native_agents"); err != nil {
		return err
	}
	*d = RuntimeDelta(value)
	return nil
}

// RuntimeFrameKind is which publication a frame carries.
type RuntimeFrameKind int

const (
	RuntimeFrameSnapshot RuntimeFrameKind = iota
	RuntimeFrameDelta
	RuntimeFrameResetRequired
)

// RuntimeFrame is one publication of the feed. Externally tagged, as serde
// writes an enum without attributes: {"Snapshot":{...}}, {"Delta":{...}} or
// "ResetRequired". One struct with a kind: Cursor belongs to Snapshot and
// Delta, Projection to Snapshot, From and Changes to Delta.
type RuntimeFrame struct {
	Kind       RuntimeFrameKind
	Cursor     RuntimeCursor
	Projection *RuntimeProjection
	From       RuntimeCursor
	Changes    *RuntimeDelta
}

func (f RuntimeFrame) MarshalJSON() ([]byte, error) {
	switch f.Kind {
	case RuntimeFrameSnapshot:
		if f.Projection == nil {
			return nil, errors.New("a snapshot frame has no projection")
		}
		return feedMarshalJSON(map[string]any{"Snapshot": struct {
			Cursor     RuntimeCursor      `json:"cursor"`
			Projection *RuntimeProjection `json:"projection"`
		}{f.Cursor, f.Projection}})
	case RuntimeFrameDelta:
		if f.Changes == nil {
			return nil, errors.New("a delta frame has no changes")
		}
		return feedMarshalJSON(map[string]any{"Delta": struct {
			From    RuntimeCursor `json:"from"`
			Cursor  RuntimeCursor `json:"cursor"`
			Changes *RuntimeDelta `json:"changes"`
		}{f.From, f.Cursor, f.Changes}})
	case RuntimeFrameResetRequired:
		return []byte(`"ResetRequired"`), nil
	}
	return nil, fmt.Errorf("unknown runtime frame kind %d", f.Kind)
}

func (f *RuntimeFrame) UnmarshalJSON(data []byte) error {
	var unit string
	if json.Unmarshal(data, &unit) == nil {
		if unit != "ResetRequired" {
			return fmt.Errorf("unknown variant `%s`, expected one of `Snapshot`, `Delta`, `ResetRequired`", unit)
		}
		*f = RuntimeFrame{Kind: RuntimeFrameResetRequired}
		return nil
	}
	fields, err := feedObjectFields(data)
	if err != nil {
		return err
	}
	if len(fields) != 1 {
		return errors.New("invalid value: map, expected map with a single key")
	}
	for tag, content := range fields {
		switch tag {
		case "Snapshot":
			var value struct {
				Cursor     RuntimeCursor      `json:"cursor"`
				Projection *RuntimeProjection `json:"projection"`
			}
			if err := feedDecodeStruct(content, &value, false, "cursor", "projection"); err != nil {
				return err
			}
			*f = RuntimeFrame{Kind: RuntimeFrameSnapshot, Cursor: value.Cursor, Projection: value.Projection}
		case "Delta":
			var value struct {
				From    RuntimeCursor `json:"from"`
				Cursor  RuntimeCursor `json:"cursor"`
				Changes *RuntimeDelta `json:"changes"`
			}
			if err := feedDecodeStruct(content, &value, false, "from", "cursor", "changes"); err != nil {
				return err
			}
			*f = RuntimeFrame{Kind: RuntimeFrameDelta, From: value.From, Cursor: value.Cursor, Changes: value.Changes}
		case "ResetRequired":
			*f = RuntimeFrame{Kind: RuntimeFrameResetRequired}
		default:
			return fmt.Errorf("unknown variant `%s`, expected one of `Snapshot`, `Delta`, `ResetRequired`", tag)
		}
	}
	return nil
}

// RuntimeDeltaBetween is Rust's RuntimeDelta::between.
func RuntimeDeltaBetween(before, after *RuntimeProjection) RuntimeDelta {
	delta := RuntimeDelta{
		Records:           changes(before.Records, after.Records, feedJSONEqual[core.SessionRecord]),
		EnvironmentOwners: changes(before.EnvironmentOwners, after.EnvironmentOwners, feedComparableEqual[string]),
		Subagents:         changes(before.Subagents, after.Subagents, feedJSONEqual[core.SubagentMetadata]),
		Sessions:          changes(before.Sessions, after.Sessions, runtimeSessionViewEqual),
		Verdicts:          changes(before.Verdicts, after.Verdicts, sessionVerdictEqual),
		Moves:             changes(before.Moves, after.Moves, feedJSONEqual[core.MoveOperation]),
		NativeAgents:      changes(before.NativeAgents, after.NativeAgents, feedJSONEqual[core.NativeAgentSummary]),
		SessionCPU:        changes(before.SessionCPU, after.SessionCPU, feedJSONEqual[SessionCpuView]),
	}
	if before.Revision != after.Revision {
		revision := after.Revision
		delta.Revision = &revision
	}
	if !before.Metadata.Equal(&after.Metadata) {
		metadata := after.Metadata
		delta.Metadata = &metadata
	}
	for id, tail := range before.Transcripts.Changes(after.Transcripts, sessionTailEqual) {
		var change TranscriptChange
		held := before.Transcripts.GetShared(id)
		switch {
		case tail != nil && held != nil:
			change = tail.ChangeFrom(held)
		case tail != nil:
			// A tail the receiver cannot hold yet is fetched, not sent whole
			// inside a delta.
			change = TranscriptChange{Kind: TranscriptChangeRefetch}
		default:
			change = TranscriptChange{Kind: TranscriptChangeRemoved}
		}
		delta.Transcripts = append(delta.Transcripts, KeyedTranscriptChange{ID: id, Change: change})
	}
	return delta
}

func (d *RuntimeDelta) IsEmpty() bool {
	return d.Revision == nil && len(d.Records) == 0 && len(d.EnvironmentOwners) == 0 &&
		len(d.Subagents) == 0 && len(d.Sessions) == 0 && len(d.Verdicts) == 0 &&
		len(d.Moves) == 0 && len(d.NativeAgents) == 0 && len(d.SessionCPU) == 0 &&
		d.Metadata == nil && len(d.Transcripts) == 0
}

func (d *RuntimeDelta) apply(projection *RuntimeProjection) {
	if d.Revision != nil {
		projection.Revision = *d.Revision
	}
	applyKeys(&projection.Records, d.Records)
	applyKeys(&projection.EnvironmentOwners, d.EnvironmentOwners)
	applyKeys(&projection.Subagents, d.Subagents)
	applyKeys(&projection.Sessions, d.Sessions)
	applyKeys(&projection.Verdicts, d.Verdicts)
	applyKeys(&projection.Moves, d.Moves)
	applyKeys(&projection.NativeAgents, d.NativeAgents)
	applyKeys(&projection.SessionCPU, d.SessionCPU)
	if d.Metadata != nil {
		projection.Metadata = *d.Metadata
	}
	// Only the tails this receiver holds are followed; the others are
	// fetched when needed, at a cursor of their own.
	for _, entry := range d.Transcripts {
		switch entry.Change.Kind {
		case TranscriptChangeItems:
			if tail := projection.Transcripts.GetMut(entry.ID); tail != nil {
				tail.apply(entry.Change.Items)
			}
		default:
			projection.Transcripts.Remove(entry.ID)
		}
	}
}

// changes is what turns before into after, record by record; eq is T's
// PartialEq.
func changes[T any](before, after core.SnapshotMap[string, T], eq func(a, b *T) bool) KeyChanges[T] {
	var result KeyChanges[T]
	for id, value := range before.Changes(after, eq) {
		change := KeyChange[T]{ID: id}
		if value != nil {
			copied := *value
			change.Value = &copied
		}
		result = append(result, change)
	}
	return result
}

func applyKeys[T any](m *core.SnapshotMap[string, T], changes KeyChanges[T]) {
	for _, change := range changes {
		if change.Value != nil {
			m.Insert(change.ID, *change.Value)
		} else {
			m.Remove(change.ID)
		}
	}
}

// RuntimeReplica is a client's copy of the feed.
type RuntimeReplica struct {
	Cursor     *RuntimeCursor
	Projection RuntimeProjection
}

// Apply validates the entire cursor before applying any part of a
// publication.
func (r *RuntimeReplica) Apply(frame RuntimeFrame) (bool, error) {
	retainAlerts := func(after, through uint64) {
		r.Projection.Metadata.AlertEvents = slices.DeleteFunc(r.Projection.Metadata.AlertEvents, func(event AlertEvent) bool {
			return !(event.Sequence > after && event.Sequence <= through)
		})
	}
	switch frame.Kind {
	case RuntimeFrameSnapshot:
		acknowledged := r.Projection.Metadata.AlertHighSequence
		if r.Cursor != nil {
			acknowledged = r.Cursor.AlertSequence
		}
		r.Projection = *frame.Projection
		retainAlerts(acknowledged, frame.Cursor.AlertSequence)
		cursor := frame.Cursor
		r.Cursor = &cursor
		return true, nil
	case RuntimeFrameDelta:
		from, cursor := frame.From, frame.Cursor
		if r.Cursor != nil && r.Cursor.Equal(cursor) {
			r.Projection.Metadata.AlertEvents = nil
			return false, nil
		}
		if r.Cursor == nil || !r.Cursor.Equal(from) {
			return false, errors.New("runtime cursor gap")
		}
		if !(from.Incarnation == cursor.Incarnation && (from.Sequence < cursor.Sequence ||
			(from.Sequence == cursor.Sequence && from.AlertSequence < cursor.AlertSequence))) {
			return false, errors.New("invalid runtime cursor transition")
		}
		r.Projection.Metadata.AlertEvents = nil
		frame.Changes.apply(&r.Projection)
		retainAlerts(from.AlertSequence, cursor.AlertSequence)
		r.Cursor = &cursor
		return true, nil
	}
	r.Projection.Metadata.AlertEvents = nil
	r.Cursor = nil
	return false, nil
}

// serializeClientProjection writes projection without the internal
// `answered_through` of its last turn outcome: older clients strictly decode
// the shared internal outcome inside projections.
func serializeClientProjection(projection any) ([]byte, error) {
	encoded, err := feedMarshalJSON(projection)
	if err != nil {
		return nil, err
	}
	value, err := jsonvalue.Parse(encoded)
	if err != nil {
		return nil, err
	}
	object, ok := value.(jsonvalue.Object)
	if !ok {
		return encoded, nil
	}
	outcome, ok := object.Get("last_turn_outcome")
	if outcomeObject, isObject := outcome.(jsonvalue.Object); ok && isObject {
		outcomeObject = outcomeObject.Clone()
		// serde_json's Map::remove with preserve_order is a swap remove.
		outcomeObject.SwapDelete("answered_through")
		object.Set("last_turn_outcome", outcomeObject)
	}
	return jsonvalue.Marshal(object)
}
