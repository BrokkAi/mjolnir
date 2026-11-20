// Port of mj-checkpoint/src/checkpoint/submodules/tests.rs.

package checkpoint

import (
	"fmt"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"testing"
)

// scriptedGitKey is the directory a command runs in and its first argument.
type scriptedGitKey struct {
	directory string
	command   string
}

// scriptedGit answers each Git command from a script keyed by the directory
// it runs in and its first argument, and records where each command ran.
type scriptedGit struct {
	answers map[scriptedGitKey]GitOutput
	mu      sync.Mutex
	ran     []scriptedGitKey
}

type scriptedGitAnswer struct {
	directory string
	command   string
	output    GitOutput
}

func newScriptedGit(answers ...scriptedGitAnswer) *scriptedGit {
	git := &scriptedGit{answers: map[scriptedGitKey]GitOutput{}}
	for _, answer := range answers {
		git.answers[scriptedGitKey{checkpointNormalize(answer.directory), answer.command}] = answer.output
	}
	return git
}

// directories are the distinct directories commands ran in, sorted as a
// Rust BTreeSet<PathBuf>.
func (g *scriptedGit) directories() []string {
	g.mu.Lock()
	defer g.mu.Unlock()
	var directories []string
	for _, ran := range g.ran {
		directories = append(directories, ran.directory)
	}
	slices.SortFunc(directories, checkpointPathCompare)
	return slices.Compact(directories)
}

func (g *scriptedGit) Run(repository string, command *GitCommand) (GitOutput, error) {
	key := scriptedGitKey{checkpointNormalize(repository), command.Arguments[0]}
	g.mu.Lock()
	g.ran = append(g.ran, key)
	g.mu.Unlock()
	output, ok := g.answers[key]
	if !ok {
		return GitOutput{}, fmt.Errorf("unexpected Git command in %s: %q", repository, command.Arguments)
	}
	return output, nil
}

func scriptedAnswer(stdout string) GitOutput {
	return GitOutput{Status: 0, Stdout: []byte(stdout), Stderr: []byte{}}
}

func gitlinkEntries(paths ...string) string {
	var entries strings.Builder
	for _, path := range paths {
		fmt.Fprintf(&entries, "160000 %s 0\t%s\x00", strings.Repeat("b", 40), path)
	}
	return entries.String()
}

func registrations(paths ...string) string {
	var entries strings.Builder
	for _, path := range paths {
		fmt.Fprintf(&entries, "submodule.%s.path\n%s\x00", path, path)
	}
	return entries.String()
}

func TestOnlyCheckedOutRegisteredSubmodulesAreInspectedAndUnregisteredGitlinksAreNamed(t *testing.T) {
	top := t.TempDir()
	project := filepath.Join(top, "project")
	clean := filepath.Join(top, "vendor/clean")
	inner := filepath.Join(clean, "inner")
	dirty := filepath.Join(top, "vendor/dirty")
	for _, checkout := range []string{clean, inner, dirty, filepath.Join(top, "projectsrc/tailscale")} {
		checkpointMkdirAll(t, filepath.Join(checkout, ".git"))
	}
	checkpointMkdirAll(t, filepath.Join(top, "vendor/absent"))
	checkpointWriteFile(t, filepath.Join(top, ".gitmodules"), []byte{})
	checkpointWriteFile(t, filepath.Join(clean, ".gitmodules"), []byte{})
	files := fmt.Sprintf("100644 %s 0\tproject/README.md\x00", strings.Repeat("a", 40))
	git := newScriptedGit(
		scriptedGitAnswer{project, "rev-parse", scriptedAnswer(top + "\n")},
		scriptedGitAnswer{top, "ls-files", scriptedAnswer(files + gitlinkEntries(
			"vendor/clean",
			"vendor/dirty",
			"vendor/absent",
			"projectsrc/tailscale",
		))},
		scriptedGitAnswer{top, "config", scriptedAnswer(registrations(
			"vendor/clean",
			"vendor/dirty",
			"vendor/absent",
		))},
		scriptedGitAnswer{clean, "status", scriptedAnswer("")},
		scriptedGitAnswer{clean, "ls-files", scriptedAnswer(gitlinkEntries("inner"))},
		scriptedGitAnswer{clean, "config", scriptedAnswer(registrations("inner"))},
		scriptedGitAnswer{inner, "status", scriptedAnswer("?? notes.txt\n")},
		scriptedGitAnswer{inner, "ls-files", scriptedAnswer("")},
		scriptedGitAnswer{dirty, "status", scriptedAnswer(" M lib.rs\n")},
		scriptedGitAnswer{dirty, "ls-files", scriptedAnswer("")},
	)

	inspection := checkpointMust(InspectSubmodules(git, project))(t)

	expected := SubmoduleInspection{
		Dirty:        []string{"vendor/clean/inner", "vendor/dirty"},
		Unregistered: []string{"projectsrc/tailscale"},
	}
	if !checkpointSamePaths(inspection.Dirty, expected.Dirty) || !checkpointSamePaths(inspection.Unregistered, expected.Unregistered) {
		t.Fatalf("inspection = %+v", inspection)
	}
	ran := []string{project, top, clean, inner, dirty}
	for i := range ran {
		ran[i] = checkpointNormalize(ran[i])
	}
	slices.SortFunc(ran, checkpointPathCompare)
	if directories := git.directories(); !reflect.DeepEqual(directories, ran) {
		t.Fatalf("Git never runs in an unregistered gitlink or a submodule that is not checked out: %v", directories)
	}
	err := rejectDirtySubmodules(git, project)
	if err == nil || err.Error() != "submodules vendor/clean/inner, vendor/dirty have uncommitted changes; commit or stash them, then try again" {
		t.Fatalf("error = %v", err)
	}
}

