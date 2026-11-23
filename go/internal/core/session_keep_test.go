// Port of the tests in mj-core/src/session_keep.rs (Unix only, as in Rust).

//go:build unix

package core

import (
	"io/fs"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func sessionKeepGitOK(t *testing.T, cwd string, args ...string) string {
	t.Helper()
	output, err := sessionKeepGit(cwd, args, "test")
	if err != nil {
		t.Fatal(err)
	}
	if !output.Status.Success() {
		t.Fatalf("git %q: %s", args, output.Stderr)
	}
	return string(output.Stdout)
}

func sessionKeepInitRepo(t *testing.T, path string) {
	t.Helper()
	if err := os.MkdirAll(path, 0o777); err != nil {
		t.Fatal(err)
	}
	sessionKeepGitOK(t, path, "init", "-q")
}

func sessionKeepInstall(t *testing.T, cwd, sessionID string) (string, map[string]string) {
	t.Helper()
	environment := map[string]string{}
	path, err := InstallSessionKeepDir(cwd, sessionID, environment)
	if err != nil {
		t.Fatal(err)
	}
	return path, environment
}

func sessionKeepIsSymlink(t *testing.T, path string) bool {
	t.Helper()
	info, err := os.Lstat(path)
	if err != nil {
		t.Fatal(err)
	}
	return info.Mode()&fs.ModeSymlink != 0
}

func sessionKeepIsDir(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.IsDir()
}

func sessionKeepExists(path string) bool {
	_, err := os.Lstat(path)
	return err == nil
}

// A symlink at `.mj`, `.mj/keep`, or the session directory must not receive
// the scratch directory, including when it points at another session. A
// clean tree creates the directory, lists `/.mj/keep/` once, and exports
// that path.
func TestSessionKeepDirRefusesSymlinkEscapeAndExportsItsPath(t *testing.T) {
	temp := t.TempDir()
	session := "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	other := "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
	repo := filepath.Join(temp, "repo")
	sessionKeepInitRepo(t, repo)
	if err := os.WriteFile(filepath.Join(repo, ".git/info/exclude"), []byte("*.tmp\n/.mj/agents/"), 0o666); err != nil {
		t.Fatal(err)
	}

	path, environment := sessionKeepInstall(t, repo, session)
	againPath, againEnvironment := sessionKeepInstall(t, repo, session)
	if againPath != path {
		t.Fatalf("second install %s, first %s", againPath, path)
	}
	if environment[SessionKeepDirEnv] != path {
		t.Fatalf("environment %v", environment)
	}
	if againEnvironment[SessionKeepDirEnv] != environment[SessionKeepDirEnv] {
		t.Fatalf("second environment %v", againEnvironment)
	}
	if !sessionKeepIsDir(path) || sessionKeepIsSymlink(t, path) {
		t.Fatalf("%s is not a plain directory", path)
	}
	canonicalRepo, err := canonicalizePath(repo)
	if err != nil {
		t.Fatal(err)
	}
	if relative, ok := pathStripPrefix(path, canonicalRepo); !ok || relative != filepath.Join(".mj", "keep", session) {
		t.Fatalf("relative path %q %v", relative, ok)
	}
	if err := os.WriteFile(filepath.Join(path, "receipt.json"), []byte("{}\n"), 0o666); err != nil {
		t.Fatal(err)
	}
	exclude, err := os.ReadFile(filepath.Join(repo, ".git/info/exclude"))
	if err != nil {
		t.Fatal(err)
	}
	if string(exclude) != "*.tmp\n/.mj/agents/\n/.mj/keep/\n" {
		t.Fatalf("exclude %q", exclude)
	}
	if count := strings.Count(string(exclude), "/.mj/keep/\n"); count != 1 {
		t.Fatalf("exclude lists the keep directory %d times", count)
	}
	if status := sessionKeepGitOK(t, repo, "status", "--porcelain", "--ignored=no"); status != "" {
		t.Fatalf("status %q", status)
	}

	nested := filepath.Join(repo, "nested")
	if err := os.Mkdir(nested, 0o777); err != nil {
		t.Fatal(err)
	}
	if nestedPath, _ := sessionKeepInstall(t, nested, session); nestedPath != path {
		t.Fatalf("nested install %s", nestedPath)
	}
	if sessionKeepExists(filepath.Join(repo, "nested/.mj")) {
		t.Fatal("nested install created its own .mj")
	}

	outside := filepath.Join(temp, "outside")
	if err := os.Mkdir(outside, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(outside, "secret"), []byte("keep"), 0o666); err != nil {
		t.Fatal(err)
	}

	linkedMj := filepath.Join(temp, "linked-mj")
	sessionKeepInitRepo(t, linkedMj)
	if err := os.Symlink(outside, filepath.Join(linkedMj, ".mj")); err != nil {
		t.Fatal(err)
	}
	environment = map[string]string{}
	_, err = InstallSessionKeepDir(linkedMj, session, environment)
	if err == nil || !strings.Contains(err.Error(), "symlinked") {
		t.Fatalf("linked .mj: %v", err)
	}
	if len(environment) != 0 {
		t.Fatalf("environment %v", environment)
	}
	if sessionKeepExists(filepath.Join(outside, "keep")) {
		t.Fatal("keep directory created through a linked .mj")
	}
	if info, err := os.Stat(filepath.Join(outside, "secret")); err != nil || !info.Mode().IsRegular() {
		t.Fatal("secret disturbed")
	}

	linkedKeep := filepath.Join(temp, "linked-keep")
	sessionKeepInitRepo(t, linkedKeep)
	if err := os.Mkdir(filepath.Join(linkedKeep, ".mj"), 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(linkedKeep, ".mj/keep")); err != nil {
		t.Fatal(err)
	}
	if _, err := InstallSessionKeepDir(linkedKeep, session, map[string]string{}); err == nil {
		t.Fatal("installed through a linked .mj/keep")
	}
	if sessionKeepExists(filepath.Join(outside, session)) {
		t.Fatal("session directory created through a linked .mj/keep")
	}
	if secret, err := os.ReadFile(filepath.Join(outside, "secret")); err != nil || string(secret) != "keep" {
		t.Fatalf("secret %q %v", secret, err)
	}

	linkedSession := filepath.Join(temp, "linked-session")
	sessionKeepInitRepo(t, linkedSession)
	otherDir := filepath.Join(linkedSession, ".mj/keep", other)
	if err := os.MkdirAll(otherDir, 0o777); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(otherDir, "secret"), []byte("other session"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(otherDir, filepath.Join(linkedSession, ".mj/keep", session)); err != nil {
		t.Fatal(err)
	}
	if _, err := InstallSessionKeepDir(linkedSession, session, map[string]string{}); err == nil {
		t.Fatal("installed through a linked session directory")
	}
	if secret, err := os.ReadFile(filepath.Join(otherDir, "secret")); err != nil || string(secret) != "other session" {
		t.Fatalf("other session's secret %q %v", secret, err)
	}
	if !sessionKeepIsSymlink(t, filepath.Join(linkedSession, ".mj/keep", session)) {
		t.Fatal("the session link was replaced")
	}

	plain := filepath.Join(temp, "plain")
	if err := os.Mkdir(plain, 0o777); err != nil {
		t.Fatal(err)
	}
	plainPath, plainEnvironment := sessionKeepInstall(t, plain, session)
	if !sessionKeepIsDir(plainPath) {
		t.Fatalf("%s is not a directory", plainPath)
	}
	if sessionKeepExists(filepath.Join(plain, ".git")) {
		t.Fatal("a plain directory became a repository")
	}
	if plainEnvironment[SessionKeepDirEnv] != plainPath {
		t.Fatalf("plain environment %v", plainEnvironment)
	}

	real := filepath.Join(temp, "real")
	sessionKeepInitRepo(t, real)
	alias := filepath.Join(temp, "alias")
	if err := os.Symlink(real, alias); err != nil {
		t.Fatal(err)
	}
	aliased, _ := sessionKeepInstall(t, alias, session)
	canonicalReal, err := canonicalizePath(real)
	if err != nil {
		t.Fatal(err)
	}
	if aliased != filepath.Join(canonicalReal, ".mj", "keep", session) {
		t.Fatalf("aliased install %s", aliased)
	}
	if !sessionKeepIsDir(aliased) {
		t.Fatalf("%s is not a directory", aliased)
	}

	escape := filepath.Join(temp, "escape")
	sessionKeepInitRepo(t, escape)
	if _, err := InstallSessionKeepDir(escape, "../other", map[string]string{}); err == nil {
		t.Fatal("installed an escaping session id")
	}
	if sessionKeepExists(filepath.Join(temp, "other")) {
		t.Fatal("an escaping session id created a directory")
	}
	if sessionKeepExists(filepath.Join(escape, ".mj")) {
		t.Fatal("an escaping session id created .mj")
	}
}
