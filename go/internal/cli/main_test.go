// Tests of main.go, ported from the tests module of mj-cli/src/main.rs.

package cli

import (
	"bytes"
	"strings"
	"testing"

	"github.com/spf13/cobra"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// rootHelp is `mj --help` as the command tree renders it.
func rootHelp(t *testing.T) string {
	t.Helper()
	unsetHelpEnvironment(t)
	var stdout, stderr bytes.Buffer
	if code := Main([]string{"--help"}, &stdout, &stderr); code != 0 {
		t.Fatalf("exit %d: %s", code, stderr.String())
	}
	return stdout.String()
}

// Hard-won: #1659: Move dry-run discarded the inventory's outside-worktree blocker.
func TestGoldenMoveDryRunReportsSnapshotBlockers(t *testing.T) {
	preflight := core.SuspendPreflight{
		Blockers: []core.SuspendBlocker{{
			Kind:   "unrepresented_git_state",
			Paths:  []string{"/workspace/outside"},
			Remedy: "unrepresented worktree /workspace/outside; push or copy out anything wanted, then run `git --git-dir /workspace/clone --work-tree /workspace/clone worktree remove --force /workspace/outside` or move it inside the clone before suspending",
		}},
		KeepDirEntries: []core.KeepDirEntry{{
			Checkout: core.DefaultCheckoutKey(),
			Path:     "legacy.txt",
			Bytes:    8,
			Identity: core.NewFileIdentity("abc"),
		}},
	}
	route := core.RoutePreviewDirectOrDaemonStream
	output := formatMoveDryRun(&route, nil, &preflight)
	core.AssertGolden(t, "testdata", "move-dry-run", output)
}

// Hard-won: 0475c51230: Launch finding C-7 found the refusal say “1 queued command require”; this checks singular and plural counts agree.
func TestQueuedCommandRefusalAgreesInNumber(t *testing.T) {
	if got, want := queuedCommandsNeedChoice(1), "1 queued command requires an explicit --queue discard|start with --yes"; got != want {
		t.Errorf("queuedCommandsNeedChoice(1) = %q, want %q", got, want)
	}
	if got, want := queuedCommandsNeedChoice(2), "2 queued commands require an explicit --queue discard|start with --yes"; got != want {
		t.Errorf("queuedCommandsNeedChoice(2) = %q, want %q", got, want)
	}
}

// F-16: `--session`, `--json`, and other flags had no description in
// `--help`. Every visible argument of every command now says what it is.
// Hard-won: 820ba5c7eb: Finding F-16 found dozens of visible flags without help text; the recursive command-tree check rejects every undescribed argument.
func TestEveryVisibleArgumentIsDescribedInHelp(t *testing.T) {
	var undescribed func(command *cobra.Command, path string, missing *[]string)
	undescribed = func(command *cobra.Command, path string, missing *[]string) {
		for _, arg := range clapOf(command).args {
			if arg.hidden {
				continue
			}
			if arg.help == "" {
				*missing = append(*missing, path+" --"+arg.id)
			}
		}
		for _, sub := range command.Commands() {
			if sub.Hidden {
				continue
			}
			undescribed(sub, path+" "+sub.Name(), missing)
		}
	}
	var missing []string
	undescribed(newCommandTree(func(cli) error { return nil }), "mj", &missing)
	if len(missing) != 0 {
		t.Errorf("undescribed arguments: %q", missing)
	}
}

// R2-5: `mj new --workspace nosuch` said only `unknown workspace
// "nosuch"`. It now lists what exists and how to make one, in the shape the
// missing-flag refusal uses.
// Hard-won: f0e899a4bf: Launch finding R2-5 found unknown workspace errors omitted available workspaces and creation guidance; the test checks the repaired refusal text.
func TestAnUnknownWorkspaceIsRefusedWithTheWorkspacesAndHowToMakeOne(t *testing.T) {
	listed := []workspaceSessions{{name: "beta", sessions: 0}, {name: "alpha", sessions: 1}}
	message := unknownWorkspaceMessage("nosuch", &listed)
	if want := "unknown workspace \"nosuch\". Workspaces:\n  beta  (0 sessions)\n  alpha  (1 session)\nCreate one with `mj workspaces create NAME`."; message != want {
		t.Errorf("message = %q, want %q", message, want)
	}

	none := []workspaceSessions{}
	message = unknownWorkspaceMessage("nosuch", &none)
	if want := "unknown workspace \"nosuch\"; this instance has none yet\nCreate one with `mj workspaces create NAME`."; message != want {
		t.Errorf("message = %q, want %q", message, want)
	}
}

// F-16: `--workspace` was global, so every command's help offered it.
// Hard-won: 820ba5c7eb: Finding F-16 found `--workspace` exposed globally on commands that ignore it; this checks acceptance only on commands that use it.
func TestWorkspaceIsOfferedOnlyWhereItSelectsSomething(t *testing.T) {
	for _, argv := range [][]string{
		{"new", "--workspace", "w", "--project-directory", "/p"},
		{"sessions", "--workspace", "w"},
		{"events", "--workspace", "w"},
		{"resume", "--wiki", "x", "--workspace", "w"},
		{"import", "codex", "--latest", "--workspace", "w"},
		{"acp", "--workspace", "w"},
		{"--workspace", "w", "new", "--project-directory", "/p"},
		{"--workspace", "w"},
	} {
		if _, err := parseCommandLine(argv); err != nil {
			t.Errorf("%q: %v", argv, err)
		}
	}
	for _, argv := range [][]string{
		{"prompt", "--session", "s", "hi", "--workspace", "w"},
		{"wait", "--session", "s", "--workspace", "w"},
		{"doctor", "--workspace", "w"},
	} {
		if _, err := parseCommandLine(argv); err == nil {
			t.Errorf("%q parsed", argv)
		}
	}
	parsed, err := parseCommandLine([]string{"sessions", "--workspace", "w"})
	if err != nil {
		t.Fatal(err)
	}
	sessions, ok := parsed.command.(commandSessions)
	if !ok {
		t.Fatalf("expected the sessions command, got %T", parsed.command)
	}
	if name := sessions.args.workspace.or(nil); name == nil || *name != "w" {
		t.Errorf("workspace = %v, want w", name)
	}
}

// R2-10: the help said commands other than `mj new` and `mj acp` "need it
// when the instance has more than one", but with two workspaces
// `mj sessions` without it lists every session.
// Hard-won: 12dc56b2e8: Launch finding R2-10 found help falsely saying `mj sessions` needs a workspace; the test checks required-command and filter descriptions.
func TestWorkspaceHelpSaysWhichCommandsRequireItAndWhichFilter(t *testing.T) {
	root := newCommandTree(func(cli) error { return nil })
	sessions, _, err := root.Find([]string{"sessions"})
	if err != nil || sessions == root {
		t.Fatalf("the sessions command: %v", err)
	}
	argument := clapOf(sessions).find("workspace_name")
	if argument == nil {
		t.Fatal("the --workspace option")
	}
	help := argument.help
	if help == "" {
		t.Fatal("the --workspace help")
	}
	if strings.Contains(help, "other commands need it") {
		t.Errorf("%s", help)
	}
	if !strings.Contains(help, "`mj new` and `mj acp` require it") {
		t.Errorf("%s", help)
	}
	if !strings.Contains(help, "`mj sessions` and `mj events` show only that workspace") {
		t.Errorf("%s", help)
	}
}

// F-16: `mj acp` is documented, so `mj --help` lists it.
// Hard-won: 820ba5c7eb: Finding F-16 found `mj acp` hidden from `mj --help`; this checks the public command list includes it.
func TestAcpIsListedInHelp(t *testing.T) {
	help := rootHelp(t)
	if !strings.Contains(help, "acp") {
		t.Errorf("%s", help)
	}
}

// F-10: the removed names got clap's generic "unrecognized subcommand".
// Hard-won: e5db6edb36: Finding F-10 found removed `close` and `cancel-turn` names yielded generic clap errors; the test checks replacement guidance for both spellings and keeps them hidden from help.
func TestARemovedCommandNamesItsReplacementWhateverItWasGiven(t *testing.T) {
	for _, entry := range []struct {
		argv        []string
		replacement string
	}{
		{[]string{"checkpoint", "--session", "s1"}, "`mj suspend` saves the session"},
		{[]string{"close", "--session", "s1"}, "`mj suspend` and `mj destroy`"},
		{[]string{"close"}, "`mj suspend` and `mj destroy`"},
		{[]string{"cancel-turn", "--session", "s1"}, "`mj interrupt-turn`"},
	} {
		parsed, err := parseCommandLine(entry.argv)
		if err != nil {
			t.Fatalf("%q: the old name still parses: %v", entry.argv, err)
		}
		notice, ok := replacementNotice(parsed.command)
		if !ok {
			t.Fatalf("%q: a notice", entry.argv)
		}
		if !strings.Contains(notice, entry.replacement) {
			t.Errorf("%q: %s", entry.argv, notice)
		}
	}
	help := rootHelp(t)
	startsWithCheckpoint := false
	for _, line := range strings.Split(help, "\n") {
		if strings.HasPrefix(strings.TrimLeft(line, " \t"), "checkpoint ") {
			startsWithCheckpoint = true
		}
	}
	if strings.Contains(help, "cancel-turn") || startsWithCheckpoint {
		t.Errorf("the old names stay out of --help:\n%s", help)
	}
}

// The human report already prints every fix, so its closing line must point
// at them rather than send the user to `--json` for the same text.
// Hard-won: 36382afd92: Launch finding J-5 found human doctor output printed fixes then pointed users to JSON; the test checks the closing guidance refers to the printed remediations.
func TestHumanDoctorFailurePointsAtThePrintedFixes(t *testing.T) {
	human := doctorFailure(false).Error()
	if strings.Contains(human, "--json") {
		t.Errorf("%s", human)
	}
	if !strings.Contains(human, "remediation") {
		t.Errorf("%s", human)
	}
	if !strings.Contains(doctorFailure(true).Error(), "remediation") {
		t.Errorf("%s", doctorFailure(true))
	}
}

func TestFailedArchiveRemovalRetainsSessionMetadataForRetry(t *testing.T) {
	t.Skip("needs mj-controller (not yet ported): Controller::destroy_session_controlled with ProcessExecutor")
}
