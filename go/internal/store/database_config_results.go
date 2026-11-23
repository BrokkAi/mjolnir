// Port of mj-store/src/database/config_results.rs.

package store

// LoadConfigResult is the exact result of a configuration command, once
// durably projected. done is false while the command has no projected result
// (Rust's outer None); rejection is the recorded error, nil when the command
// was accepted (Rust's inner None). The shape matches
// client.SessionHandleBackend's ConfigResult.
func LoadConfigResult(sessionID, commandID string) (rejection *string, done bool, err error) {
	panic("unported: mj-store/src/database/config_results.rs load_config_result")
}
