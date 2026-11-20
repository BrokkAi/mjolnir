// Port of the tests in mj-core/src/config/secrets.rs.

package core

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strings"
	"testing"
)

func fixedSecretResolver() SecretResolver {
	return SecretResolverFixed(
		map[string]string{"FROM_PROCESS": "process-value"},
		map[string]string{"STORED": "stored-value"},
	)
}

// readEnvironmentTOML is `toml::from_str::<Environment>` under the scope the
// caller opened.
func readEnvironmentTOML(text string) (Environment, error) {
	var environment Environment
	err := unmarshalTOML(text, &environment)
	return environment, err
}

func readEnvironmentJSON(data []byte) (Environment, error) {
	var environment Environment
	err := json.Unmarshal(data, &environment)
	return environment, err
}

type environmentRead struct {
	environment Environment
	err         error
}

func TestEveryReferenceFormResolvesAndSerializesAsWritten(t *testing.T) {
	text := "LITERAL = \"plain\"\nA = { from_env = \"FROM_PROCESS\" }\nB = { from_secret = \"STORED\" }\n"
	read := WithSecretResolver(fixedSecretResolver(), func() environmentRead {
		environment, err := readEnvironmentTOML(text)
		return environmentRead{environment, err}
	})
	if read.err != nil {
		t.Fatal(read.err)
	}
	environment := read.environment
	for key, want := range map[string]string{"LITERAL": "plain", "A": "process-value", "B": "stored-value"} {
		if got, ok := environment.Get(key); !ok || got != want {
			t.Fatalf("%s = %q, %v", key, got, ok)
		}
	}
	if got := environment.Sources()["A"]; got != (EnvironmentValueFromEnv{Value: "FROM_PROCESS"}) {
		t.Fatalf("A's source = %#v", got)
	}
	written, err := marshalTOMLPretty(environment)
	if err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{`from_env = "FROM_PROCESS"`, `from_secret = "STORED"`} {
		if !strings.Contains(written, want) {
			t.Fatal(written)
		}
	}
	for _, secret := range []string{"process-value", "stored-value"} {
		if strings.Contains(written, secret) {
			t.Fatal(written)
		}
	}
	encoded, err := json.Marshal(environment)
	if err != nil {
		t.Fatal(err)
	}
	var value map[string]any
	if err := json.Unmarshal(encoded, &value); err != nil {
		t.Fatal(err)
	}
	if got := value["B"].(map[string]any)["from_secret"]; got != "STORED" {
		t.Fatalf("B = %v", value["B"])
	}
	back := WithSecretResolver(fixedSecretResolver(), func() environmentRead {
		environment, err := readEnvironmentJSON(encoded)
		return environmentRead{environment, err}
	})
	if back.err != nil {
		t.Fatal(back.err)
	}
	if !back.environment.Equal(environment) {
		t.Fatalf("%+v != %+v", back.environment, environment)
	}
}

func TestSourceProjectionsNeedNoCredentialsAndPreserveReferencesForBackgroundResolution(t *testing.T) {
	document := []byte(`{"A": {"from_env": "UNDEFINED"}, "B": {"from_secret": "UNDEFINED"}, "C": "literal"}`)
	var want any
	if err := json.Unmarshal(document, &want); err != nil {
		t.Fatal(err)
	}
	WithSecretResolver(SecretResolverFixed(map[string]string{}, map[string]string{}), func() struct{} {
		projected := WithEnvironmentSourcesOnly(func() environmentRead {
			environment, err := readEnvironmentJSON(document)
			return environmentRead{environment, err}
		})
		if projected.err != nil {
			t.Fatal(projected.err)
		}
		if len(projected.environment.Resolved()) != 0 {
			t.Fatalf("%v", projected.environment.Resolved())
		}
		encoded, err := json.Marshal(projected.environment)
		if err != nil {
			t.Fatal(err)
		}
		var got any
		if err := json.Unmarshal(encoded, &got); err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(got, want) {
			t.Fatalf("%s", encoded)
		}
		resolved, err := readEnvironmentJSON(document)
		if err != nil {
			t.Fatal(err)
		}
		if len(resolved.Resolved()) != 1 {
			t.Fatalf("%v", resolved.Resolved())
		}
		if resolved.EnsureResolved() == nil {
			t.Fatal("normal reads still resolve, and report what is missing")
		}
		malformed := WithEnvironmentSourcesOnly(func() environmentRead {
			environment, err := readEnvironmentJSON([]byte(`{"A": {"from_secret": ""}}`))
			return environmentRead{environment, err}
		})
		if malformed.err == nil {
			t.Fatal("projections still validate source syntax")
		}
		return struct{}{}
	})
}

