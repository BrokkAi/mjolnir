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
	"context"
	"errors"
	"fmt"
	"os"
	"time"

	tea "charm.land/bubbletea/v2"
	"github.com/charmbracelet/x/term"

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
	if chat.NoColorRequested() || symbolsForSplash(core.ConfigPath()) != core.SymbolSetUnicode {
		return false
	}
	width, _, err := term.GetSize(os.Stdout.Fd())
	return err == nil && tui.Fits(width)
}

func symbolsForSplash(configPath string) core.SymbolSet {
	var configured *core.SymbolSet
	if config, err := core.ConfigLoadFrom(configPath); err == nil {
		configured = config.Advanced.Symbols
	}
	return chat.SymbolsFor(configured)
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
//
// A JoinHandle cannot be aborted, so on Cancelled, Failed or an error the
// caller cancels the context it started loading under, which is Rust's
// `loading.abort()`.
func playUntilLoaded[T any](screen *dashboardScreen, loading *core.JoinHandle[splashLoad[T]], background func(*T) chat.Color) (splashOutcome[T], error) {
	route, notices := openStartupNoticeRoute()
	defer route.Close()
	playback := startSplashPlayback()
	draw := func() error {
		if err := screen.terminal.draw(playback.render); err != nil {
			return fmt.Errorf("draw the startup splash: %w", err)
		}
		return nil
	}
	// A Tokio interval's first tick is immediate.
	if err := draw(); err != nil {
		return splashOutcome[T]{}, err
	}
	frames := time.NewTicker(splashFrame)
	defer frames.Stop()
	// The notice route is an unbounded channel read by context; forward it
	// into the select below, and stop the forwarder when this returns.
	noticeCtx, stopNotices := context.WithCancel(context.Background())
	defer stopNotices()
	noticeCh := make(chan string)
	go func() {
		for {
			notice, ok := notices.Recv(noticeCtx)
			if !ok {
				return
			}
			select {
			case noticeCh <- notice:
			case <-noticeCtx.Done():
				return
			}
		}
	}()
	for {
		select {
		case <-frames.C:
			if err := draw(); err != nil {
				return splashOutcome[T]{}, err
			}
		case <-loading.Done():
			loaded, err := loading.Join(context.Background())
			if err != nil {
				return splashOutcome[T]{kind: splashFailed, err: fmt.Errorf("dashboard startup task failed: %w", err)}, nil
			}
			if loaded.err != nil {
				return splashOutcome[T]{kind: splashFailed, err: loaded.err}, nil
			}
			playback.markReady(background(&loaded.value))
			return splashOutcome[T]{kind: splashReady, value: loaded.value, playback: playback}, nil
		case notice := <-noticeCh:
			playback.status = &notice
		case <-screen.termination.Done():
			return splashOutcome[T]{kind: splashCancelled}, nil
		case event, ok := <-screen.events:
			switch {
			case !ok:
				return splashOutcome[T]{}, errors.New("terminal input closed during the startup splash")
			case event.err != nil:
				return splashOutcome[T]{}, fmt.Errorf("read terminal input during the startup splash: %w", event.err)
			case isInterrupt(event.msg):
				return splashOutcome[T]{kind: splashCancelled}, nil
			}
			// There is no dashboard yet to take other input.
		}
	}
}
