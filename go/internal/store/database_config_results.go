// Port of mj-store/src/database/config_results.rs.

package store

import (
	"database/sql"
	"errors"
)

// LoadConfigResult is the exact result of a configuration command, once
// durably projected. done is false while the command has no projected result
// (Rust's outer None); rejection is the recorded error, nil when the command
// was accepted (Rust's inner None). The shape matches
// client.SessionHandleBackend's ConfigResult.
func LoadConfigResult(sessionID, commandID string) (rejection *string, done bool, err error) {
	reader, err := openReader(DatabasePath())
	if err != nil {
		return nil, false, err
	}
	defer reader.Close()
	var stored sql.NullString
	err = reader.QueryRow(
		"SELECT error FROM api_config_results WHERE session_id = ?1 AND command_id = ?2",
		sessionID, commandID,
	).Scan(&stored)
	if errors.Is(err, sql.ErrNoRows) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, err
	}
	if !stored.Valid {
		return nil, true, nil
	}
	return &stored.String, true, nil
}
