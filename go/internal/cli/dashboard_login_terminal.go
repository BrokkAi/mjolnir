// Port of mj-cli/src/dashboard/login_terminal.rs.
//
// A modal terminal whose network I/O is owned by a cancellable task.

package cli

import (
	"context"
	"log/slog"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"

	"github.com/BrokkAi/mjolnir/go/internal/chat"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

// loginTerminalUpdate is Rust's login_terminal::Update (sealed:
// loginTerminalUpdateReady, loginTerminalUpdateFrame,
// loginTerminalUpdateFailed). A nil loginTerminalUpdate is Rust's None: the
// connection's channel closed.
type loginTerminalUpdate interface {
	isLoginTerminalUpdate()
}

type loginTerminalUpdateReady struct{}

type loginTerminalUpdateFrame struct{ output client.TerminalOutput }

type loginTerminalUpdateFailed struct{ message string }

func (loginTerminalUpdateReady) isLoginTerminalUpdate()  {}
func (loginTerminalUpdateFrame) isLoginTerminalUpdate()  {}
func (loginTerminalUpdateFailed) isLoginTerminalUpdate() {}

// loginTerminalConnection is Rust's `connection`: it opens the daemon's
// login terminal for profile and relays frames to updates and input to the
// daemon until the terminal exits, input closes, or ctx ends (abort).
func loginTerminalConnection(ctx context.Context, profile string, cols uint16, rows uint16, input *core.Unbounded[client.TerminalInput], updates chan<- loginTerminalUpdate) error {
	panic("unported: mj-cli/src/dashboard/login_terminal.rs connection")
}

// loginResult is how a login ended (Rust's Result<(), String>): err is nil
// when it signed in.
type loginResult struct {
	err error
}

type activeLogin struct {
	pane       *tui.LoginPane
	profile    string
	generation uint64
	// completed is nil until the login ended (Rust's Option).
	completed *loginResult
	input     *core.Unbounded[client.TerminalInput]
	// output is the receiving end of Rust's mpsc::channel(1): backpressure
	// reaches the daemon's PTY instead of accumulating a transcript faster
	// than the dashboard can render it.
	output <-chan loginTerminalUpdate
	// job aborts the connection task (Rust's AbortOnDropHandle).
	job context.CancelFunc
}

type loginTerminalPaneAction int

const (
	loginTerminalPaneActionContinue loginTerminalPaneAction = iota
	loginTerminalPaneActionClose
	loginTerminalPaneActionQuit
)

// startActiveLogin is Rust's `ActiveLogin::start`: it sizes the pane from
// the terminal and starts the connection task.
func startActiveLogin(profile string, generation uint64) (*activeLogin, error) {
	panic("unported: mj-cli/src/dashboard/login_terminal.rs ActiveLogin::start")
}

func (l *activeLogin) render(frame *chat.Frame, screen uv.Rectangle) {
	l.pane.Render(frame, screen)
}

func (l *activeLogin) send(input client.TerminalInput) {
	if err := l.input.Send(input); err != nil {
		// Completion belongs to the connection owner. Its terminal result
		// may already be queued; input cannot invent a competing result.
		slog.Debug("login input closed; awaiting connection result", "profile", l.profile)
	}
}

// accept applies one update to the pane; it answers how the login ended
// once it has.
func (l *activeLogin) accept(update loginTerminalUpdate) *loginResult {
	panic("unported: mj-cli/src/dashboard/login_terminal.rs ActiveLogin::accept")
}

func (l *activeLogin) event(event tea.Msg) loginTerminalPaneAction {
	panic("unported: mj-cli/src/dashboard/login_terminal.rs ActiveLogin::event")
}

// loginTerminalWait is Rust's `wait`: the next update of a login still
// running; a nil update when its channel closed. With no login, or a
// completed one, it waits until ctx ends and returns ctx's error.
func loginTerminalWait(ctx context.Context, login *activeLogin) (loginTerminalUpdate, error) {
	panic("unported: mj-cli/src/dashboard/login_terminal.rs wait")
}

// loginTerminalPump is Rust's `pump`. Fair selection keeps an output-heavy
// login and warm chats independently live even though the enclosing
// dashboard select prioritizes terminal input. fromLogin is false when the
// chats were pumped instead (Rust's outer None).
func loginTerminalPump(ctx context.Context, login *activeLogin, chats map[string]*chat.ActiveChat) (update loginTerminalUpdate, fromLogin bool, err error) {
	panic("unported: mj-cli/src/dashboard/login_terminal.rs pump")
}

func (c *dashboardContext) acceptLoginUpdate(update loginTerminalUpdate) {
	panic("unported: mj-cli/src/dashboard/login_terminal.rs DashboardContext::accept_login_update")
}

func (c *dashboardContext) loginEvent(event tea.Msg) {
	panic("unported: mj-cli/src/dashboard/login_terminal.rs DashboardContext::login_event")
}
