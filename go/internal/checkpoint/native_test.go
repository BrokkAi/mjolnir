// Port of the tests in mj-checkpoint/src/native.rs.

package checkpoint

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// Hard-won: fab6694e7: platform-native dirs sent Muse imports outside XDG on macOS and Windows.
func TestMuseExternalAndPrivateProfilesResolveSeparateDataRoots(t *testing.T) {
	home, ok := core.UserHome()
	if !ok {
		t.Fatal("no user home")
	}
	// Muse follows XDG on macOS and Windows too, where os.UserConfigDir and
	// the platform data directory select a different native application
	// directory.
	xdg := func(name, fallback string) string {
		if value := os.Getenv(name); value != "" && filepath.IsAbs(value) {
			return filepath.Join(value, "muse")
		}
		return filepath.Join(home, fallback, "muse")
	}
	nativeConfig := xdg("XDG_CONFIG_HOME", ".config")
	nativeData := xdg("XDG_DATA_HOME", ".local/share")
	root, err := MuseSessionsRoot(nativeConfig)
	if err != nil {
		t.Fatal(err)
	}
	if want := filepath.Join(nativeData, "sessions"); root != want {
		t.Fatalf("external Muse sessions root = %s, want %s", root, want)
	}
	private := filepath.Join(t.TempDir(), "muse")
	root, err = MuseSessionsRoot(private)
	if err != nil {
		t.Fatal(err)
	}
	if want := filepath.Join(private, ".data/muse/sessions"); root != want {
		t.Fatalf("private Muse sessions root = %s, want %s", root, want)
	}
}
