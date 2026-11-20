// Port of the tests in mj-core/src/remote_git.rs.

package core

import (
	"fmt"
	"slices"
	"testing"
)

func TestGithubShorthandExpandsToHTTPS(t *testing.T) {
	github := "BrokkAi/hel"
	repository := ProjectRepository{ID: "repo", Github: &github, Destination: "repo"}
	source, err := ResolveRepository(&repository, ProcessExecutor{})
	if err != nil {
		t.Fatal(err)
	}
	if source.FetchURL != "https://github.com/BrokkAi/hel.git" {
		t.Fatalf("fetch URL = %q", source.FetchURL)
	}
	if !slices.Equal(source.PushURLs, []string{"https://github.com/BrokkAi/hel.git"}) {
		t.Fatalf("push URLs = %q", source.PushURLs)
	}
}

// Hard-won: a2e8bd8e9: a GitHub token was withheld from a URL whose path the parser rejects.
func TestGithubHTTPSHostsAreRecognizedWhateverThePath(t *testing.T) {
	for _, url := range []string{
		"https://github.com/o/r",
		"https://github.com/acme/%72epo.git",
		"https://GITHUB.com/o/r",
		"https://github.com:443/o/r",
	} {
		if !IsGithubHTTPSURL(url) {
			t.Errorf("rejected %s", url)
		}
	}
	// The token goes only to GitHub's web host, over https, on its port.
	for _, url := range []string{
		"http://github.com/o/r",
		"https://github.com:444/o/r",
		"https://gist.github.com/o/r",
		"https://github.example.com/o/r",
		"https://notgithub.com/o/r",
		"https://github.com.example.com/o/r",
		"git@github.com:o/r.git",
		"ssh://git@github.com/o/r.git",
		"o/r",
	} {
		if IsGithubHTTPSURL(url) {
			t.Errorf("accepted %s", url)
		}
	}
}

func TestGithubRemoteParserSeparatesRealRemotesFromRelativePathsAndOtherHosts(t *testing.T) {
	// (input, result with shorthand allowed, whether only shorthand makes it
	// GitHub). Results read `owner/repo spelling host port`.
	table := []struct {
		input         string
		allowed       string
		shorthandOnly bool
	}{
		{"owner/repo", "owner/repo Shorthand GithubCom None", true},
		{"owner/repo.git", "owner/repo Shorthand GithubCom None", true},
		{"mirror/repo.git", "mirror/repo Shorthand GithubCom None", true},
		{"https://github.com/o/r", "o/r Https GithubCom None", false},
		{"https://github.com/o/r.git", "o/r Https GithubCom None", false},
		{"https://github.com/o/r/", "o/r Https GithubCom None", false},
		{"https://github.com/o/r/extra", "", false},
		{"http://github.com/o/r", "o/r Http GithubCom None", false},
		{"https://user:pass@github.com/o/r.git", "o/r Https GithubCom None", false},
		{"https://github.com:443/o/r", "o/r Https GithubCom None", false},
		{"https://github.com:444/o/r", "", false},
		{"http://github.com:443/o/r", "", false},
		{"git@github.com:o/r.git", "o/r Scp GithubCom None", false},
		{"github.com:o/r.git", "o/r Scp GithubCom None", false},
		{"ssh://git@github.com/o/r.git", "o/r Ssh GithubCom None", false},
		{"ssh://github.com/o/r.git", "o/r Ssh GithubCom None", false},
		{"ssh://git@github.com:22/o/r.git", "o/r Ssh GithubCom Some(22)", false},
		{"ssh://git@github.com:443/o/r.git", "", false},
		{"ssh://git@ssh.github.com:443/o/r.git", "o/r Ssh SshGithubCom Some(443)", false},
		{"ssh://ssh.github.com:443/o/r.git", "o/r Ssh SshGithubCom Some(443)", false},
		{"ssh://git@ssh.github.com/o/r", "", false},
		{"git@GitHub.com:O/R.git", "O/R Scp GithubCom None", false},
		{"https://GITHUB.com/O/R", "O/R Https GithubCom None", false},
		{"  git@github.com:o/r.git\n", "o/r Scp GithubCom None", false},
		{"./o/r", "", false},
		{"/o/r", "", false},
		{"https://github.com/o/r?x=1", "", false},
		{"https://github.com/o/r#readme", "", false},
		{"https://gist.github.com/o/r", "", false},
		{"https://github.example.com/o/r", "", false},
		{"https://notgithub.com/o/r", "", false},
		{"git@gitlab.com:o/r", "", false},
		{"git@ssh.github.com:o/r", "o/r Scp SshGithubCom None", false},
		{"alice@github.com:o/r", "", false},
		{"ssh://alice@github.com/o/r", "", false},
		{"git://github.com/o/r", "o/r Git GithubCom None", false},
		{"git://github.com:9418/o/r", "", false},
		{"git://gitlab.com/o/r", "", false},
		{"acme/widget/child", "", false},
		{"-o/r", "", false},
		{"o/r$", "", false},
	}
	describe := func(remote *GithubRemote) string {
		if remote == nil {
			return ""
		}
		port := "None"
		if remote.Port != nil {
			port = fmt.Sprintf("Some(%d)", *remote.Port)
		}
		return fmt.Sprintf("%s/%s %v %v %s", remote.Owner, remote.Repository, remote.Spelling, remote.Host, port)
	}
	for _, row := range table {
		if got := describe(ParseGithubRemote(row.input, ShorthandAllowed)); got != row.allowed {
			t.Errorf("%q with shorthand allowed = %q, want %q", row.input, got, row.allowed)
		}
		want := row.allowed
		if row.shorthandOnly {
			want = ""
		}
		if got := describe(ParseGithubRemote(row.input, ShorthandRejected)); got != want {
			t.Errorf("%q with shorthand rejected = %q, want %q", row.input, got, want)
		}
	}
}

