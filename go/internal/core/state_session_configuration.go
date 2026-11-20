// Port of mj-core/src/state/session_configuration.rs.

package core

import (
	"bytes"
	"encoding/json"
	"sort"
)

// SessionConfiguration: adapter settings and projected goal state have
// independent owners. Keep the existing flat JSON shape for stores, clients
// and checkpoints: the goal under "mj_goal_state" beside the flattened
// values (serde flatten).
type SessionConfiguration struct {
	Goal   *GoalState
	Values map[string]any
}

func (c *SessionConfiguration) IsEmpty() bool {
	return len(c.Values) == 0 && c.Goal == nil
}

// MarshalJSON writes the goal first, then the flattened values in key order.
func (c SessionConfiguration) MarshalJSON() ([]byte, error) {
	var out bytes.Buffer
	out.WriteByte('{')
	first := true
	write := func(key string, value any) error {
		encodedKey, err := json.Marshal(key)
		if err != nil {
			return err
		}
		encodedValue, err := json.Marshal(value)
		if err != nil {
			return err
		}
		if !first {
			out.WriteByte(',')
		}
		first = false
		out.Write(encodedKey)
		out.WriteByte(':')
		out.Write(encodedValue)
		return nil
	}
	if c.Goal != nil {
		if err := write(ProjectionKey, c.Goal); err != nil {
			return nil, err
		}
	}
	keys := make([]string, 0, len(c.Values))
	for key := range c.Values {
		keys = append(keys, key)
	}
	sort.Strings(keys)
	for _, key := range keys {
		if err := write(key, c.Values[key]); err != nil {
			return nil, err
		}
	}
	out.WriteByte('}')
	return out.Bytes(), nil
}

func (c *SessionConfiguration) UnmarshalJSON(data []byte) error {
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	c.Goal = nil
	c.Values = map[string]any{}
	for key, raw := range fields {
		if key == ProjectionKey {
			if string(bytes.TrimSpace(raw)) == "null" {
				continue
			}
			var goal GoalState
			if err := json.Unmarshal(raw, &goal); err != nil {
				return err
			}
			c.Goal = &goal
			continue
		}
		decoder := json.NewDecoder(bytes.NewReader(raw))
		decoder.UseNumber()
		var value any
		if err := decoder.Decode(&value); err != nil {
			return err
		}
		c.Values[key] = value
	}
	return nil
}