func TestAGitFailureWhileInspectingSubmodulesKeepsItsStderr(t *testing.T) {
	top := t.TempDir()
	git := newScriptedGit(
		scriptedGitAnswer{top, "rev-parse", scriptedAnswer(top + "\n")},
		scriptedGitAnswer{top, "ls-files", GitOutput{
			Status: 128,
			Stdout: []byte{},
			Stderr: []byte("fatal: index file corrupt\n"),
		}},
	)

	err := rejectDirtySubmodules(git, top)

	if err == nil || err.Error() != "failed to inspect submodules: fatal: index file corrupt" {
		t.Fatalf("error = %v", err)
	}
}

func initRepository(t testing.TB, path string) {
	t.Helper()
	checkpointMkdirAll(t, path)
	git(path, "init", "-q")
	git(path, "config", "user.email", "hel@example.test")
	git(path, "config", "user.name", "Hel Test")
}

// R18: a checkout that records a nested repository as a bare gitlink, with
// no `.gitmodules` entry, could not be checkpointed at all, because
// `git submodule foreach` fails on a gitlink it has no URL for. The
// session's project directory was a subdirectory of that checkout.
// Hard-won: 4d80f0eb: checkpointing a checkout with an unregistered gitlink failed with Git's missing-URL error.
func TestARealCheckoutRefusesADirtySubmoduleButNotAnUnregisteredGitlink(t *testing.T) {
	temp := t.TempDir()
	top := filepath.Join(temp, "checkout")
	project := filepath.Join(top, "project")
	initRepository(t, top)
	checkpointMkdirAll(t, project)
	checkpointWriteFile(t, filepath.Join(project, "README.md"), []byte("project"))
	git(top, "add", ".")
	git(top, "commit", "-q", "-m", "base")

	nested := filepath.Join(top, "projectsrc/tailscale")
	initRepository(t, nested)
	git(nested, "commit", "-q", "--allow-empty", "-m", "nested")
	nestedHead := gitLine(nested, "rev-parse", "HEAD")
	git(top, "update-index", "--add", "--cacheinfo", "160000,"+nestedHead+",projectsrc/tailscale")
	git(top, "commit", "-q", "-m", "record the nested repository")
	// Work inside an unregistered gitlink never blocks the checkpoint.
	checkpointWriteFile(t, filepath.Join(nested, "scratch.txt"), []byte("nested work"))

	library := filepath.Join(temp, "library")
	initRepository(t, library)
	checkpointWriteFile(t, filepath.Join(library, "lib.txt"), []byte("base"))
	git(library, "add", ".")
	git(library, "commit", "-q", "-m", "library")
	git(top, "-c", "protocol.file.allow=always", "submodule", "add", library, "vendor/library")
	git(top, "commit", "-q", "-m", "add the library")
	checkpointWriteFile(t, filepath.Join(top, "vendor/library/lib.txt"), []byte("changed"))

	inspection := checkpointMust(InspectSubmodules(SystemGit{}, project))(t)
	if !checkpointSamePaths(inspection.Dirty, []string{"vendor/library"}) ||
		!checkpointSamePaths(inspection.Unregistered, []string{"projectsrc/tailscale"}) {
		t.Fatalf("inspection = %+v", inspection)
	}
	err := rejectDirtySubmodules(SystemGit{}, project)
	if err == nil || err.Error() != "submodule vendor/library has uncommitted changes; commit or stash them, then try again" {
		t.Fatalf("error = %v", err)
	}

	git(filepath.Join(top, "vendor/library"), "checkout", "--", "lib.txt")
	if err := rejectDirtySubmodules(SystemGit{}, project); err != nil {
		t.Fatalf("an unregistered gitlink never blocks the checkpoint: %v", err)
	}
}
