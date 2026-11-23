// Port of mj-store/src/database/turn_outcome.rs.

// Semantic database bodies retain the exact internal turn for existing
// clients.

package store

import (
	"bytes"
	"encoding/json"
	"errors"

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
	semantic, err := json.Marshal(s.semantic)
	if err != nil {
		return nil, err
	}
	raw, err := json.Marshal(s.rawOutcome)
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
	encoded, err := json.Marshal(storedTurnOutcome{semantic: core.ApiTurnOutcomeFrom(turn), rawOutcome: *turn})
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
	panic("unported: mj-store/src/database/turn_outcome.rs migrate_turn_outcomes")
}
