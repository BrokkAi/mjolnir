// Port of the tests in mj-worker/src/work_facts/macos.rs, which run on
// every platform as the parsing does.

package worker

import "testing"

func TestASameUserProcessWhoseEnvironmentPsCannotShowIsUnreadable(t *testing.T) {
	wanted := "MJ_SESSION_ID=abc"
	command := "/usr/bin/tool --flag"
	line := func(text string) *string { return &text }
	for _, check := range []struct {
		line     *string
		sameUser bool
		want     WorkFactsMarker
	}{
		{line("  41 /usr/bin/tool --flag HOME=/h MJ_SESSION_ID=abc"), true, WorkFactsMarkerPresent},
		{line("  41 /usr/bin/tool --flag HOME=/h"), true, WorkFactsMarkerAbsent},
		// No environment appended to a same-user process: it could not be
		// read.
		{line("  41 /usr/bin/tool --flag"), true, WorkFactsMarkerUnreadable},
		// Another user's process is not this session's.
		{line("  41 /usr/bin/tool --flag"), false, WorkFactsMarkerAbsent},
		{nil, true, WorkFactsMarkerAbsent},
	} {
		if got := classifyMarker(check.line, command, check.sameUser, wanted); got != check.want {
			t.Fatalf("%v %v: got %d, want %d", check.line, check.sameUser, got, check.want)
		}
	}
}
