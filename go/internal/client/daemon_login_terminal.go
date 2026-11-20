// Port of mj-client/src/daemon/login_terminal.rs.

// Dedicated login terminal framing, shared by every placement.

package client

import (
	"context"
	"encoding/json"
	"fmt"
	"net"
)

// TerminalInput is what a client sends a login terminal. Externally tagged
// with the Rust variant names; Bytes is a JSON array of numbers, as serde
// writes a Vec<u8>.
type TerminalInput interface {
	isTerminalInput()
}

type TerminalInputBytes struct {
	Bytes []byte
}

type TerminalInputResize struct {
	Cols uint16 `json:"cols"`
	Rows uint16 `json:"rows"`
}

func (TerminalInputBytes) isTerminalInput()  {}
func (TerminalInputResize) isTerminalInput() {}

func (v TerminalInputBytes) MarshalJSON() ([]byte, error) {
	return daemonExternalJSON("Bytes", daemonByteArrayJSON(v.Bytes))
}

func (v TerminalInputResize) MarshalJSON() ([]byte, error) {
	type plain TerminalInputResize
	body, err := daemonMarshalStruct(plain(v))
	if err != nil {
		return nil, err
	}
	return daemonExternalJSON("Resize", body)
}

// UnmarshalTerminalInputJSON decodes serde's externally tagged form.
func UnmarshalTerminalInputJSON(data []byte) (TerminalInput, error) {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "Bytes":
		bytes, err := daemonDecodeByteArray(body)
		if err != nil {
			return nil, err
		}
		return TerminalInputBytes{Bytes: bytes}, nil
	case "Resize":
		var v TerminalInputResize
		if err := daemonDecodeStruct(body, &v, false); err != nil {
			return nil, err
		}
		return v, nil
	}
	return nil, daemonUnknownVariant(name, []string{"Bytes", "Resize"})
}

// TerminalOutput is what a login terminal sends back. Externally tagged with
// the Rust variant names.
type TerminalOutput interface {
	isTerminalOutput()
}

type TerminalOutputBytes struct {
	Bytes []byte
}

type TerminalOutputExited struct {
	Code uint32 `json:"code"`
}

type TerminalOutputFailed struct {
	Message string
}

func (TerminalOutputBytes) isTerminalOutput()  {}
func (TerminalOutputExited) isTerminalOutput() {}
func (TerminalOutputFailed) isTerminalOutput() {}

func (v TerminalOutputBytes) MarshalJSON() ([]byte, error) {
	return daemonExternalJSON("Bytes", daemonByteArrayJSON(v.Bytes))
}

func (v TerminalOutputExited) MarshalJSON() ([]byte, error) {
	type plain TerminalOutputExited
	body, err := daemonMarshalStruct(plain(v))
	if err != nil {
		return nil, err
	}
	return daemonExternalJSON("Exited", body)
}

func (v TerminalOutputFailed) MarshalJSON() ([]byte, error) {
	body, err := daemonMarshalJSON(v.Message)
	if err != nil {
		return nil, err
	}
	return daemonExternalJSON("Failed", body)
}

// UnmarshalTerminalOutputJSON decodes serde's externally tagged form.
func UnmarshalTerminalOutputJSON(data []byte) (TerminalOutput, error) {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "Bytes":
		bytes, err := daemonDecodeByteArray(body)
		if err != nil {
			return nil, err
		}
		return TerminalOutputBytes{Bytes: bytes}, nil
	case "Exited":
		var v TerminalOutputExited
		if err := daemonDecodeStruct(body, &v, false); err != nil {
			return nil, err
		}
		return v, nil
	case "Failed":
		var message string
		if err := json.Unmarshal(body, &message); err != nil {
			return nil, err
		}
		return TerminalOutputFailed{Message: message}, nil
	}
	return nil, daemonUnknownVariant(name, []string{"Bytes", "Exited", "Failed"})
}

// LoginTerminal owns the placement lease until the dedicated terminal
// closes.
type LoginTerminal struct {
	Stream    net.Conn
	Connector *DaemonConnector
}

// LoginTerminal consumes a dedicated client; ordinary control traffic must
// use another connection.
func (c *DaemonClient) LoginTerminal(ctx context.Context, profileID string, cols, rows uint16) (*LoginTerminal, error) {
	reply, err := c.Request(ctx, DaemonActionLoginTerminal{ProfileID: profileID, Cols: cols, Rows: rows})
	if err != nil {
		return nil, err
	}
	if _, ready := reply.(DaemonReplyLoginTerminalReady); !ready {
		return nil, fmt.Errorf("unexpected login terminal reply %s", daemonReplyDebug(reply))
	}
	stream, err := c.takeStream()
	if err != nil {
		return nil, err
	}
	return &LoginTerminal{Stream: stream, Connector: c.connector}, nil
}
