// Port of the tests in mj-worker/src/worker_runtime/gh_rules.rs.

//go:build unix

package worker

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

const ghRulesSession = "gh-policy-session"

func ghRulesStagedProfile(t *testing.T) (string, string) {
	t.Helper()
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	home := filepath.Join(root, "profile")
	if err := os.MkdirAll(home, 0o777); err != nil {
		t.Fatal(err)
	}
	return root, home
}

func ghRulesConfigure(root, sessionID, home string, harness core.HarnessKind, policy core.ExecutionPolicy, environment map[string]string) error {
	return configureNativeRules(root, sessionID, home, harness, policy, openCodeDenials{nativeSubagent: false}, environment)
}

func ghRulesRead(t *testing.T, path string) string {
	t.Helper()
	body, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return string(body)
}

func ghRulesWrite(t *testing.T, path, body string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(body), 0o666); err != nil {
		t.Fatal(err)
	}
}

func TestCodexProfilePrepInstallsTheRuleUnderEveryPolicy(t *testing.T) {
	for _, policy := range []core.ExecutionPolicy{core.ExecutionPolicyConfiguredApprovals, core.ExecutionPolicyUnconstrained} {
		root, home := ghRulesStagedProfile(t)
		rules := filepath.Join(home, "rules")
		if err := os.Mkdir(rules, 0o777); err != nil {
			t.Fatal(err)
		}
		user := "prefix_rule(pattern=[\"git\"], decision=\"prompt\")\n"
		ghRulesWrite(t, filepath.Join(rules, "user.rules"), user)
		environment := map[string]string{}
		if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindCodex, policy, environment); err != nil {
			t.Fatal(err)
		}
		path := filepath.Join(home, "rules/mj-gh.rules")
		if got := ghRulesRead(t, path); got != codexGhRule {
			t.Fatalf("rule = %q", got)
		}
		if got := ghRulesRead(t, filepath.Join(rules, "user.rules")); got != user {
			t.Fatalf("user rule = %q", got)
		}
		ghRulesWrite(t, path, "obsolete managed rule")
		if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindCodex, policy, environment); err != nil {
			t.Fatal(err)
		}
		if got := ghRulesRead(t, path); got != codexGhRule {
			t.Fatalf("rule = %q", got)
		}
	}
}

// TestClaudeProfileSettingsAreNeverEditedForTheGhApproval: Claude's approval
// rides the session-open request, so its staged settings — whether Mjolnir
// staged them or they are the person's own — are never edited for it.
func TestClaudeProfileSettingsAreNeverEditedForTheGhApproval(t *testing.T) {
	root, home := ghRulesStagedProfile(t)
	path := filepath.Join(home, "settings.json")
	original := `{"model":"opus","permissions":{"allow":["Bash(ls:*)"],"deny":["WebFetch"]}}`
	ghRulesWrite(t, path, original)
	environment := map[string]string{}
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindClaude, core.ExecutionPolicyConfiguredApprovals, environment); err != nil {
		t.Fatal(err)
	}
	if got := ghRulesRead(t, path); got != original {
		t.Fatalf("settings = %q", got)
	}
	if len(environment) != 0 {
		t.Fatalf("environment = %v", environment)
	}
	if core.HarnessKindClaude.GhApproval() != (core.GhApprovalSessionOptions{}) {
		t.Fatal("Claude takes its approval in the session options")
	}
}

func TestNonGuardianLaunchesLeaveTheJSONConfigurationAlone(t *testing.T) {
	root, home := ghRulesStagedProfile(t)
	settings := filepath.Join(home, "settings.json")
	ghRulesWrite(t, settings, "{}")
	environment := map[string]string{}
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindKimi, core.ExecutionPolicyUnconstrained, environment); err != nil {
		t.Fatal(err)
	}
	if got := ghRulesRead(t, settings); got != "{}" {
		t.Fatalf("settings = %q", got)
	}
	if _, err := os.Stat(filepath.Join(home, "config.toml")); err == nil {
		t.Fatal("no configuration may be written")
	}
	if len(environment) != 0 {
		t.Fatalf("environment = %v", environment)
	}
}