// Hard-won: 871a4d9: an absent secret made config loading fail and blocked daemon startup/upgrade.
func TestAMissingSecretOrVariableStillReadsAndNamesWhatToSet(t *testing.T) {
	missingSecret := WithSecretResolver(fixedSecretResolver(), func() environmentRead {
		environment, err := readEnvironmentTOML("KEY = { from_secret = \"ABSENT\" }\nOTHER = \"kept\"")
		return environmentRead{environment, err}
	})
	if missingSecret.err != nil {
		t.Fatal(missingSecret.err)
	}
	if _, ok := missingSecret.environment.Get("KEY"); ok {
		t.Fatal("an absent secret has a value")
	}
	if got, _ := missingSecret.environment.Get("OTHER"); got != "kept" {
		t.Fatalf("OTHER = %q", got)
	}
	err := missingSecret.environment.EnsureResolved()
	if err == nil || !strings.Contains(err.Error(), "ABSENT") || !strings.Contains(err.Error(), "KEY") {
		t.Fatalf("%v", err)
	}
	missingVariable := WithSecretResolver(fixedSecretResolver(), func() environmentRead {
		environment, err := readEnvironmentTOML("KEY = { from_env = \"ABSENT\" }")
		return environmentRead{environment, err}
	})
	if missingVariable.err != nil {
		t.Fatal(missingVariable.err)
	}
	environment := missingVariable.environment
	err = environment.EnsureResolved()
	if err == nil || !strings.Contains(err.Error(), "ABSENT set") {
		t.Fatalf("%v", err)
	}
	// A save still writes the reference back.
	written, err := marshalTOMLPretty(environment)
	if err != nil {
		t.Fatal(err)
	}
	if !strings.Contains(written, `from_env = "ABSENT"`) {
		t.Fatal(written)
	}
	environment.Insert("KEY", "set")
	if err := environment.EnsureResolved(); err != nil {
		t.Fatal(err)
	}
}

func TestTheSecretsFileIsAFlatTableOfStrings(t *testing.T) {
	directory := t.TempDir()
	path := filepath.Join(directory, SecretsFile)
	write := func(text string) {
		t.Helper()
		if err := os.WriteFile(path, []byte(text), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write("A = \"1\"\nB = \"2\"\n")
	secrets, err := LoadSecrets(path)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(secrets, map[string]string{"A": "1", "B": "2"}) {
		t.Fatalf("%v", secrets)
	}
	write("A = 1\n")
	if _, err := LoadSecrets(path); err == nil || !strings.Contains(err.Error(), "`A` must be a string") {
		t.Fatalf("%v", err)
	}
	write("[nested]\nA = \"1\"\n")
	if _, err := LoadSecrets(path); err == nil {
		t.Fatal("a nested table loaded")
	}
	config := filepath.Join(directory, "config.toml")
	write("STORED = \"from-file\"\n")
	read := WithSecretResolver(SecretResolverBeside(config), func() environmentRead {
		environment, err := readEnvironmentTOML("KEY = { from_secret = \"STORED\" }")
		return environmentRead{environment, err}
	})
	if read.err != nil {
		t.Fatal(read.err)
	}
	if got, _ := read.environment.Get("KEY"); got != "from-file" {
		t.Fatalf("KEY = %q", got)
	}
}

func TestASharedSecretsFileIsReported(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("file modes are a Unix check")
	}
	path := filepath.Join(t.TempDir(), SecretsFile)
	if err := os.WriteFile(path, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o644); err != nil {
		t.Fatal(err)
	}
	if _, found, err := SecretsPermissionProblem(path); err != nil || !found {
		t.Fatalf("found %v, %v", found, err)
	}
	if err := os.Chmod(path, 0o600); err != nil {
		t.Fatal(err)
	}
	if _, found, err := SecretsPermissionProblem(path); err != nil || found {
		t.Fatalf("found %v, %v", found, err)
	}
}
