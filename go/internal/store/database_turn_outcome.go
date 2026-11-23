// Port of mj-store/src/database/turn_outcome.rs.

// Semantic database bodies retain the exact internal turn for existing
// clients.

package store

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// storedTurnOutcome is a turn's stored body: the semantic outcome's fields
// (serde flatten) followed by raw_outcome.
type storedTurnOutcome struct {
	semantic   core.ApiTurnOutcome
	rawOutcome core.MaterializedTurnOutcome
}

// MarshalJSON writes the flattened semantic fields, then raw_outcome.
func (s storedTurnOutcome) MarshalJSON() ([]byte, error) {
	semantic, err := databaseMarshalJSON(s.semantic)
	if err != nil {
		return nil, err
	}
	raw, err := databaseMarshalJSON(s.rawOutcome)
	if err != nil {
		return nil, err
	}
	semantic = bytes.TrimSpace(semantic)
	if len(semantic) < 2 || semantic[0] != '{' || semantic[len(semantic)-1] != '}' {
		return nil, errors.New("can only flatten structs and maps")
	}
	out := append([]byte{}, semantic[:len(semantic)-1]...)
	if len(bytes.TrimSpace(semantic[1:len(semantic)-1])) > 0 {
		out = append(out, ',')
	}
	out = append(out, `"raw_outcome":`...)
	out = append(out, raw...)
	return append(out, '}'), nil
}

func serializeTurnOutcome(turn *core.MaterializedTurnOutcome) (string, error) {
	encoded, err := databaseMarshalJSON(storedTurnOutcome{semantic: core.ApiTurnOutcomeFrom(turn), rawOutcome: *turn})
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}

func deserializeTurnOutcome(body string) (core.MaterializedTurnOutcome, error) {
	var raw struct {
		RawOutcome *core.MaterializedTurnOutcome `json:"raw_outcome"`
	}
	if err := json.Unmarshal([]byte(body), &raw); err != nil {
		return core.MaterializedTurnOutcome{}, err
	}
	if raw.RawOutcome == nil {
		return core.MaterializedTurnOutcome{}, errors.New("missing field `raw_outcome`")
	}
	return *raw.RawOutcome, nil
}

// migrateTurnOutcomes rewrites legacy turn bodies in the semantic form.
func migrateTurnOutcomes(tx *Transaction) error {
	type usageRow struct{ session, command, body string }
	rows, err := tx.Query("SELECT session_id, command_id, body FROM session_turn_usage")
	if err != nil {
		return err
	}
	var usage []usageRow
	for rows.Next() {
		var row usageRow
		if err := rows.Scan(&row.session, &row.command, &row.body); err != nil {
			rows.Close()
			return err
		}
		usage = append(usage, row)
	}
	if err := errors.Join(rows.Err(), rows.Close()); err != nil {
		return err
	}
	for _, row := range usage {
		var turn core.MaterializedTurnOutcome
		if err := json.Unmarshal([]byte(row.body), &turn); err != nil {
			return fmt.Errorf("decode legacy session_turn_usage for %s/%s: %w", row.session, row.command, err)
		}
		body, err := serializeTurnOutcome(&turn)
		if err != nil {
			return err
		}
		if _, err := tx.Exec("UPDATE session_turn_usage SET body=?3 WHERE session_id=?1 AND command_id=?2", row.session, row.command, body); err != nil {
			return err
		}
	}
	type lastRow struct{ session, body string }
	rows, err = tx.Query("SELECT session_id, last_turn_outcome_json FROM materialized_sessions WHERE last_turn_outcome_json IS NOT NULL")
	if err != nil {
		return err
	}
	var last []lastRow
	for rows.Next() {
		var row lastRow
		if err := rows.Scan(&row.session, &row.body); err != nil {
			rows.Close()
			return err
		}
		last = append(last, row)
	}
	if err := errors.Join(rows.Err(), rows.Close()); err != nil {
		return err
	}
	for _, row := range last {
		var turn core.MaterializedTurnOutcome
		if err := json.Unmarshal([]byte(row.body), &turn); err != nil {
			return fmt.Errorf("decode legacy materialized last turn for %s: %w", row.session, err)
		}
		body, err := serializeTurnOutcome(&turn)
		if err != nil {
			return err
		}
		if _, err := tx.Exec("UPDATE materialized_sessions SET last_turn_outcome_json=?2 WHERE session_id=?1", row.session, body); err != nil {
			return err
		}
	}
	return nil
}