// TestOpencodeRulesReachTheDocumentMjolnirSuppliesLast: a YOLO OpenCode
// launch keeps the denials it needs in the document Mjolnir supplies last,
// and carries no `gh` allow; a guardian launch carries both. Nothing is read
// from or written to a staged home.
func TestOpencodeRulesReachTheDocumentMjolnirSuppliesLast(t *testing.T) {
	root, home := ghRulesStagedProfile(t)
	for _, check := range []struct {
		policy   core.ExecutionPolicy
		approved bool
	}{
		{core.ExecutionPolicyUnconstrained, false},
		{core.ExecutionPolicyConfiguredApprovals, true},
	} {
		environment := map[string]string{}
		if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindOpenCode, check.policy, environment); err != nil {
			t.Fatal(err)
		}
		content := environment["OPENCODE_CONFIG_CONTENT"]
		if strings.Contains(content, `"gh"`) != check.approved {
			t.Fatalf("%v: %s", check.policy, content)
		}
		if !strings.Contains(content, `"execute"`) {
			t.Fatalf("%v: %s", check.policy, content)
		}
	}
	if _, err := os.Stat(filepath.Join(home, "opencode.json")); err == nil {
		t.Fatal("the staged configuration file is not rewritten")
	}
}

func TestKimiProfilePrepPreservesConfiguredRulesAndIsIdempotent(t *testing.T) {
	root, home := ghRulesStagedProfile(t)
	path := filepath.Join(home, "config.toml")
	ghRulesWrite(t, path, `
                [agent]
                name = "reviewer"
                [permission]
                allow = ["Read"]
                deny = ["Bash(rm *)"]
                ask = ["Bash(git push *)"]
                rules = [{ decision = "deny", pattern = "Bash(gh *)" }]
            `)
	environment := map[string]string{}
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindKimi, core.ExecutionPolicyConfiguredApprovals, environment); err != nil {
		t.Fatal(err)
	}
	once := ghRulesRead(t, path)
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindKimi, core.ExecutionPolicyConfiguredApprovals, environment); err != nil {
		t.Fatal(err)
	}
	if got := ghRulesRead(t, path); got != once {
		t.Fatalf("second preparation changed the file:\n%s\n%s", once, got)
	}
	config, err := core.TOMLTableFromStr(once)
	if err != nil {
		t.Fatal(err)
	}
	permission := config["permission"].(map[string]any)
	for _, pair := range [][2]string{{"deny", "Bash(rm *)"}, {"ask", "Bash(git push *)"}} {
		if permission[pair[0]].([]any)[0] != pair[1] {
			t.Fatalf("%s = %v", pair[0], permission[pair[0]])
		}
	}
	if config["agent"].(map[string]any)["name"] != "reviewer" {
		t.Fatalf("agent = %v", config["agent"])
	}
	found := false
	for _, rule := range permission["rules"].([]any) {
		table := rule.(map[string]any)
		if table["decision"] == "allow" && table["pattern"] == "Bash(gh)" {
			found = true
		}
	}
	if !found {
		t.Fatalf("rules = %v", permission["rules"])
	}
}

func TestGrokProfilePrepPreservesDenyAndAskRules(t *testing.T) {
	root, home := ghRulesStagedProfile(t)
	path := filepath.Join(home, "config.toml")
	ghRulesWrite(t, path, `[permission]
allow = ["Read"]
deny = ["Bash(rm *)"]
ask = ["Bash(git push *)"]
rules = [{ decision = "deny", pattern = "Bash(gh)" }]
`)
	environment := map[string]string{}
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindGrok, core.ExecutionPolicyConfiguredApprovals, environment); err != nil {
		t.Fatal(err)
	}
	config, err := core.TOMLTableFromStr(ghRulesRead(t, path))
	if err != nil {
		t.Fatal(err)
	}
	permission := config["permission"].(map[string]any)
	for _, pair := range [][2]string{{"allow", "Read"}, {"deny", "Bash(rm *)"}, {"ask", "Bash(git push *)"}} {
		if permission[pair[0]].([]any)[0] != pair[1] {
			t.Fatalf("%s = %v", pair[0], permission[pair[0]])
		}
	}
}

