// Port of mj-store/src/database/container_size.rs.

// Breaking revision 132: one durable per-session container size.

package store

import (
	"bytes"
	"database/sql"
	"encoding/json"
	"fmt"
	"log/slog"
	"math/big"
	"math/bits"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// decimalUnits parses positive decimal engine sizes exactly, rounding up
// only the final unit; false when text is not one or the size is zero or
// overflows.
func decimalUnits(text string, multiplier uint64) (uint64, bool) {
	text = strings.TrimSpace(text)
	whole, fraction, _ := strings.Cut(text, ".")
	if whole == "" && fraction == "" {
		return 0, false
	}
	for _, digit := range []byte(whole + fraction) {
		if digit < '0' || digit > '9' {
			return 0, false
		}
	}
	var wholeValue uint64
	if whole != "" {
		parsed, err := strconv.ParseUint(whole, 10, 64)
		if err != nil {
			return 0, false
		}
		wholeValue = parsed
	}
	high, units := bits.Mul64(wholeValue, multiplier)
	if high != 0 {
		return 0, false
	}
	// Trim trailing zeros so long decimal representations remain exact.
	fraction = strings.TrimRight(fraction, "0")
	if fraction != "" {
		// Rust computes in u128: 10^39 and larger do not fit.
		if len(fraction) > 38 {
			return 0, false
		}
		denominator := new(big.Int).Exp(big.NewInt(10), big.NewInt(int64(len(fraction))), nil)
		numerator, ok := new(big.Int).SetString(fraction, 10)
		if !ok {
			return 0, false
		}
		numerator.Mul(numerator, new(big.Int).SetUint64(multiplier))
		if numerator.BitLen() > 128 {
			return 0, false
		}
		rounded, remainder := new(big.Int).QuoRem(numerator, denominator, new(big.Int))
		if remainder.Sign() != 0 {
			rounded.Add(rounded, big.NewInt(1))
		}
		if !rounded.IsUint64() {
			return 0, false
		}
		sum, carry := bits.Add64(units, rounded.Uint64(), 0)
		if carry != 0 {
			return 0, false
		}
		units = sum
	}
	return units, units > 0
}

func memoryBytes(text string) (uint64, bool) {
	text = strings.TrimSpace(text)
	split := strings.IndexFunc(text, func(c rune) bool { return (c < '0' || c > '9') && c != '.' })
	if split < 0 {
		split = len(text)
	}
	var multiplier uint64
	switch strings.ToLower(text[split:]) {
	case "", "b":
		multiplier = 1
	case "k", "kb", "kib":
		multiplier = 1 << 10
	case "m", "mb", "mib":
		multiplier = 1 << 20
	case "g", "gb", "gib":
		multiplier = 1 << 30
	case "t", "tb", "tib":
		multiplier = 1 << 40
	default:
		return 0, false
	}
	return decimalUnits(text[:split], multiplier)
}

// fold combines a legacy allocation with the session's legacy cpu and memory
// override strings.
func fold(allocation core.SessionResourceAllocation, cpus, memory *string) core.SessionResourceAllocation {
	if _, ec2 := allocation.(core.SessionResourceAllocationAwsEc2); ec2 {
		return allocation
	}
	var foldedCpus, foldedMemory *uint64
	if container, ok := allocation.(core.SessionResourceAllocationContainer); ok {
		foldedCpus, foldedMemory = &container.Cpus, &container.MemoryBytes
	}
	if cpus != nil {
		if parsed, ok := decimalUnits(*cpus, 1); ok {
			foldedCpus = &parsed
		}
	}
	if memory != nil {
		if parsed, ok := memoryBytes(*memory); ok {
			foldedMemory = &parsed
		}
	}
	if foldedCpus != nil && foldedMemory != nil {
		return core.SessionResourceAllocationContainer{Cpus: *foldedCpus, MemoryBytes: *foldedMemory}
	}
	return allocation
}

// containerSizeDecode reads JSON text as serde_json::Value: objects as
// map[string]any, whose keys Go encodes sorted as serde_json's map does, and
// numbers as json.Number so none is rounded.
func containerSizeDecode(text []byte) (any, error) {
	decoder := json.NewDecoder(bytes.NewReader(text))
	decoder.UseNumber()
	var value any
	if err := decoder.Decode(&value); err != nil {
		return nil, err
	}
	if decoder.More() {
		return nil, fmt.Errorf("trailing characters")
	}
	return value, nil
}

// containerSizeToValue is serde_json::to_value.
func containerSizeToValue(value any) (any, error) {
	encoded, err := databaseMarshalJSON(value)
	if err != nil {
		return nil, err
	}
	return containerSizeDecode(encoded)
}

// containerSizePointer is serde_json's Value::pointer_mut: the value a JSON
// pointer names, or nil when it names nothing.
func containerSizePointer(value any, pointer string) any {
	if pointer == "" {
		return value
	}
	if !strings.HasPrefix(pointer, "/") {
		return nil
	}
	for _, token := range strings.Split(pointer[1:], "/") {
		token = strings.ReplaceAll(strings.ReplaceAll(token, "~1", "/"), "~0", "~")
		switch target := value.(type) {
		case map[string]any:
			next, ok := target[token]
			if !ok {
				return nil
			}
			value = next
		case []any:
			if token == "" || token[0] == '+' || (len(token) > 1 && token[0] == '0') {
				return nil
			}
			index, err := strconv.ParseUint(token, 10, 64)
			if err != nil || index >= uint64(len(target)) {
				return nil
			}
			value = target[index]
		default:
			return nil
		}
	}
	return value
}

// rewriteEmbeddedSizes uses persisted type locations rather than
// interpreting arbitrary JSON keys (a runtime environment map may itself
// contain a variable named container_cpus). value is a decoded JSON value
// (map[string]any and friends) rewritten in place.
func rewriteEmbeddedSizes(value any, sessionPaths, snapshotPaths []string, changes *[]any) error {
	for _, path := range sessionPaths {
		if fields, ok := containerSizePointer(value, path).(map[string]any); ok {
			if err := foldFields(fields, "container_cpus", "container_memory", path, changes); err != nil {
				return err
			}
		}
	}
	for _, path := range snapshotPaths {
		if fields, ok := containerSizePointer(value, path).(map[string]any); ok {
			if err := foldFields(fields, "cpus", "memory", path, changes); err != nil {
				return err
			}
		}
	}
	return nil
}

// foldJSONAllocation folds decoded JSON values (nil when absent): a stored
// allocation and override strings, of which anything but a string counts as
// no override.
func foldJSONAllocation(allocation, cpus, memory any) (core.SessionResourceAllocation, error) {
	var decoded core.SessionResourceAllocation
	if allocation != nil {
		encoded, err := databaseMarshalJSON(allocation)
		if err != nil {
			return nil, err
		}
		decoded, err = core.UnmarshalSessionResourceAllocationJSON(encoded)
		if err != nil {
			return nil, err
		}
	}
	text := func(value any) *string {
		if text, ok := value.(string); ok {
			return &text
		}
		return nil
	}
	return fold(decoded, text(cpus), text(memory)), nil
}

// containerSizeAllocationValue is an allocation as serde_json::to_value
// writes an Option<SessionResourceAllocation>.
func containerSizeAllocationValue(allocation core.SessionResourceAllocation) (any, error) {
	if allocation == nil {
		return nil, nil
	}
	return containerSizeToValue(allocation)
}

// rewriteMoveSource preserves the effective source size of a legacy failed
// Move: its Resume restored this allocation together with the recovery
// session's override strings.
func rewriteMoveSource(value any, inheritedCpus, inheritedMemory *string, changes *[]any) error {
	record, ok := value.(map[string]any)
	if !ok {
		return nil
	}
	// Without a recovery snapshot, legacy Resume kept the session row's
	// strings.
	var cpus, memory any
	var hasCpus, hasMemory bool
	if recovery, ok := record["recovery_session"]; ok && recovery != nil {
		if fields, isObject := recovery.(map[string]any); isObject {
			cpus, hasCpus = fields["container_cpus"]
			memory, hasMemory = fields["container_memory"]
		}
	} else {
		if inheritedCpus != nil {
			cpus, hasCpus = *inheritedCpus, true
		}
		if inheritedMemory != nil {
			memory, hasMemory = *inheritedMemory, true
		}
	}
	if !hasCpus && !hasMemory {
		return nil
	}
	allocation, err := foldJSONAllocation(record["source_resource_allocation"], cpus, memory)
	if err != nil {
		return err
	}
	encoded, err := containerSizeAllocationValue(allocation)
	if err != nil {
		return err
	}
	record["source_resource_allocation"] = encoded
	*changes = append(*changes, map[string]any{
		"path": "/source_resource_allocation", "old_cpus": cpus, "old_memory": memory, "allocation": encoded,
	})
	return nil
}

func foldFields(fields map[string]any, cpuKey, memoryKey, path string, changes *[]any) error {
	cpus, hasCpus := fields[cpuKey]
	memory, hasMemory := fields[memoryKey]
	delete(fields, cpuKey)
	delete(fields, memoryKey)
	if !hasCpus && !hasMemory {
		return nil
	}
	allocation, err := foldJSONAllocation(fields["resource_allocation"], cpus, memory)
	if err != nil {
		return err
	}
	encoded, err := containerSizeAllocationValue(allocation)
	if err != nil {
		return err
	}
	fields["resource_allocation"] = encoded
	*changes = append(*changes, map[string]any{
		"path": path, "old_cpus": cpus, "old_memory": memory, "allocation": encoded,
	})
	return nil
}

// conversion is one row the migration changed or whose overrides it dropped.
type conversion struct {
	table      string
	recordID   string
	sessionID  string
	cpus       *string
	memory     *string
	allocation core.SessionResourceAllocation
	snapshot   any
}

// migrate converts every stored container size and returns the
// changed/dropped rows after committing, so no rolled-back change is
// reported.
func migrate(conn *Connection) ([]conversion, error) {
	tx, err := conn.Begin()
	if err != nil {
		return nil, err
	}
	defer tx.Rollback()
	type sessionRow struct {
		sessionID           string
		cpus, memory        *string
		allocation, runtime *string
	}
	rows, err := valuesQueryMap(tx, "SELECT session_id, container_cpus, container_memory, resource_allocation, target_runtime_json FROM sessions", nil,
		func(rows *sql.Rows) (sessionRow, error) {
			var row sessionRow
			err := rows.Scan(&row.sessionID, &row.cpus, &row.memory, &row.allocation, &row.runtime)
			return row, err
		})
	if err != nil {
		return nil, err
	}
	var changes []conversion
	for _, row := range rows {
		var allocation core.SessionResourceAllocation
		if row.allocation != nil {
			decoded, err := core.UnmarshalSessionResourceAllocationJSON([]byte(*row.allocation))
			if err != nil {
				return nil, err
			}
			allocation = decoded
		}
		allocation = fold(allocation, row.cpus, row.memory)
		var runtime any
		if row.runtime != nil {
			runtime, err = containerSizeDecode([]byte(*row.runtime))
			if err != nil {
				return nil, err
			}
		}
		var details []any
		if row.runtime != nil {
			if err := rewriteEmbeddedSizes(runtime, nil, []string{"/container_settings"}, &details); err != nil {
				return nil, err
			}
		}
		var snapshot any
		if len(details) > 0 {
			snapshot = details
		}
		if row.cpus != nil || row.memory != nil || snapshot != nil {
			var storedAllocation, storedRuntime *string
			if allocation != nil {
				encoded, err := databaseMarshalJSON(allocation)
				if err != nil {
					return nil, err
				}
				text := string(encoded)
				storedAllocation = &text
			}
			if row.runtime != nil {
				encoded, err := databaseMarshalJSON(runtime)
				if err != nil {
					return nil, err
				}
				text := string(encoded)
				storedRuntime = &text
			}
			if _, err := tx.Exec("UPDATE sessions SET resource_allocation=?2, target_runtime_json=?3 WHERE session_id=?1",
				row.sessionID, storedAllocation, storedRuntime); err != nil {
				return nil, err
			}
			changes = append(changes, conversion{
				table:      "sessions",
				recordID:   row.sessionID,
				sessionID:  row.sessionID,
				cpus:       row.cpus,
				memory:     row.memory,
				allocation: allocation,
				snapshot:   snapshot,
			})
		}
	}
	// These durable records also embed SessionRecord or
	// TargetRuntimeSettings. Convert all copies before committing the new
	// incompatible schema revision.
	for _, record := range []struct {
		table, idColumn, sessionColumn, jsonColumn string
		sessionPaths, snapshotPaths                []string
	}{
		{
			"session_moves", "operation_id", "session_id", "operation_json",
			[]string{"/recovery_session", "/workspace_transfer/source"},
			[]string{
				"/recovery_session/target_runtime/container_settings",
				"/workspace_transfer/source/target_runtime/container_settings",
				"/prepared_destination/runtime/container_settings",
			},
		},
		{
			"retained_move_sources", "operation_id", "session_id", "source_json",
			[]string{""},
			[]string{"/target_runtime/container_settings"},
		},
		{
			"environments", "environment_id", "owner_session_id", "placement_json",
			nil,
			[]string{
				"/runtime/container_settings",
				"/prepared_destination/destination/runtime/container_settings",
			},
		},
		{
			"environment_workers", "lease_id", "worker_session_id", "runtime_json",
			nil,
			[]string{"/container_settings"},
		},
	} {
		type jsonRow struct{ recordID, sessionID, json string }
		rows, err := valuesQueryMap(tx, fmt.Sprintf("SELECT %s, %s, %s FROM %s WHERE %s IS NOT NULL",
			record.idColumn, record.sessionColumn, record.jsonColumn, record.table, record.jsonColumn), nil,
			func(rows *sql.Rows) (jsonRow, error) {
				var row jsonRow
				err := rows.Scan(&row.recordID, &row.sessionID, &row.json)
				return row, err
			})
		if err != nil {
			return nil, err
		}
		for _, row := range rows {
			value, err := containerSizeDecode([]byte(row.json))
			if err != nil {
				return nil, fmt.Errorf("read %s row %s for container size conversion: %w", record.table, row.recordID, err)
			}
			var details []any
			if record.table == "session_moves" {
				var cpus, memory *string
				object, _ := value.(map[string]any)
				if recovery, ok := object["recovery_session"]; !ok || recovery == nil {
					if _, err := valuesOptional(tx.QueryRow("SELECT container_cpus,container_memory FROM sessions WHERE session_id=?1", row.sessionID).
						Scan(&cpus, &memory)); err != nil {
						return nil, err
					}
				}
				if err := rewriteMoveSource(value, cpus, memory, &details); err != nil {
					return nil, err
				}
			}
			if err := rewriteEmbeddedSizes(value, record.sessionPaths, record.snapshotPaths, &details); err != nil {
				return nil, fmt.Errorf("convert container sizes in %s row %s: %w", record.table, row.recordID, err)
			}
			if len(details) > 0 {
				encoded, err := databaseMarshalJSON(value)
				if err != nil {
					return nil, err
				}
				if _, err := tx.Exec(fmt.Sprintf("UPDATE %s SET %s=?2 WHERE %s=?1", record.table, record.jsonColumn, record.idColumn),
					row.recordID, string(encoded)); err != nil {
					return nil, err
				}
				changes = append(changes, conversion{
					table:     record.table,
					recordID:  row.recordID,
					sessionID: row.sessionID,
					snapshot:  details,
				})
			}
		}
	}
	if _, err := tx.Exec(`ALTER TABLE sessions DROP COLUMN container_cpus;
        ALTER TABLE sessions DROP COLUMN container_memory;
        UPDATE schema_compatibility SET minimum_compatible_version=132 WHERE singleton=1;
        INSERT INTO schema_migrations(version,applied_at) VALUES(132,strftime('%Y-%m-%dT%H:%M:%fZ','now'));
        PRAGMA user_version=132;`); err != nil {
		return nil, err
	}
	if err := tx.Commit(); err != nil {
		return nil, err
	}
	return changes, nil
}

// Report logs each conversion migrate made.
func Report(changes []conversion) {
	for _, change := range changes {
		snapshot := "None"
		if change.snapshot != nil {
			encoded, err := databaseMarshalJSON(change.snapshot)
			if err != nil {
				snapshot = fmt.Sprintf("%v", change.snapshot)
			} else {
				snapshot = "Some(" + string(encoded) + ")"
			}
		}
		slog.Warn("converted or dropped legacy container size overrides",
			"table", change.table, "record_id", change.recordID, "session_id", change.sessionID,
			"old_cpus", containerSizeDebugOption(change.cpus), "old_memory", containerSizeDebugOption(change.memory),
			"allocation", fmt.Sprintf("%#v", change.allocation), "snapshot", snapshot)
	}
}

// containerSizeDebugOption is an Option<String> as Rust's Debug writes it.
func containerSizeDebugOption(value *string) string {
	if value == nil {
		return "None"
	}
	return "Some(" + core.RustDebugString(*value) + ")"
}
