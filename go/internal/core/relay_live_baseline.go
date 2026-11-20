// Port of mj-core/src/relay/live_baseline.rs.
//
// Connection-owned live recovery cuts and frozen snapshot transport.

package core

import (
	"encoding/base64"
	"encoding/json"
	"fmt"
)

type LiveQueuedCommand struct {
	CommandID       string       `json:"command_id"`
	Command         RelayCommand `json:"command"`
	CreatedAtMs     int64        `json:"created_at_ms"`
	AcceptedOrdinal *uint64      `json:"accepted_ordinal"`
}

// UnmarshalJSON decodes Command through its variant table.
func (c *LiveQueuedCommand) UnmarshalJSON(data []byte) error {
	type plain LiveQueuedCommand
	aux := struct {
		*plain
		Command json.RawMessage `json:"command"`
	}{plain: (*plain)(c)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	command, err := UnmarshalRelayCommandJSON(aux.Command)
	if err != nil {
		return err
	}
	c.Command = command
	return nil
}

// LiveSessionBaseline is one coherent current-state cut; no transcript
// history is represented.
type LiveSessionBaseline struct {
	State RelayOperationalState `json:"state"`
	Queue []LiveQueuedCommand   `json:"queue"`
	// Started startup briefs; preserves launch idempotency through a later archive.
	DeliveredInitialPrompts []LiveQueuedCommand       `json:"delivered_initial_prompts"`
	Questions               []ElicitationRequest      `json:"questions"`
	UnavailableQuestions    int                       `json:"unavailable_questions"`
	NativeAgentReplay       *[]NativeAgent            `json:"native_agent_replay"`
	NativeUseEvidence       *NativeSessionUseEvidence `json:"native_use_evidence"`
}

type FrozenSnapshotPage struct {
	Offset     uint64 `json:"offset"`
	NextOffset uint64 `json:"next_offset"`
	Complete   bool   `json:"complete"`
	Chunk      string `json:"chunk"`
}

// FrozenSnapshotDownload is a frozen byte download that belongs to its
// connection and dies on disconnect.
type FrozenSnapshotDownload struct {
	bytes      []byte
	nextOffset uint64
}

func NewFrozenSnapshotDownload(bytes []byte) *FrozenSnapshotDownload {
	return &FrozenSnapshotDownload{bytes: bytes}
}

func (d *FrozenSnapshotDownload) Read(offset uint64, capacity int) (FrozenSnapshotPage, error) {
	if offset != d.nextOffset {
		return FrozenSnapshotPage{}, fmt.Errorf("snapshot page offset mismatch: expected %d, got %d", d.nextOffset, offset)
	}
	start := offset
	end := min(start+uint64(capacity), uint64(len(d.bytes)))
	if start+uint64(capacity) < start {
		end = uint64(len(d.bytes))
	}
	if start > end {
		return FrozenSnapshotPage{}, fmt.Errorf("snapshot offset exceeds download length")
	}
	chunk := base64.StdEncoding.EncodeToString(d.bytes[start:end])
	d.nextOffset = end
	return FrozenSnapshotPage{
		Offset:     offset,
		NextOffset: d.nextOffset,
		Complete:   end == uint64(len(d.bytes)),
		Chunk:      chunk,
	}, nil
}

func LiveBaselinePageCapacity(protocolVersion uint32, requestID string) (int, error) {
	return RelayTransferChunkCapacity(RelayResponseEnvelope{
		RequestID:       requestID,
		ProtocolVersion: protocolVersion,
		Body: RelayResponseBodyOk{Payload: RelayResponsePayloadLiveBaselinePage{Page: FrozenSnapshotPage{
			Offset:     ^uint64(0),
			NextOffset: ^uint64(0),
		}}},
	})
}

// LiveBaselineReply is the shared framing of a connection-owned recovery
// download. Capture is separate so callers can release the relay lock before
// serialization and encoding.
func LiveBaselineReply(request RelayRequest, cut *LiveSessionBaseline, download **FrozenSnapshotDownload, requestID string, protocolVersion uint32) (RelayResponsePayload, error) {
	panic("unported: mj-core/src/relay/live_baseline.rs live_baseline_reply")
}