func TestMuseProfilePrepAddsTheGhPrefixOnce(t *testing.T) {
	root, home := ghRulesStagedProfile(t)
	// The table names where each harness accepts the approval; the writer
	// must use that same variable.
	if core.HarnessKindMuse.GhApproval() != (core.GhApprovalAdapterEnvironment{Value: musePrefixes}) {
		t.Fatal("Muse takes its approval in the adapter environment")
	}
	environment := map[string]string{musePrefixes: `[["gh","pr"]]`}
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindMuse, core.ExecutionPolicyConfiguredApprovals, environment); err != nil {
		t.Fatal(err)
	}
	var prefixes [][]string
	if err := json.Unmarshal([]byte(environment[musePrefixes]), &prefixes); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(prefixes, [][]string{{"gh", "pr"}, {"gh"}}) {
		t.Fatalf("prefixes = %v", prefixes)
	}
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindMuse, core.ExecutionPolicyConfiguredApprovals, environment); err != nil {
		t.Fatal(err)
	}
	var again [][]string
	if err := json.Unmarshal([]byte(environment[musePrefixes]), &again); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(again, prefixes) {
		t.Fatalf("prefixes = %v", again)
	}
}

func TestProfileAndConfigSymlinksAreNeverModified(t *testing.T) {
	temp := t.TempDir()
	root := filepath.Join(temp, "worker")
	if err := os.MkdirAll(root, 0o777); err != nil {
		t.Fatal(err)
	}
	source := filepath.Join(temp, "source-profile")
	if err := os.Mkdir(source, 0o777); err != nil {
		t.Fatal(err)
	}
	sourceConfig := filepath.Join(source, "config.toml")
	original := "[permission]\ndeny = [\"Bash(rm *)\"]\n"
	ghRulesWrite(t, sourceConfig, original)
	if err := os.Symlink(source, filepath.Join(root, "profile")); err != nil {
		t.Fatal(err)
	}
	environment := map[string]string{}
	if err := ghRulesConfigure(root, ghRulesSession, filepath.Join(root, "profile"), core.HarnessKindKimi, core.ExecutionPolicyConfiguredApprovals, environment); err != nil {
		t.Fatal(err)
	}
	if got := ghRulesRead(t, sourceConfig); got != original {
		t.Fatalf("source = %q", got)
	}

	home := filepath.Join(root, "profiles", ghRulesSession)
	if err := os.MkdirAll(home, 0o777); err != nil {
		t.Fatal(err)
	}
	outside := filepath.Join(temp, "outside.toml")
	ghRulesWrite(t, outside, original)
	if err := os.Symlink(outside, filepath.Join(home, "config.toml")); err != nil {
		t.Fatal(err)
	}
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindKimi, core.ExecutionPolicyConfiguredApprovals, environment); err == nil {
		t.Fatal("a linked configuration must be refused")
	}
	if got := ghRulesRead(t, outside); got != original {
		t.Fatalf("outside = %q", got)
	}

	// Claude's approval never touches the settings file, so even a linked
	// one — including the person's own — is left exactly as it was.
	outsideSettings := filepath.Join(temp, "outside-settings.json")
	ghRulesWrite(t, outsideSettings, "{}")
	if err := os.Symlink(outsideSettings, filepath.Join(home, "settings.json")); err != nil {
		t.Fatal(err)
	}
	if err := ghRulesConfigure(root, ghRulesSession, home, core.HarnessKindClaude, core.ExecutionPolicyConfiguredApprovals, environment); err != nil {
		t.Fatal(err)
	}
	if got := ghRulesRead(t, outsideSettings); got != "{}" {
		t.Fatalf("settings = %q", got)
	}
}
