// Port of mj-cli/src/hints.rs.
//
// One-time hints for this client: things worth saying once, such as the
// prefix key colliding with tmux, and never again once seen.
//
// They live in a small JSON file beside `config.toml` rather than in the
// daemon's database, because they belong to the person's terminal habits,
// not to any session, and a fresh clone of the configuration directory
// should reset them.

package cli

import (
	"os"
	"path/filepath"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// hint is a hint the dashboard shows once.
type hint int

const (
	// hintPrefixCollision: the configured prefix is `ctrl+b` inside tmux or
	// GNU screen.
	hintPrefixCollision hint = iota
	// hintPrefixKeys: the first dashboard of a fresh install, before any key
	// was pressed.
	hintPrefixKeys
)

// name is the hint's name in the seen-hints file.
func (h hint) name() string {
	switch h {
	case hintPrefixCollision:
		return "prefix_collision"
	case hintPrefixKeys:
		return "prefix_keys"
	}
	panic("unknown hint")
}

// seenHints is the hints already shown, read from and written to one file.
type seenHints struct {
	// seen is the names of the hints shown; the file lists them sorted.
	seen map[string]struct{}
	path string
}

// seenHintsLoad reads the hints already shown from `client-hints.json` in
// the configuration directory (SeenHints::load).
func seenHintsLoad() seenHints {
	return seenHintsLoadFrom(filepath.Join(core.ConfigDir(), "client-hints.json"))
}

// seenHintsLoadFrom reads the hints already shown from path. A missing or
// unreadable file, or entries that are not names, count as nothing seen.
func seenHintsLoadFrom(path string) seenHints {
	panic("unported: mj-cli/src/hints.rs SeenHints::load_from")
}

// pending is whether the hint still has to be shown.
func (s *seenHints) pending(shown hint) bool {
	_, seen := s.seen[shown.name()]
	return !seen
}

// markShown records a hint that has been on screen. Marking it seen and
// saving happen together, so a hint is never shown twice even if the
// dashboard exits without saving anything else. Only a hint the notice bar
// actually carried may be recorded: writing one into a bar that never draws
// it spends it unread.
func (s *seenHints) markShown(shown hint) {
	panic("unported: mj-cli/src/hints.rs SeenHints::mark_shown")
}

// save writes the seen hints, sorted, to the file.
func (s *seenHints) save() error {
	panic("unported: mj-cli/src/hints.rs SeenHints::save")
}

// multiplexerName is the terminal multiplexer the dashboard is running
// under, whose own prefix would swallow `ctrl+b` before it reaches the
// dashboard. The name goes into the collision notice, so there is one place
// that reads the environment.
func multiplexerName() (string, bool) {
	if os.Getenv("TMUX") != "" {
		return "tmux", true
	}
	if os.Getenv("STY") != "" {
		return "screen", true
	}
	return "", false
}

// queuedHint is one hint waiting for the notice bar, or holding it.
type queuedHint struct {
	hint hint
	text string
}

// shownHint is the hint on screen now: when it went up, and whether the file
// records it.
type shownHint struct {
	hint     hint
	text     string
	since    time.Time
	recorded bool
}

// pendingHints is the first-launch hints, shown one after another as the
// notice bar comes free.
//
// A hint is recorded as shown only once it has been in the bar, because a
// launch that restores a conversation gives the bar to its own reports first
// and a hint written over those would be spent unread. Each hint then holds
// the bar for chat.NoticeMinimumDisplay before the next one takes over, so
// two hints on one launch are both readable.
type pendingHints struct {
	seen seenHints
	// queued is the hints waiting, oldest first (Rust's queue).
	queued  []queuedHint
	showing *shownHint
}

// newPendingHints is a queue of hints over the ones already seen.
func newPendingHints(seen seenHints) pendingHints {
	return pendingHints{seen: seen}
}

// queue queues hint unless it has been shown on an earlier launch.
func (p *pendingHints) queue(waiting hint, text string) {
	if !p.seen.pending(waiting) {
		return
	}
	p.queued = append(p.queued, queuedHint{hint: waiting, text: text})
}

// pump gives the next hint the notice bar when ready says the surface has
// settled and nothing else is reporting, and records the hint already there
// now that a frame has carried it. It answers whether the bar changed, so
// the caller can draw the frame that shows it.
func (p *pendingHints) pump(notices *chat.Notices, ready bool, now time.Time) bool {
	panic("unported: mj-cli/src/hints.rs PendingHints::pump")
}
