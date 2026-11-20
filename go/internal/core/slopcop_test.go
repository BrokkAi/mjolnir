// Port of the tests in mj-core/src/slopcop.rs.

package core

import (
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestHostLoginFollowsSlopcopAndReportsAnUnreadableConfigurationAsUnknown(t *testing.T) {
	home := t.TempDir()
	file := filepath.Join(home, "slopcop", "config.json")
	unset := func(string) (string, bool) { return "", false }
	resolve := func(environment func(string) (string, bool)) (*SlopcopLogin, error) {
		return ResolveLogin(environment, &home)
	}
	login := func(token string, serviceURL *string) *SlopcopLogin {
		t.Helper()
		value, err := NewSlopcopLogin(token, serviceURL)
		if err != nil {
			t.Fatal(err)
		}
		return &value
	}
	if got, err := resolve(unset); err != nil || got != nil {
		t.Fatalf("no configuration = %v, %v", got, err)
	}

	// The file `slopcop login` writes: indented JSON with a trailing newline.
	if err := os.MkdirAll(filepath.Dir(file), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(file, []byte("{\n  \"serviceUrl\": \"https://slopcop.example\",\n  \"token\": \"stored-token\"\n}\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	service := "https://slopcop.example"
	if got, err := resolve(unset); err != nil || !reflect.DeepEqual(got, login("stored-token", &service)) {
		t.Fatalf("stored = %v, %v", got, err)
	}

	// SLOPCOP_TOKEN wins over the stored token; the stored URL still applies.
	token := func(name string) (string, bool) {
		if name == TokenEnv {
			return " env-token\n", true
		}
		return "", false
	}
	if got, err := resolve(token); err != nil || !reflect.DeepEqual(got, login("env-token", &service)) {
		t.Fatalf("environment = %v, %v", got, err)
	}

	// `slopcop logout` keeps the service URL and drops the token.
	if err := os.WriteFile(file, []byte("{\n  \"serviceUrl\": \"https://slopcop.example\"\n}\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if got, err := resolve(unset); err != nil || got != nil {
		t.Fatalf("logged out = %v, %v", got, err)
	}

	// A configuration that cannot be read says nothing about the login.
	if err := os.WriteFile(file, []byte("{ \"token\": "), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := resolve(unset); err == nil {
		t.Fatal("an unreadable configuration reads as signed out")
	}
}