func TestDefaultBranchRequiresSymbolicHeadAndCommit(t *testing.T) {
	output := CommandOutput{
		Status: 0,
		Stdout: []byte("ref: refs/heads/main\tHEAD\n0123456789012345678901234567890123456789\tHEAD\n"),
	}
	branch, err := parseDefaultBranch(&output)
	if err != nil || branch != "main" {
		t.Fatalf("branch = %q, %v", branch, err)
	}
	missingCommit := CommandOutput{Status: 0, Stdout: []byte("ref: refs/heads/main\tHEAD\n")}
	if _, err := parseDefaultBranch(&missingCommit); err == nil {
		t.Fatal("accepted a HEAD without a commit")
	}
}

func TestNetworkURLValidationRejectsLocalAndHelperEndpoints(t *testing.T) {
	for _, url := range []string{
		".",
		"./repo",
		"/tmp/repo",
		"file:/tmp/repo",
		"file:///tmp/repo",
		"ssh:/example/repo.git",
		"ext::foo",
		"helper::foo",
	} {
		if ValidateNetworkURL(url) == nil {
			t.Errorf("accepted %s", url)
		}
	}
	for _, url := range []string{
		"https://github.com/BrokkAi/hel.git",
		"ssh://git@example.com/repo.git",
		"git://example.com/repo.git",
		"git@example.com:org/repo.git",
	} {
		if err := ValidateNetworkURL(url); err != nil {
			t.Errorf("rejected %s: %v", url, err)
		}
	}
}

func TestDisplayURLRemovesUserinfo(t *testing.T) {
	if got := DisplayURL("https://user:secret@example.com/repo.git"); got != "https://example.com/repo.git" {
		t.Fatalf("display = %q", got)
	}
	if got := DisplayURL("git@github.com:org/repo.git"); got != "github.com:org/repo.git" {
		t.Fatalf("display = %q", got)
	}
}
