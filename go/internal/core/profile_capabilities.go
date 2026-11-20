// Port of mj-core/src/profile_capabilities.rs.

// Read-only publications from the daemon's single capability cache.

package core

import (
	"encoding/json"
	"fmt"
)

// CapabilityStateKind is the variant of a CapabilityState. The zero value is
// Pending, the default.
type CapabilityStateKind int

const (
	CapabilityStatePending CapabilityStateKind = iota
	CapabilityStateReady
	CapabilityStateFailed
)

// CapabilityState is Rust's `CapabilityState<T>`, adjacently tagged as
// {"status": "pending"|"ready"|"failed", "value": ...}. Value is set only
// for Ready and Error only for Failed.
type CapabilityState[T any] struct {
	Kind  CapabilityStateKind
	Value T
	Error string
}

func (s CapabilityState[T]) MarshalJSON() ([]byte, error) {
	switch s.Kind {
	case CapabilityStatePending:
		return []byte(`{"status":"pending"}`), nil
	case CapabilityStateReady:
		return json.Marshal(struct {
			Status string `json:"status"`
			Value  T      `json:"value"`
		}{"ready", s.Value})
	case CapabilityStateFailed:
		return json.Marshal(struct {
			Status string `json:"status"`
			Value  string `json:"value"`
		}{"failed", s.Error})
	}
	return nil, fmt.Errorf("invalid capability state %d", int(s.Kind))
}

func (s *CapabilityState[T]) UnmarshalJSON(data []byte) error {
	var wire struct {
		Status *string         `json:"status"`
		Value  json.RawMessage `json:"value"`
	}
	if err := json.Unmarshal(data, &wire); err != nil {
		return err
	}
	if wire.Status == nil {
		return fmt.Errorf("missing field `status`")
	}
	switch *wire.Status {
	case "pending":
		*s = CapabilityState[T]{Kind: CapabilityStatePending}
	case "ready":
		if wire.Value == nil {
			return fmt.Errorf("missing field `value`")
		}
		var value T
		if err := json.Unmarshal(wire.Value, &value); err != nil {
			return err
		}
		*s = CapabilityState[T]{Kind: CapabilityStateReady, Value: value}
	case "failed":
		if wire.Value == nil {
			return fmt.Errorf("missing field `value`")
		}
		var message string
		if err := json.Unmarshal(wire.Value, &message); err != nil {
			return err
		}
		*s = CapabilityState[T]{Kind: CapabilityStateFailed, Error: message}
	default:
		return fmt.Errorf("unknown variant `%s`, expected one of `pending`, `ready`, `failed`", *wire.Status)
	}
	return nil
}

type ProfileCapabilities struct {
	Choices CapabilityState[ProfileConfig]                    `json:"choices"`
	Efforts map[string]CapabilityState[[]SessionConfigChoice] `json:"efforts"`
}

func (c ProfileCapabilities) MarshalJSON() ([]byte, error) {
	type plain ProfileCapabilities
	value := plain(c)
	value.Efforts = workerLaunchNonNilMap(value.Efforts)
	return json.Marshal(value)
}

// ProfileCapabilitiesSnapshot: entries are keyed by opaque
// configured-installation identities. This also lets unsaved drafts share
// entries without changing the live profile list.
type ProfileCapabilitiesSnapshot struct {
	Profiles map[string]ProfileCapabilities `json:"profiles"`
}

func (s ProfileCapabilitiesSnapshot) MarshalJSON() ([]byte, error) {
	type plain ProfileCapabilitiesSnapshot
	value := plain(s)
	value.Profiles = workerLaunchNonNilMap(value.Profiles)
	return json.Marshal(value)
}

// Options: completed profiles contribute choices while pending profiles
// remain unknown. Nil means no profile has completed discovery yet.
func (s *ProfileCapabilitiesSnapshot) Options(config *Config, model *string) *SubagentOptions {
	panic("unported: mj-core/src/profile_capabilities.rs ProfileCapabilitiesSnapshot::options")
}

// ModelOffers is the models completed enabled profiles offer, one offer per
// value, with usagePriced saying which profiles are billed per use.
func (s *ProfileCapabilitiesSnapshot) ModelOffers(config *Config, usagePriced func(string) bool) []ReviewAutoconfigureModelOffer {
	panic("unported: mj-core/src/profile_capabilities.rs ProfileCapabilitiesSnapshot::model_offers")
}

// EffortBlockers is why model's efforts are not fully known: a profile that
// has not finished discovery (it may offer the model), or one that offers the
// model whose effort discovery is pending or failed. A profile whose own
// discovery failed is not listed: nothing shows that it offers the model.
func (s *ProfileCapabilitiesSnapshot) EffortBlockers(config *Config, model string) []string {
	panic("unported: mj-core/src/profile_capabilities.rs ProfileCapabilitiesSnapshot::effort_blockers")
}

// ModelID resolves once over the ready enabled profiles in this publication.
func (s *ProfileCapabilitiesSnapshot) ModelID(config *Config, wanted string) string {
	panic("unported: mj-core/src/profile_capabilities.rs ProfileCapabilitiesSnapshot::model_id")
}

// ForConfig limits a public projection to saved enabled profiles. Draft
// identities are controller-private; environment sources are never
// published.
func (s *ProfileCapabilitiesSnapshot) ForConfig(config *Config) ProfileCapabilitiesSnapshot {
	panic("unported: mj-core/src/profile_capabilities.rs ProfileCapabilitiesSnapshot::for_config")
}

// Covers reports whether every enabled profile in config has ready model
// choices and every model config selects on it has ready effort choices.
func (s *ProfileCapabilitiesSnapshot) Covers(config *Config) bool {
	panic("unported: mj-core/src/profile_capabilities.rs ProfileCapabilitiesSnapshot::covers")
}

func mergeEfforts(into *[]SessionConfigChoice, choices []SessionConfigChoice) {
	for _, choice := range choices {
		present := false
		for _, existing := range *into {
			if existing.Value == choice.Value {
				present = true
				break
			}
		}
		if !present {
			*into = append(*into, choice)
		}
	}
}
