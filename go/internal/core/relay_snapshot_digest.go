// Port of mj-core/src/relay/snapshot/digest.rs.
//
// The relay event digest chain and the validation that checks it.

package core

import (
	"crypto/sha256"
	"encoding/json"
	"errors"
	"fmt"
)

// relayEventDigestPayload is the v1 digest payload: it folds previous_digest
// into the hash (the chain link). Its exact field order and tags are
// load-bearing — changing them would invalidate every stored v1 digest.
type relayEventDigestPayload struct {
	Ordinal        uint64  `json:"ordinal"`
	PreviousDigest string  `json:"previous_digest"`
	RecordedAtMs   int64   `json:"recorded_at_ms"`
	CommandID      *string `json:"command_id,omitempty"`
	Observation    any     `json:"observation"`
}

// relayEventDigestPayloadV2 is identical to v1 but with no previous_digest,
// so the digest depends only on the record's own content.
type relayEventDigestPayloadV2 struct {
	Ordinal      uint64  `json:"ordinal"`
	RecordedAtMs int64   `json:"recorded_at_ms"`
	CommandID    *string `json:"command_id,omitempty"`
	Observation  any     `json:"observation"`
}

// legacyFlaggedObservationSessionOpened is the session_opened encoding
// written by builds 0f070506 through e6ed54ed on 2026-09-15, which
// serialized native_continuity_lost even when false. Records from that
// window carry digests over this shape, so validation accepts it for exactly
// that observation and nothing else.
type legacyFlaggedObservationSessionOpened struct {
	NativeSessionID      string `json:"native_session_id"`
	Resumed              bool   `json:"resumed"`
	NativeContinuityLost bool   `json:"native_continuity_lost"`
}

// MarshalJSON writes the adjacently tagged session_opened form.
func (o legacyFlaggedObservationSessionOpened) MarshalJSON() ([]byte, error) {
	type plain legacyFlaggedObservationSessionOpened
	return relayAdjacentJSON("type", "data", "session_opened", plain(o))
}

// legacyMailboxCommand is the protocol-33 shape of a mailbox command: one of
// the two fields is set.
type legacyMailboxCommand struct {
	deliverMailboxEvent *legacyMailboxEvent
	mailboxWake         []legacyMailboxEvent
}

// MarshalJSON writes the adjacently tagged legacy command.
func (c legacyMailboxCommand) MarshalJSON() ([]byte, error) {
	if c.deliverMailboxEvent != nil {
		return relayAdjacentJSON("type", "data", "deliver_mailbox_event", struct {
			Event legacyMailboxEvent `json:"event"`
		}{*c.deliverMailboxEvent})
	}
	return relayAdjacentJSON("type", "data", "mailbox_wake", struct {
		Events []legacyMailboxEvent `json:"events"`
	}{c.mailboxWake})
}

// legacyMailboxObservation is the protocol-33 shape of a mailbox-bearing
// observation, built by legacyMailboxObservationFrom.
type legacyMailboxObservation interface {
	json.Marshaler
	isLegacyMailboxObservation()
}

type legacyMailboxObservationCommandQueued struct {
	CommandID   string               `json:"command_id"`
	Command     legacyMailboxCommand `json:"command"`
	CreatedAtMs int64                `json:"created_at_ms"`
}

type legacyMailboxObservationMailboxHookLeaseCreated struct {
	Lease legacyMailboxHookLease `json:"lease"`
}

type legacyMailboxObservationMailboxEventsDelivered struct {
	EventKeys       []string             `json:"event_keys"`
	Path            MailboxDeliveryPath  `json:"path"`
	PromptCommandID *string              `json:"prompt_command_id,omitempty"`
	HookEvent       *string              `json:"hook_event,omitempty"`
	Events          []legacyMailboxEvent `json:"events,omitempty"`
	LeaseID         *string              `json:"lease_id,omitempty"`
}

func (legacyMailboxObservationCommandQueued) isLegacyMailboxObservation()           {}
func (legacyMailboxObservationMailboxHookLeaseCreated) isLegacyMailboxObservation() {}
func (legacyMailboxObservationMailboxEventsDelivered) isLegacyMailboxObservation()  {}

// MarshalJSON writes the adjacently tagged command_queued form.
func (o legacyMailboxObservationCommandQueued) MarshalJSON() ([]byte, error) {
	type plain legacyMailboxObservationCommandQueued
	return relayAdjacentJSON("type", "data", "command_queued", plain(o))
}

// MarshalJSON writes the adjacently tagged mailbox_hook_lease_created form.
func (o legacyMailboxObservationMailboxHookLeaseCreated) MarshalJSON() ([]byte, error) {
	type plain legacyMailboxObservationMailboxHookLeaseCreated
	return relayAdjacentJSON("type", "data", "mailbox_hook_lease_created", plain(o))
}

// MarshalJSON writes the adjacently tagged mailbox_events_delivered form.
func (o legacyMailboxObservationMailboxEventsDelivered) MarshalJSON() ([]byte, error) {
	type plain legacyMailboxObservationMailboxEventsDelivered
	return relayAdjacentJSON("type", "data", "mailbox_events_delivered", plain(o))
}

