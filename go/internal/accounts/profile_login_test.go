// Port of the tests in mj-accounts/src/profile_login.rs.

package accounts

import (
	"os"
	"path/filepath"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func profileLoginProfile(kind core.HarnessKind, home string) core.HarnessProfile {
	return core.HarnessProfile{
		ThirdPartyModels: true,
		Enabled:          true,
		Kind:             kind,
		Home:             home,
		MjolnirGuardian:  true,
	}
}

func TestAProfileWithoutItsLoginFileIsSignedOut(t *testing.T) {
	home := t.TempDir()
	profile := profileLoginProfile(core.HarnessKindCodex, home)
	if got := ProfileLoginStatus("codex", &profile, core.ProcessExecutor{}); got != (core.LoginStatusSignedOut{}) {
		t.Fatalf("status = %#v", got)
	}
}

func TestAPresentLoginFileIsSignedInAndDoesNotParseSecrets(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte("{}"), 0o644); err != nil {
		t.Fatal(err)
	}
	profile := profileLoginProfile(core.HarnessKindCodex, home)
	if got := ProfileLoginStatus("codex", &profile, core.ProcessExecutor{}); got != (core.LoginStatusSignedIn{}) {
		t.Fatalf("status = %#v", got)
	}
}

func TestAnUnreadableLoginFileIsReportedAsUnknown(t *testing.T) {
	home := t.TempDir()
	if err := os.WriteFile(filepath.Join(home, "auth.json"), []byte("not json"), 0o644); err != nil {
		t.Fatal(err)
	}
	profile := profileLoginProfile(core.HarnessKindCodex, home)
	if got, ok := ProfileLoginStatus("codex", &profile, core.ProcessExecutor{}).(core.LoginStatusUnknown); !ok {
		t.Fatalf("status = %#v", got)
	}
}

func TestAnAPIKeyProfileSaysWhichKeyItUses(t *testing.T) {
	home := t.TempDir()
	profile := profileLoginProfile(core.HarnessKindDsh, home)
	profile.Environment.Insert("DEEPSEEK_API_KEY", "secret")
	status := ProfileLoginStatus("dsh", &profile, core.ProcessExecutor{})
	if status != (core.LoginStatusNotRequired{Detail: "DEEPSEEK_API_KEY API key"}) {
		t.Fatalf("status = %#v", status)
	}
	if status.Label() != "DEEPSEEK_API_KEY API key" {
		t.Fatalf("label = %q", status.Label())
	}
}

func TestAnOpencodeProfileWithoutADatabaseIsSignedOut(t *testing.T) {
	home := t.TempDir()
	profile := profileLoginProfile(core.HarnessKindOpenCode, home)
	if got := ProfileLoginStatus("opencode", &profile, core.ProcessExecutor{}); got != (core.LoginStatusSignedOut{}) {
		t.Fatalf("status = %#v", got)
	}
}
