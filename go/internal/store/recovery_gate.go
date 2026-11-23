// Port of mj-store/src/recovery_gate.rs.

// Per-session coordination between foreground operations and background
// recovery. The gate itself lives in core (Rust re-exports
// mj_core::recovery_gate here; Go callers use core directly); this file adds
// the check a queued placement makes against the store once it is admitted.

package store

import (
	"reflect"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// CurrentBackgroundSession validates a queued placement after admission. A
// lifecycle may have finished between the observation and this attempt, so
// the old target cannot be used. Nil means the placement no longer applies.
func CurrentBackgroundSession(observed *core.SessionRecord) (*core.SessionRecord, error) {
	owned, err := ContainerUpgradeOwnsTarget(observed.ID)
	if err != nil || owned {
		return nil, err
	}
	current, err := ReadDurableSessionRecord(observed.ID)
	if err != nil || current == nil {
		return nil, err
	}
	if current.State == core.SessionStateRunning &&
		reflect.DeepEqual(current.Target, observed.Target) &&
		reflect.DeepEqual(current.NativeSessionID, observed.NativeSessionID) &&
		current.HarnessKind == observed.HarnessKind &&
		current.LastProfile == observed.LastProfile {
		return current, nil
	}
	return nil, nil
}
