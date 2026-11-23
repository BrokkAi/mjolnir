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

func daemonTestSSHPlacement() DaemonConfig {
	placement := DefaultDaemonConfig()
	placement.Runtime = DaemonRuntimeSsh
	host := "test-box"
	placement.Host = &host
	return placement
}

func daemonTestRead(t *testing.T, path string) string {
	t.Helper()
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(body)
}

func daemonTestLoadPlacement(t *testing.T, path string) DaemonConfig {
	t.Helper()
	placement, err := DaemonConfigLoadPlacement(path)
	if err != nil {
		t.Fatal(err)
	}
	return placement
}

// Hard-won: 916f27ded: the first-launch save wrote a file with only
// `[daemon]`, which the local daemon refuses to load once `runtime` is
// switched back to `host`; and an explicit `runtime = "host"` looked unset.
func TestAPlacementFileMadeHereLoadsAsAConfigAndExplicitHostCountsAsSet(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	if set, err := DaemonConfigIsSetIn(path); err != nil || set {
		t.Fatalf("set %v, %v", set, err)
	}
	ssh := daemonTestSSHPlacement()
	if err := ssh.ReplacePlacement(path, nil); err != nil {
		t.Fatal(err)
	}
	if written := daemonTestRead(t, path); !strings.HasPrefix(written, "version = ") {
		t.Fatal(written)
	}
	if _, err := ConfigLoadFrom(path); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte("version = 23\n[daemon]\nruntime = \"host\"\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if set, err := DaemonConfigIsSetIn(path); err != nil || !set {
		t.Fatalf("set %v, %v", set, err)
	}
	if !daemonTestLoadPlacement(t, path).IsDefault() {
		t.Fatal("explicit host loads as the default")
	}
	// A launch that saw no table must not overwrite one added since.
	refused := ssh.ReplacePlacement(path, nil)
	if refused == nil || !strings.Contains(refused.Error(), "it is now set") {
		t.Fatalf("refusal %v", refused)
	}
	if !daemonTestLoadPlacement(t, path).IsDefault() {
		t.Fatal("the refused launch replaced the placement")
	}
}

// The placement is written into a file the daemon machine owns, so every
// other table and comment must survive, and choosing the default placement
// again must leave no empty `[daemon]` table behind.
func TestSavingAPlacementKeepsTheRestOfTheFileAndRoundTrips(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.toml")
	others := "# kept comment\nprofiles = \"owned by remote\"\n\n[phone]\nenabled = false\n"
	if err := os.WriteFile(path, []byte(others), 0o644); err != nil {
		t.Fatal(err)
	}
	ssh := daemonTestSSHPlacement()
	if err := ssh.ReplacePlacement(path, nil); err != nil {
		t.Fatal(err)
	}
	if written := daemonTestRead(t, path); !strings.HasPrefix(written, others) {
		t.Fatal(written)
	}
	if got := daemonTestLoadPlacement(t, path); !daemonConfigOptionEqual(&got, &ssh) {
		t.Fatalf("placement %+v", got)
	}

	// A launch that decided from an older file is refused, not applied.
	stale := DefaultDaemonConfig().ReplacePlacement(path, nil)
	if stale == nil || !strings.Contains(stale.Error(), "it now names test-box") {
		t.Fatalf("refusal %v", stale)
	}
	if err := DefaultDaemonConfig().ReplacePlacement(path, &ssh); err != nil {
		t.Fatal(err)
	}
	if got := strings.TrimRight(daemonTestRead(t, path), " \t\r\n"); got != strings.TrimRight(others, " \t\r\n") {
		t.Fatalf("file %q", got)
	}
}
