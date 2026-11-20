// Port of the tests in mj-core/src/repository.rs.

package core

import (
	"errors"
	"io"
	"io/fs"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"
)

// fakeGitExecutor is a hand-written CommandExecutor whose Execute is a
// function, standing in for the Rust tests' one-off executor types.
type fakeGitExecutor struct {
	CommandExecutorDefaults
	execute func(command *CommandSpec) (CommandOutput, error)
}

func (e fakeGitExecutor) Execute(command *CommandSpec) (CommandOutput, error) {
	return e.execute(command)
}

func (e fakeGitExecutor) ExecuteCleanup(command *CommandSpec) (CommandOutput, error) {
	return e.Execute(command)
}

func (e fakeGitExecutor) ExecuteCleanupWithTimeout(command *CommandSpec, _ time.Duration) (CommandOutput, error) {
	return e.Execute(command)
}

func (e fakeGitExecutor) ExecuteWithoutDeadline(command *CommandSpec) (CommandOutput, error) {
	return e.Execute(command)
}

func (e fakeGitExecutor) ExecuteWithStdinWithoutDeadline(command *CommandSpec, input io.Reader) (CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e fakeGitExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e fakeGitExecutor) PauseDeadline() *DeadlinePause { return DeadlinePauseNone() }

func failingGit(status int, diagnostic string) fakeGitExecutor {
	return fakeGitExecutor{execute: func(*CommandSpec) (CommandOutput, error) {
		return CommandOutput{Status: status, Stderr: []byte(diagnostic)}, nil
	}}
}

func TestALocalFolderThatIsNotARepositoryIsAPlainProject(t *testing.T) {
	executor := ProcessExecutor{}
	directory := t.TempDir()
	plain, err := ResolveLocalDirectory(directory, executor)
	if err != nil {
		t.Fatal(err)
	}
	canonical, err := canonicalizePath(directory)
	if err != nil {
		t.Fatal(err)
	}
	if !plain.PlainDirectory || plain.CheckoutRoot != canonical || plain.CheckoutRoot != plain.RepositoryRoot {
		t.Fatalf("plain = %#v", plain)
	}
	if plain.Identity != (RepositoryIdentityLocal{Root: plain.RepositoryRoot}) {
		t.Fatalf("identity = %#v", plain.Identity)
	}

	// Git failing for any reason other than the expected absence keeps the
	// error: a repository that cannot be read is not a plain folder.
	refusedGit := failingGit(128, "fatal: detected dubious ownership in repository")
	if _, err := ResolveLocalDirectory(directory, refusedGit); err == nil || !strings.Contains(err.Error(), "dubious ownership") {
		t.Fatalf("refused error = %v", err)
	}

	// An initialized repository without a first commit is still usable in
	// place, but it cannot provide the committed snapshot a Git session
	// needs. Record that decision now rather than changing it if the agent
	// commits later.
	unborn := t.TempDir()
	runTestGit(t, unborn, "init", "-q")
	if resolved, err := ResolveLocalDirectory(unborn, executor); err != nil || !resolved.PlainDirectory {
		t.Fatalf("unborn = %#v, %v", resolved, err)
	}

	// The HEAD check must happen before identity resolution. Multiple
	// remotes are ambiguous for a committed repository, but they cannot
	// prevent an unborn repository from opening as a plain directory.
	unbornRemotes := t.TempDir()
	runTestGit(t, unbornRemotes, "init", "-q")
	runTestGit(t, unbornRemotes, "remote", "add", "one", "https://example.test/one.git")
	runTestGit(t, unbornRemotes, "remote", "add", "two", "https://example.test/two.git")
	if resolved, err := ResolveLocalDirectory(unbornRemotes, executor); err != nil || !resolved.PlainDirectory {
		t.Fatalf("unborn with remotes = %#v, %v", resolved, err)
	}
	runTestGit(t, unbornRemotes, "config", "user.name", "Repository Test")
	runTestGit(t, unbornRemotes, "config", "user.email", "repository@example.invalid")
	runTestGit(t, unbornRemotes, "commit", "--allow-empty", "-qm", "initial")
	if _, err := ResolveLocalDirectory(unbornRemotes, executor); err == nil || !strings.Contains(err.Error(), "multiple Git remotes") {
		t.Fatalf("ambiguous error = %v", err)
	}

	// A plain local session must not require the Git executable itself.
	missingGit := fakeGitExecutor{execute: func(*CommandSpec) (CommandOutput, error) {
		return CommandOutput{}, &fs.PathError{Op: "exec", Path: "git", Err: fs.ErrNotExist}
	}}
	if resolved, err := ResolveLocalDirectory(directory, missingGit); err != nil || !resolved.PlainDirectory {
		t.Fatalf("missing git = %#v, %v", resolved, err)
	}

	// A directory that is not there is still an error, not a project.
	_, err = ResolveLocalDirectory(filepath.Join(directory, "missing"), executor)
	var unavailable RepositoryUnavailableDirectory
	if !errors.As(err, &unavailable) {
		t.Fatalf("missing directory error = %v", err)
	}

	// A remote project is addressed by the repository it shares with its
	// sessions, so a plain folder is refused there.
	plainRemote := failingGit(128, "fatal: not a git repository (or any of the parent directories): .git")
	host := "example.test"
	_, err = ResolveDirectory("/projects/plain", &host, plainRemote)
	var notRepository RepositoryUnavailableNotRepository
	if !errors.As(err, &notRepository) {
		t.Fatalf("remote plain folder error = %v", err)
	}
}

