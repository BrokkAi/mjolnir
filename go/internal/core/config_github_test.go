// Port of the tests in mj-core/src/config/github.rs.

package core

import (
	"reflect"
	"testing"
)

func TestOmittedGithubConfigIsDefaultAndSerializesAway(t *testing.T) {
	var value GithubConfig
	if err := unmarshalTOML("", &value); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(value, DefaultGithubConfig()) {
		t.Fatalf("%+v", value)
	}
	written, err := marshalTOMLPretty(value)
	if err != nil {
		t.Fatal(err)
	}
	if written != "" {
		t.Fatalf("%q", written)
	}
}

func TestAppConfigRejectsInvalidOwnerAndInstallationIds(t *testing.T) {
	valid := GithubAppConfig{AppID: 42, PrivateKeyPath: "app.pem", Installations: map[string]uint64{}}
	if err := valid.Validate(); err != nil {
		t.Fatal(err)
	}

	invalid := valid
	invalid.Installations = map[string]uint64{"two--hyphens": 1}
	if invalid.Validate() == nil {
		t.Fatal("an invalid owner login validated")
	}

	invalid = valid
	invalid.Installations = map[string]uint64{"acme": 0}
	if invalid.Validate() == nil {
		t.Fatal("installation ID 0 validated")
	}

	ambiguous := GithubAppConfig{
		AppID:          42,
		PrivateKeyPath: "app.pem",
		Installations:  map[string]uint64{"Acme": 1, "acme": 2},
	}
	if ambiguous.Validate() == nil {
		t.Fatal("owners differing only in case validated")
	}
}

func TestPermissionTablesDistinguishAbsentFromPresentEmptyAndValidateNames(t *testing.T) {
	var absent GithubAppConfig
	if err := unmarshalTOML("app_id = 42\nprivate_key_path = 'app.pem'\n", &absent); err != nil {
		t.Fatal(err)
	}
	if absent.SessionPermissions != nil || absent.TokenPermissions != nil {
		t.Fatalf("%+v", absent)
	}

	var empty GithubAppConfig
	if err := unmarshalTOML("app_id = 42\nprivate_key_path = 'app.pem'\n[session_permissions]\n", &empty); err != nil {
		t.Fatal(err)
	}
	if empty.SessionPermissions == nil || len(*empty.SessionPermissions) != 0 {
		t.Fatalf("%+v", empty.SessionPermissions)
	}

	var configured GithubAppConfig
	if err := unmarshalTOML(
		"app_id = 42\nprivate_key_path = 'app.pem'\n[session_permissions]\ncontents = 'write'\nstatuses = 'read'\n",
		&configured,
	); err != nil {
		t.Fatal(err)
	}
	want := GithubPermissionSet{"contents": GithubPermissionLevelWrite, "statuses": GithubPermissionLevelRead}
	if configured.SessionPermissions == nil || !reflect.DeepEqual(*configured.SessionPermissions, want) {
		t.Fatalf("%+v", configured.SessionPermissions)
	}
	if err := configured.Validate(); err != nil {
		t.Fatal(err)
	}

	var invalidName GithubAppConfig
	if err := unmarshalTOML(
		"app_id = 42\nprivate_key_path = 'app.pem'\n[session_permissions]\n'Contents' = 'read'\n",
		&invalidName,
	); err != nil {
		t.Fatal(err)
	}
	if invalidName.Validate() == nil {
		t.Fatal("an invalid permission name validated")
	}

	var invalidLevel GithubAppConfig
	if unmarshalTOML(
		"app_id = 42\nprivate_key_path = 'app.pem'\n[session_permissions]\ncontents = 'admin'\n",
		&invalidLevel,
	) == nil {
		t.Fatal("an invalid permission level read")
	}
}
