// Port of mj-cli/src/splash.rs.
//
// Plays the startup splash while the dashboard starts behind it.
//
// The splash runs in two stages over one splashPlayback. Until the store
// has loaded there is no dashboard yet, so playUntilLoaded owns the screen.
// From then on the dashboard's own loop keeps drawing the splash while its
// background work (session summaries, the startup conversation) is already
// running, and ends it once the playback has finished.

package cli

import (
	"time"

	tea "charm.land/bubbletea/v2"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// splashFrame is about thirty frames a second. The animation follows the
// clock, so a terminal that cannot keep up drops frames rather than slowing
// it down.
const splashFrame = 33 * time.Millisecond

// splashOutcomeKind is how the splash's wait ended.
type splashOutcomeKind uint8

const (
	splashReady splashOutcomeKind = iota
	splashFailed
	// splashCancelled: the user pressed Ctrl+C, or the process was asked to
	// stop. Raw mode turns Ctrl+C into a key, so this is how startup stays
	// interruptible.
	splashCancelled
)

// splashOutcome is how playUntilLoaded ended: Ready with the loaded value
// and the playback, still running; Failed with the loading error; or
// Cancelled.
type splashOutcome[T any] struct {
	kind     splashOutcomeKind
	value    T
	playback *splashPlayback
	err      error
}

// splashLoad is what the loading task produced (Rust's Result<T>).
type splashLoad[T any] struct {
	value T
	err   error
}

// splashWanted reports whether this launch shows the splash. It needs
// color, Unicode half blocks, and the dashboard's minimum width. The symbol
// set is the one the configuration names; with no setting, or a config that
// does not load (normal startup reports that), it is the terminal's own
// guess.
func splashWanted() bool {
	panic("unported: mj-cli/src/splash.rs wanted")
}

func symbolsForSplash(configPath string) core.SymbolSet {
	panic("unported: mj-cli/src/splash.rs symbols_for_splash")
}

// splashPlayback is one showing of the splash: its clock, and what it waits
// for.
type splashPlayback struct {
	started    time.Time
	timeline   tui.SplashTimeline
	status     *string
	background chat.Color
}

func startSplashPlayback() *splashPlayback {
	return &splashPlayback{started: time.Now(), background: chat.ColorReset}
}

// markReady: the dashboard can draw; the splash dissolves into background
// once it has played out.
func (p *splashPlayback) markReady(background chat.Color) {
	p.background = background
	p.timeline.MarkReady(time.Since(p.started))
}

func (p *splashPlayback) finished() bool {
	return p.timeline.Finished(time.Since(p.started))
}

func (p *splashPlayback) render(frame *chat.Frame) {
	tui.RenderSplash(frame.Area(), frame.Buffer(), &tui.SplashFrame{
		Elapsed:    time.Since(p.started),
		Timeline:   p.timeline,
		Status:     p.status,
		Background: p.background,
	})
}

// isInterrupt reports whether event is a Ctrl+C press. A repeat is not a
// press, as crossterm reports it.
func isInterrupt(event tea.Msg) bool {
	key, ok := event.(tea.KeyPressMsg)
	return ok && !key.IsRepeat && key.Mod.Contains(tea.ModCtrl) && key.Code == 'c'
}

// playUntilLoaded animates until loading finishes, then hands back its
// value and the playback, still running, for the dashboard to finish.
// background names the color the splash dissolves into, from the loaded
// value.
func playUntilLoaded[T any](screen *dashboardScreen, loading *core.JoinHandle[splashLoad[T]], background func(*T) chat.Color) (splashOutcome[T], error) {
	panic("unported: mj-cli/src/splash.rs play_until_loaded")
}
