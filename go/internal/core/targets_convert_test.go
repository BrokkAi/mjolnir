// Port of the tests in mj-core/src/targets/convert.rs.

package core

import (
	"encoding/json"
	"errors"
	"strings"
	"testing"
)

// hasArgumentPair reports whether args holds first immediately followed by
// second (Rust: args.windows(2).any(|pair| pair == [first, second])).
func hasArgumentPair(args []string, first, second string) bool {
	for i := 0; i+1 < len(args); i++ {
		if args[i] == first && args[i+1] == second {
			return true
		}
	}
	return false
}

func conversionErrorKind(err error) (TargetConversionErrorKind, bool) {
	var conversion *TargetConversionError
	if !errors.As(err, &conversion) {
		return 0, false
	}
	return conversion.Kind, true
}

func TestEverySshLocatorChecksHostAndPreservesTheSavedConnection(t *testing.T) {
	user := "builder"
	identity := "/keys/my key"
	connection := SshConnection{
		Host:         "original.test",
		User:         &user,
		IdentityFile: &identity,
		ExtraArgs:    []string{"-p", "2222"},
	}
	owner := "owner"
	locators := []TargetLocator{
		TargetLocatorSshBare{Host: connection.Host, Workspace: "workspace"},
		TargetLocatorSshPodman{Host: connection.Host, ContainerID: "podman-id", WorkspaceStorage: PodmanWorkspaceLocatorContainerLayer{}, BorrowedFrom: &owner},
		TargetLocatorSshDocker{Host: connection.Host, ContainerID: "docker-id"},
		TargetLocatorSshAppleContainer{Host: connection.Host, ContainerID: "apple-id"},
		TargetLocatorSshNitro{Host: connection.Host, ContainerID: "nitro-id"},
	}
	for _, locator := range locators {
		runtime := TargetRuntimeSettingsFromTemplate(TargetTemplateLocalBare{})
		runtime.Kind = LocatorKindName(locator)
		runtime.Connection = TargetConnectionSsh{SSH: connection}
		backend, err := TargetsTargetLocatorFromRecordedTarget(RecordedTarget{Locator: locator, Runtime: &runtime, SessionID: "session"})
		if err != nil {
			t.Fatal(err)
		}
		if backend.SSH == nil {
			t.Fatalf("%s has no ssh target", backend.Kind)
		}
		ssh := backend.SSH
		if ssh.Destination != "builder@original.test" {
			t.Fatal(ssh.Destination)
		}
		if !hasArgumentPair(ssh.SSHArgs, "-i", "/keys/my key") || !hasArgumentPair(ssh.SSHArgs, "-p", "2222") {
			t.Fatalf("%v", ssh.SSHArgs)
		}
		replaced := connection
		replaced.Host = "replacement.test"
		runtime.Connection = TargetConnectionSsh{SSH: replaced}
		_, err = TargetsTargetLocatorFromRecordedTarget(RecordedTarget{Locator: locator, Runtime: &runtime, SessionID: "session"})
		if kind, ok := conversionErrorKind(err); !ok || kind != TargetConversionErrorSshHostMismatch {
			t.Fatalf("%v", err)
		}
		runtime.Kind = "local-bare"
		_, err = TargetsTargetLocatorFromRecordedTarget(RecordedTarget{Locator: locator, Runtime: &runtime, SessionID: "session"})
		if kind, ok := conversionErrorKind(err); !ok || kind != TargetConversionErrorRecordedKindMismatch {
			t.Fatalf("%v", err)
		}
	}
}

// Hard-won: #1018: removing a target template left running sessions without the access data needed to reconnect.
func TestRecordedEc2AccessPreservesRegionProfileAndIdentityWithoutLaunchTemplate(t *testing.T) {
	template, err := UnmarshalTargetTemplateJSON([]byte(`{
		"kind":"aws-ec2", "aws_profile":"production", "region":"eu-west-1",
		"launch_template":"creation-only", "ssh_user":"ubuntu", "identity_file":"/keys/ec2",
		"ssh_args":["-p","2222"]
	}`))
	if err != nil {
		t.Fatal(err)
	}
	runtime := TargetRuntimeSettingsFromTemplate(template)
	encoded, err := json.Marshal(runtime)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), "creation-only") {
		t.Fatal(string(encoded))
	}
	var decoded TargetRuntimeSettings
	if err := json.Unmarshal(encoded, &decoded); err != nil {
		t.Fatal(err)
	}
	address := "10.0.0.1"
	locator := TargetLocatorAwsEc2{InstanceID: "i-original", Address: &address}
	backend, err := TargetsTargetLocatorFromRecordedTarget(RecordedTarget{Locator: locator, Runtime: &decoded, SessionID: "session"})
	if err != nil {
		t.Fatal(err)
	}
	if backend.Kind != TargetsTargetLocatorAwsEc2 {
		t.Fatal(backend.Kind)
	}
	if backend.Profile != "production" || backend.Region != "eu-west-1" || backend.InstanceID != "i-original" {
		t.Fatalf("%+v", backend)
	}
	if backend.SSH.Destination != "ubuntu@10.0.0.1" {
		t.Fatal(backend.SSH.Destination)
	}
	if !hasArgumentPair(backend.SSH.SSHArgs, "-i", "/keys/ec2") || !hasArgumentPair(backend.SSH.SSHArgs, "-p", "2222") {
		t.Fatalf("%v", backend.SSH.SSHArgs)
	}
}

// effectiveSSHOption is the value OpenSSH would use for key: the first
// `-o key=value` (or `-okey=value`) on the command line wins, as `ssh -G`
// shows.
func effectiveSSHOption(args []string, key string) (string, bool) {
	for i := 0; i < len(args); i++ {
		arg := args[i]
		var option string
		if arg == "-o" {
			if i+1 >= len(args) {
				continue
			}
			i++
			option = args[i]
		} else if rest, ok := strings.CutPrefix(arg, "-o"); ok {
			option = rest
		} else {
			continue
		}
		name, value := option, ""
		if index := strings.IndexAny(option, "= "); index >= 0 {
			name, value = option[:index], option[index+1:]
		}
		if strings.EqualFold(strings.TrimSpace(name), key) {
			return strings.TrimSpace(value), true
		}
	}
	return "", false
}

// Hard-won: aa4e0b51: OpenSSH used Mjolnir accept-new before the user's strict host-key requirement.
func TestUserExtraArgsCanRequireStrictHostKeyChecking(t *testing.T) {
	args := SSHArgsWithIdentity([]string{"-o", "StrictHostKeyChecking=yes"}, nil)
	if value, ok := effectiveSSHOption(args, "StrictHostKeyChecking"); !ok || value != "yes" {
		t.Fatalf("%q", value)
	}
	args = SSHArgsWithIdentity([]string{"-oUserKnownHostsFile=/k"}, nil)
	if value, ok := effectiveSSHOption(args, "UserKnownHostsFile"); !ok || value != "/k" {
		t.Fatalf("%q", value)
	}
}
