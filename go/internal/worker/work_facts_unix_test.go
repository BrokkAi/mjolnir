// Port of the Unix-only tests in mj-worker/src/work_facts.rs.

//go:build unix

package worker

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// rendezvousGit is a fake git for the tree read: status and log each
// announce that they started and then wait for the other to have started,
// so the reads can only finish if they run together.
func rendezvousGit(t *testing.T, directory, statusSha, logSha string) string {
	t.Helper()
	git := filepath.Join(directory, "rendezvous-git")
	body := fmt.Sprintf(`#!/bin/sh
dir="$2"; command="$3"
wait_for() {
  i=0
  while [ ! -f "$dir/$1" ]; do
    i=$((i+1)); [ "$i" -gt 1000 ] && exit 3
    sleep 0.01
  done
}
case "$command" in
  status) touch "$dir/status.started"; wait_for log.started
          printf '# branch.oid %s\000# branch.head main\000' ;;
  log) touch "$dir/log.started"; wait_for status.started
       printf '%s\000The subject\n' ;;
  *) exit 2 ;;
esac
`, statusSha, logSha)
	if err := os.WriteFile(git, []byte(body), 0o755); err != nil {
		t.Fatal(err)
	}
	return git
}

func TestTheStatusAndTheSubjectReadsRunTogetherAndAgreeOnTheCommit(t *testing.T) {
	directory := t.TempDir()
	sha := strings.Repeat("1", 40)
	git := rendezvousGit(t, directory, sha, sha)
	observed := readWorktree(context.Background(), &ObserverChildren{}, git, directory)
	if observed.Value == nil {
		t.Fatalf("the status and the log did not run together: %s", observed.Reason)
	}
	subject := observed.Value.Head.Subject.Known()
	if subject == nil || *subject != "The subject" {
		t.Fatalf("%+v", observed.Value.Head.Subject)
	}
}

// A commit made between the two reads would give the id of one commit the
// subject of another: the subject is then unavailable, with that reason,
// and the commit id from status stays.
func TestASubjectReadForADifferentCommitIsUnavailable(t *testing.T) {
	directory := t.TempDir()
	git := rendezvousGit(t, directory, strings.Repeat("1", 40), strings.Repeat("2", 40))
	observed := readWorktree(context.Background(), &ObserverChildren{}, git, directory)
	if observed.Value == nil {
		t.Fatalf("the tree was unavailable: %s", observed.Reason)
	}
	head := observed.Value.Head
	if head.Sha != strings.Repeat("1", 40) {
		t.Fatal(head.Sha)
	}
	if head.Subject.Known() != nil || !strings.Contains(head.Subject.Reason, "HEAD moved") {
		t.Fatalf("%+v", head.Subject)
	}
}
