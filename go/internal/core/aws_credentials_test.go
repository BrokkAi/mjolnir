// Port of the tests in mj-core/src/aws_credentials.rs.

package core

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func awsTestCredentials(t *testing.T) AwsCredentials {
	t.Helper()
	credentials, err := AwsCredentialsFromExport("bedrock", []byte(
		`{"Version":1,"AccessKeyId":"key","SecretAccessKey":"secret","SessionToken":"token","Expiration":"2099-01-01T00:00:00Z"}`))
	if err != nil {
		t.Fatal(err)
	}
	return credentials
}

// One rule for profile names: what launch accepts when it stages credentials
// is what is read back from the staged configuration.
func TestStagingAndReadingAcceptTheSameProfileNames(t *testing.T) {
	home := t.TempDir()
	staged := func(name string) error {
		credentials, err := awsTestCredentials(t).WithProfile(name)
		if err != nil {
			return err
		}
		_, err = credentials.Install(home, home)
		return err
	}
	for _, name := range []string{"bedrock", "dev@example team", "team.a_b-c", "a/b"} {
		if err := staged(name); err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		profile, ok, err := AwsCredentialsStagedProfile(home)
		if err != nil || !ok || profile != name {
			t.Fatalf("%s: got %q %v %v", name, profile, ok, err)
		}
	}
	// Launch refuses what cannot be a section name, and a configuration that
	// has such a header (a session can write one) has no profile.
	for _, name := range []string{"", "a]b", "a[b", "a\nb"} {
		if staged(name) == nil {
			t.Fatalf("%q was staged", name)
		}
		if AwsCredentialsValidProfileName(name) {
			t.Fatalf("%q is valid", name)
		}
	}
	for _, header := range []string{"[profile a]b]", "[profile ]", "[default]"} {
		if err := os.WriteFile(filepath.Join(home, AwsCredentialsConfigFile), []byte(header+"\n"), 0o666); err != nil {
			t.Fatal(err)
		}
		if profile, ok, err := AwsCredentialsStagedProfile(home); err != nil || ok {
			t.Fatalf("%s: got %q %v %v", header, profile, ok, err)
		}
	}
	if err := os.Remove(filepath.Join(home, AwsCredentialsConfigFile)); err != nil {
		t.Fatal(err)
	}
	if profile, ok, err := AwsCredentialsStagedProfile(home); err != nil || ok {
		t.Fatalf("got %q %v %v", profile, ok, err)
	}
}

// Launch writes an SSH worker's profile path relative to the user's home; the
// worker has resolved its own home to an absolute path before it compares.
// Those are two spellings of one path, so valid credentials are present, not
// absent: reading them as absent would have the next sync withdraw them.
func TestARelativeLaunchPathAndAnAbsoluteWorkerHomeNameTheSameSecret(t *testing.T) {
	userHome := t.TempDir()
	relative := filepath.Join(".local", "share", "hel", "workers", "s", "profile")
	home := filepath.Join(userHome, relative)
	if err := os.MkdirAll(home, 0o777); err != nil {
		t.Fatal(err)
	}
	present := func(user *string) bool {
		t.Helper()
		state, err := AwsCredentialsStateForUser(home, user)
		if err != nil {
			t.Fatal(err)
		}
		return state.Present
	}
	install := func(targetHome string) {
		t.Helper()
		if _, err := awsTestCredentials(t).Install(home, targetHome); err != nil {
			t.Fatal(err)
		}
	}

	install(relative)
	configuration, err := os.ReadFile(filepath.Join(home, AwsCredentialsConfigFile))
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(string(configuration), `"$HOME"`) {
		t.Fatalf("launch wrote the relative spelling: %s", configuration)
	}
	if !present(&userHome) {
		t.Fatal("the relative spelling names this secret")
	}

	// The absolute spelling (a local worker) is the same secret too.
	install(home)
	if !present(nil) {
		t.Fatal("the absolute spelling names this secret")
	}

	// A configuration naming another secret, or a relative one that cannot be
	// resolved, is not this session's.
	install(filepath.Join(".local", "share", "hel", "workers", "other", "profile"))
	if present(&userHome) {
		t.Fatal("another secret is not this session's")
	}
	install(relative)
	if present(nil) {
		t.Fatal("an unresolvable relative path is not this session's")
	}
	// Nor is a configuration for another profile.
	configuration, err = os.ReadFile(filepath.Join(home, AwsCredentialsConfigFile))
	if err != nil {
		t.Fatal(err)
	}
	other := strings.ReplaceAll(string(configuration), "[profile bedrock]", "[profile other]")
	if err := os.WriteFile(filepath.Join(home, AwsCredentialsConfigFile), []byte(other), 0o666); err != nil {
		t.Fatal(err)
	}
	if present(&userHome) {
		t.Fatal("another profile's configuration is not this session's")
	}
}
