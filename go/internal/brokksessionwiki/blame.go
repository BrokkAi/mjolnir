// Port of brokk-sessionwiki 0.34.28 src/blame.rs.

package brokksessionwiki

// LineBlame is one line of `git blame --line-porcelain` output.
type LineBlame struct {
	Line       int
	Commit     string
	AuthorTime int64
}

// ParseLinePorcelain parses `git blame --line-porcelain` output.
func ParseLinePorcelain(out string) []LineBlame {
	panic("unported: brokk-sessionwiki src/blame.rs parse_line_porcelain")
}

// Run is a contiguous range of lines sharing one commit.
type Run struct {
	Start      int
	End        int
	Commit     string
	AuthorTime int64
}

// GroupRuns collapses per-line blame into contiguous runs sharing one commit.
func GroupRuns(lines []LineBlame) []Run {
	var runs []Run
	for _, line := range lines {
		if len(runs) > 0 {
			last := &runs[len(runs)-1]
			if last.Commit == line.Commit && line.Line == last.End+1 {
				last.End = line.Line
				continue
			}
		}
		runs = append(runs, Run{Start: line.Line, End: line.Line, Commit: line.Commit, AuthorTime: line.AuthorTime})
	}
	return runs
}

// TouchingSession is a session that touched a file, with the window blame
// attribution compares commit times against.
type TouchingSession struct {
	SessionID string
	Tool      string
	Title     string
	Project   string
	Started   *int64
	Ended     *int64
	Archived  bool
}

// Attribution is which session a commit is attributed to.
type Attribution interface {
	isAttribution()
}

type AttributionConfident struct {
	Session TouchingSession
}

type AttributionAmbiguous struct {
	Sessions []TouchingSession
}

type AttributionUnattributed struct{}

func (AttributionConfident) isAttribution()    {}
func (AttributionAmbiguous) isAttribution()    {}
func (AttributionUnattributed) isAttribution() {}

// AttributeCommit attributes a commit to the sessions that may have made
// it.
func AttributeCommit(authorTime int64, repoPath string, candidates []TouchingSession) Attribution {
	panic("unported: brokk-sessionwiki src/blame.rs attribute_commit")
}