type legacyMailboxHookLease struct {
	LeaseID     string               `json:"lease_id"`
	Events      []legacyMailboxEvent `json:"events"`
	HookEvent   string               `json:"hook_event"`
	ExpiresAtMs int64                `json:"expires_at_ms"`
}

// legacyMailboxObservationFrom is the legacy form of an observation that
// carries mailbox events; false for any other observation.
func legacyMailboxObservationFrom(observation RelayObservation) (legacyMailboxObservation, bool) {
	panic("unported: mj-core/src/relay/snapshot/digest.rs LegacyMailboxObservation::from_observation")
}

func digestOver(domain, encoded []byte) string {
	hash := sha256.New()
	hash.Write(domain)
	hash.Write(encoded)
	return LowerHex(hash.Sum(nil))
}

// RelayStateDigest is a digest of an operational state's serialized form,
// which the worker compares with the one a controller names to tell whether
// that controller already holds this exact state. Only the worker computes
// it, over the same type and serializer each time, so equal states give equal
// digests.
func RelayStateDigest(state *RelayOperationalState) (string, error) {
	encoded, err := relayMarshalJSON(state)
	if err != nil {
		return "", fmt.Errorf("serialize relay operational state: %w", err)
	}
	digest := sha256.Sum256(encoded)
	return LowerHex(digest[:]), nil
}

// RelayEventDigest computes the domain-separated SHA-256 digest for a relay
// event, using the formula that matches the record's format. The digest field
// itself is excluded; for v2 so is previous_digest.
func RelayEventDigest(event *RelayEvent) (string, error) {
	return relayEventDigestOver(event, event.Observation)
}

func relayEventDigestOver(event *RelayEvent, observation json.Marshaler) (string, error) {
	switch event.Format {
	case RelayEventFormatV1:
		if err := ValidateRelayDigest(event.PreviousDigest, "previous event digest"); err != nil {
			return "", err
		}
		encoded, err := relayMarshalJSON(relayEventDigestPayload{
			Ordinal:        event.Ordinal,
			PreviousDigest: event.PreviousDigest,
			RecordedAtMs:   event.RecordedAtMs,
			CommandID:      event.CommandID,
			Observation:    observation,
		})
		if err != nil {
			return "", fmt.Errorf("serialize relay event digest payload: %w", err)
		}
		return digestOver(RelayEventDigestDomain, encoded), nil
	case RelayEventFormatV2:
		if event.PreviousDigest != "" {
			return "", fmt.Errorf("v2 relay event %d must not carry a previous_digest", event.Ordinal)
		}
		encoded, err := relayMarshalJSON(relayEventDigestPayloadV2{
			Ordinal:      event.Ordinal,
			RecordedAtMs: event.RecordedAtMs,
			CommandID:    event.CommandID,
			Observation:  observation,
		})
		if err != nil {
			return "", fmt.Errorf("serialize relay event digest payload: %w", err)
		}
		return digestOver(RelayEventDigestDomainV2, encoded), nil
	}
	return "", fmt.Errorf("unknown relay event format %d at event %d", event.Format, event.Ordinal)
}

// ValidateRelayEvent verifies an event against the exact previously applied
// event cursor. This is the shared validation contract for both the relay
// journal and controller projections.
//
// Every event is validated by its own recomputed digest (self-contained)
// plus ordinal contiguity. For v1 records the in-record previous_digest link
// to the cursor is also enforced; v2 records carry no link — their
// continuity to the cursor is proven by the digest anchor at the cursor
// ordinal (validate_cursor) and by the page/frontier endpoint, not by an
// in-record back-reference. This keeps a corrupt record from invalidating
// its successors.
func ValidateRelayEvent(previousOrdinal uint64, previousDigest string, event *RelayEvent) error {
	if err := ValidateRelayDigest(previousDigest, "previous cursor digest"); err != nil {
		return err
	}
	if previousOrdinal == ^uint64(0) {
		return errors.New("relay event ordinal exhausted")
	}
	expectedOrdinal := previousOrdinal + 1
	if event.Ordinal != expectedOrdinal {
		return fmt.Errorf("relay event gap: expected %d, found %d", expectedOrdinal, event.Ordinal)
	}
	if event.Format == RelayEventFormatV1 && event.PreviousDigest != previousDigest {
		return fmt.Errorf("relay event %d previous digest does not match cursor", event.Ordinal)
	}
	return ValidateRelayEventSelf(event)
}

// ValidateRelayEventSelf verifies a record purely against itself: its digest
// field is well-formed and recomputes to the same value. This is the
// corruption check for a single record, independent of any neighbour — the
// unit of trust that lets a corrupt record be isolated instead of poisoning
// the events around it. It does not check ordinal continuity or (for v1) the
// chain link; those are the caller's job where a trusted cursor is available.
func ValidateRelayEventSelf(event *RelayEvent) error {
	panic("unported: mj-core/src/relay/snapshot/digest.rs validate_relay_event_self")
}

func ValidateRelayDigest(digest, name string) error {
	valid := len(digest) == 64
	for i := 0; valid && i < len(digest); i++ {
		b := digest[i]
		valid = b >= '0' && b <= '9' || b >= 'a' && b <= 'f'
	}
	if !valid {
		return fmt.Errorf("%s must be 64 lowercase hexadecimal characters", name)
	}
	return nil
}
