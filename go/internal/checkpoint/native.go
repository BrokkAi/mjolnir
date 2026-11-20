// Port of mj-checkpoint/src/native.rs.
//
// Native harness storage shared by import and checkpoint restoration.

package checkpoint

import (
	"errors"
	"path/filepath"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// MuseSessionsRoot is where Muse keeps the sessions of configHome. External
// Muse sessions use XDG data storage; private mj profiles use their isolated
// data directory. Configuration credentials are never a session root.
func MuseSessionsRoot(configHome string) (string, error) {
	data, ok := core.HarnessKindMuse.NestedDataDir(configHome)
	if !ok {
		return "", errors.New("Muse data directory is unavailable")
	}
	return filepath.Join(data, "sessions"), nil
}
