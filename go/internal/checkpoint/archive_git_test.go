// Port of the tests in mj-checkpoint/src/archive/git.rs (review_record_tests).

package checkpoint

import (
	"strings"
	"testing"
)

func TestReviewedTreeRefPointsToTheReviewedCommit(t *testing.T) {
	root := t.TempDir()
	if _, err := gitBytes(SystemGit{}, root, []string{"init", "-q"}, nil, "initialize repository"); err != nil {
		t.Fatal(err)
	}
	tree, err := EmptyTreeID(SystemGit{}, root)
	if err != nil {
		t.Fatal(err)
	}
	var env [][2]string
	for _, key := range []string{"GIT_AUTHOR_NAME", "GIT_AUTHOR_EMAIL", "GIT_COMMITTER_NAME", "GIT_COMMITTER_EMAIL"} {
		env = append(env, [2]string{key, "review@example.test"})
	}
	output, err := SystemGit{}.Run(root, &GitCommand{
		Arguments: []string{"commit-tree", tree, "-m", "reviewed"},
		Env:       env,
	})
	if err != nil {
		t.Fatal(err)
	}
	if output.Status != 0 {
		t.Fatalf("commit-tree status %d: %s", output.Status, output.Stderr)
	}
	commit := strings.TrimSpace(string(output.Stdout))
	if err := RecordReviewedTree(SystemGit{}, root, tree, commit); err != nil {
		t.Fatal(err)
	}
	recorded, err := gitText(SystemGit{}, root, []string{"rev-parse", "--verify", "refs/mj/reviewed/" + tree})
	if err != nil {
		t.Fatal(err)
	}
	if recorded != commit {
		t.Fatalf("recorded %q, want %q", recorded, commit)
	}
}
