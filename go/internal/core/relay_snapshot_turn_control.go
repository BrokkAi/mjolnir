// Port of mj-core/src/relay/snapshot/turn_control.rs.
//
// Published control choices; only the worker decides their availability.

package core

import "encoding/json"

type TurnControlStatus string

const (
	TurnControlStatusReady       TurnControlStatus = "ready"
	TurnControlStatusPending     TurnControlStatus = "pending"
	TurnControlStatusUnconfirmed TurnControlStatus = "unconfirmed"
	TurnControlStatusUnavailable TurnControlStatus = "unavailable"
)

type TurnControlKind string

const (
	TurnControlKindSteer       TurnControlKind = "steer"
	TurnControlKindApplyQueued TurnControlKind = "apply_queued"
	TurnControlKindInterrupt   TurnControlKind = "interrupt"
)

type TurnControlAction struct {
	Kind    TurnControlKind `json:"kind"`
	Command RelayCommand    `json:"command"`
}

// UnmarshalJSON decodes Command through its variant table.
func (a *TurnControlAction) UnmarshalJSON(data []byte) error {
	type plain TurnControlAction
	aux := struct {
		*plain
		Command json.RawMessage `json:"command"`
	}{plain: (*plain)(a)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	command, err := UnmarshalRelayCommandJSON(aux.Command)
	if err != nil {
		return err
	}
	a.Command = command
	return nil
}

type TurnControls struct {
	Status  TurnControlStatus  `json:"status"`
	Primary *TurnControlAction `json:"primary"`
	// Explicit interruption remains separate from delivering queued input.
	// Nil is serialized as null (Rust Option without skip).
	Interrupt RelayCommand `json:"interrupt"`
	// Short, wrapping product explanation, separate from the button label.
	Message *string `json:"message"`
}

// UnmarshalJSON decodes Interrupt through its variant table.
func (c *TurnControls) UnmarshalJSON(data []byte) error {
	type plain TurnControls
	aux := struct {
		*plain
		Interrupt json.RawMessage `json:"interrupt"`
	}{plain: (*plain)(c)}
	if err := json.Unmarshal(data, &aux); err != nil {
		return err
	}
	c.Interrupt = nil
	if len(aux.Interrupt) > 0 && string(aux.Interrupt) != "null" {
		command, err := UnmarshalRelayCommandJSON(aux.Interrupt)
		if err != nil {
			return err
		}
		c.Interrupt = command
	}
	return nil
}
