// Port of the tests in mj-core/src/config/daemon.rs.

package core

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func TestPlacementLoadingDoesNotResolveDaemonProfiles(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	// Profiles intentionally have the wrong type for Config: a client must
	// not interpret daemon-owned settings before connecting.
	contents := `
profiles = "owned by remote"
[daemon]
runtime = "ssh"
host = "test-box"
ssh_args = ["-p", "2222"]
`
	if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
		t.Fatal(err)
	}
	placement, err := DaemonConfigLoadPlacement(path)
	if err != nil {
		t.Fatal(err)
	}
	if placement.Runtime != DaemonRuntimeSsh {
		t.Fatalf("runtime = %q", placement.Runtime)
	}
	if placement.Host == nil || *placement.Host != "test-box" {
		t.Fatalf("host = %v", placement.Host)
	}
	if !reflect.DeepEqual(placement.SSHArgs, []string{"-p", "2222"}) {
		t.Fatalf("ssh_args = %v", placement.SSHArgs)
	}
}

// An SSH client reads `[daemon]` only here, so a misspelled key would
// otherwise attach it to another instance without a word. Keys outside
// `[daemon]` belong to the daemon and are not this reader's to judge.
//
// Hard-won: #1557: once configuration types read past unknown keys, a `[daemon]` typo went unrefused
func TestPlacementRefusesADaemonKeyItDoesNotRead(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	contents := "a_later_section = 1\n[daemon]\nruntime = \"ssh\"\nhost = \"box\"\nisntance = \"work\"\n"
	if err := os.WriteFile(path, []byte(contents), 0o644); err != nil {
		t.Fatal(err)
	}
	_, err := DaemonConfigLoadPlacement(path)
	if err == nil {
		t.Fatal("a misspelled [daemon] key loaded")
	}
	refusal := err.Error()
	if !strings.Contains(refusal, "`daemon.isntance`") {
		t.Fatal(refusal)
	}
	if strings.Contains(refusal, "a_later_section") {
		t.Fatal(refusal)
	}
}