func TestUnavailableLocalHistoryIsDistinctFromGitFailures(t *testing.T) {
	refusedGit := fakeGitExecutor{execute: func(command *CommandSpec) (CommandOutput, error) {
		if command.Env["LC_ALL"] != "C" {
			t.Errorf("LC_ALL = %q", command.Env["LC_ALL"])
		}
		return CommandOutput{Status: 128, Stderr: []byte("fatal: detected dubious ownership in repository")}, nil
	}}
	directory := t.TempDir()
	_, err := ResolveDirectory(filepath.Join(directory, "missing"), nil, ProcessExecutor{})
	var unavailableDirectory RepositoryUnavailableDirectory
	if !errors.As(err, &unavailableDirectory) {
		t.Fatalf("missing error = %v", err)
	}
	_, err = ResolveDirectory(directory, nil, ProcessExecutor{})
	var notRepository RepositoryUnavailableNotRepository
	if !errors.As(err, &notRepository) {
		t.Fatalf("not repository error = %v", err)
	}
	_, err = ResolveDirectory(directory, nil, refusedGit)
	var unavailable RepositoryUnavailable
	if err == nil || errors.As(err, &unavailable) || !strings.Contains(err.Error(), "dubious ownership") {
		t.Fatalf("refused error = %v", err)
	}
}

func TestUnavailableRemoteHistoryPreservesTransportAndAccessFailures(t *testing.T) {
	for _, row := range []struct {
		status      int
		diagnostic  string
		unavailable bool
	}{
		{128, "fatal: cannot change to '/projects/deleted': No such file or directory", true},
		{128, "fatal: cannot change to '/projects/file/child': Not a directory", true},
		{128, "fatal: not a git repository (or any of the parent directories): .git", true},
		{128, "fatal: cannot change to '/projects/private': Permission denied", false},
		{128, "fatal: detected dubious ownership in repository", false},
		{255, "ssh: connect to host remote port 22: No route to host", false},
		{255, "fatal: cannot change to '/projects/deleted': No such file or directory", false},
	} {
		failed := fakeGitExecutor{execute: func(command *CommandSpec) (CommandOutput, error) {
			if command.Env["LC_ALL"] != "C" {
				t.Errorf("LC_ALL = %q", command.Env["LC_ALL"])
			}
			return CommandOutput{Status: row.status, Stderr: []byte(row.diagnostic)}, nil
		}}
		host := "remote"
		_, err := ResolveDirectory("/projects/checkout", &host, failed)
		if err == nil {
			t.Fatalf("%d: %s resolved", row.status, row.diagnostic)
		}
		var unavailable RepositoryUnavailable
		if errors.As(err, &unavailable) != row.unavailable {
			t.Fatalf("%d: %s: unavailable = %v", row.status, row.diagnostic, !row.unavailable)
		}
	}
}

// gitRootsExecutor answers `rev-parse` with fixed roots and every other Git
// command with empty success.
func gitRootsExecutor(roots string) fakeGitExecutor {
	return fakeGitExecutor{execute: func(command *CommandSpec) (CommandOutput, error) {
		stdout := ""
		if slices.Contains(command.Args, "rev-parse") {
			stdout = roots
		}
		return CommandOutput{Status: 0, Stdout: []byte(stdout)}, nil
	}}
}

func TestRemoteDirectoryResolutionSupportsSubdirectoriesAndLinkedCheckouts(t *testing.T) {
	for _, row := range [][4]string{
		{"/projects/app/subdir", "/projects/app\n/projects/app/.git\n", "/projects/app", "/projects/app"},
		{"/worktrees/app-side", "/worktrees/app-side\n/projects/app/.git\n", "/worktrees/app-side", "/projects/app"},
	} {
		host := "remote-host"
		resolved, err := ResolveDirectory(row[0], &host, gitRootsExecutor(row[1]))
		if err != nil {
			t.Fatal(err)
		}
		if comparePaths(resolved.CheckoutRoot, row[2]) != 0 || comparePaths(resolved.RepositoryRoot, row[3]) != 0 {
			t.Fatalf("%s resolved to %#v", row[0], resolved)
		}
		if resolved.Identity != (RepositoryIdentityRemoteDirectory{Host: "remote-host", Root: row[3]}) {
			t.Fatalf("identity = %#v", resolved.Identity)
		}
	}
}

func TestSuccessfulGitExitDoesNotAcceptInvalidProjectRoots(t *testing.T) {
	for _, roots := range []string{
		"--path-format=absolute\n/projects/app\n/projects/app/.git\n",
		"/projects/app\n--path-format=absolute\n",
		"/projects/app\n.git\nextra\n",
		"/projects/app\n",
		"\n.git\n",
	} {
		host := "host"
		if _, err := ResolveDirectory("/projects/app", &host, gitRootsExecutor(roots)); err == nil {
			t.Fatalf("accepted %q", roots)
		}
	}
}

func TestNetworkIdentityKeepsCaseSensitivePathsAndRemovesCredentials(t *testing.T) {
	first := RepositoryIdentityFromRemote("https://user:secret@example.com/Team/App.git")
	if first == nil || first.Key() != "git:https://example.com/Team/App" {
		t.Fatalf("identity = %#v", first)
	}
	if second := RepositoryIdentityFromRemote("https://example.com/team/app.git"); second == first {
		t.Fatalf("case-different paths share an identity: %#v", second)
	}
	if local := RepositoryIdentityFromRemote("../local"); local != nil {
		t.Fatalf("local path has an identity: %#v", local)
	}
}
