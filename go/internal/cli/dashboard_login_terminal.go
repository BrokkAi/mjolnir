// Port of mj-cli/src/dashboard/login_terminal.rs.
//
// A modal terminal whose network I/O is owned by a cancellable task.

package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"runtime"

	tea "charm.land/bubbletea/v2"
	uv "github.com/charmbracelet/ultraviolet"
	"github.com/charmbracelet/x/term"

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
	daemonClient, err := ConnectOrStart(ctx)
	if err != nil {
		return err
	}
	terminal, err := daemonClient.LoginTerminal(ctx, profile, cols, rows)
	daemonClient.Close()
	if err != nil {
		return err
	}
	// The placement lease lives until the terminal closes.
	placementLease := terminal.Connector
	defer runtime.KeepAlive(placementLease)
	stream := terminal.Stream
	defer stream.Close()
	// An abort closes the stream, which ends the read below.
	stopClosing := context.AfterFunc(ctx, func() { _ = stream.Close() })
	defer stopClosing()
	send := func(update loginTerminalUpdate) error {
		select {
		case updates <- update:
			return nil
		case <-ctx.Done():
			return fmt.Errorf("login pane closed: %w", ctx.Err())
		}
	}
	if err := send(loginTerminalUpdateReady{}); err != nil {
		return err
	}

	// One reader keeps each frame read whole across keyboard events:
	// abandoning a partially consumed frame and starting another corrupts
	// framing.
	type frameRead struct {
		output client.TerminalOutput
		err    error
	}
	frames := make(chan frameRead)
	readerCtx, stopReader := context.WithCancel(ctx)
	defer stopReader()
	go func() {
		for {
			body, err := client.ReadFrame[json.RawMessage](stream)
			var output client.TerminalOutput
			if err == nil {
				output, err = client.UnmarshalTerminalOutputJSON(body)
				if err != nil {
					err = fmt.Errorf("decode daemon frame: %w", err)
				}
			}
			select {
			case frames <- frameRead{output: output, err: err}:
			case <-readerCtx.Done():
				return
			}
			if err != nil {
				return
			}
		}
	}()
	// Input arrives over its own goroutine so the loop can wait on a frame
	// and on input at once.
	inputs := make(chan client.TerminalInput)
	go func() {
		defer close(inputs)
		for {
			next, ok := input.Recv(readerCtx)
			if !ok {
				return
			}
			select {
			case inputs <- next:
			case <-readerCtx.Done():
				return
			}
		}
	}()
	for {
		select {
		case frame := <-frames:
			if frame.err != nil {
				return frame.err
			}
			_, exited := frame.output.(client.TerminalOutputExited)
			_, failed := frame.output.(client.TerminalOutputFailed)
			if err := send(loginTerminalUpdateFrame{output: frame.output}); err != nil {
				return err
			}
			if exited || failed {
				return nil
			}
		case next, ok := <-inputs:
			if !ok {
				if ctx.Err() != nil {
					return ctx.Err()
				}
				return nil
			}
			if err := client.WriteFrame(stream, next); err != nil {
				return err
			}
		case <-ctx.Done():
			return ctx.Err()
		}
	}
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
	width, height, err := term.GetSize(os.Stdout.Fd())
	if err != nil {
		return nil, fmt.Errorf("read login pane size: %w", err)
	}
	cols, rows := tui.LoginPaneTerminalSize(width, height)
	input := core.NewUnbounded[client.TerminalInput]()
	// Backpressure reaches the daemon's PTY instead of accumulating a
	// transcript faster than the dashboard can render it.
	updates := make(chan loginTerminalUpdate, 1)
	ctx, abort := context.WithCancel(context.Background())
	core.Spawn(func() struct{} {
		worker := core.Spawn(func() error {
			return loginTerminalConnection(ctx, profile, uint16(cols), uint16(rows), input, updates)
		})
		// Both senders are gone once the job ends: the receiver then sees
		// the channel close, as Rust's does when the senders drop.
		defer func() {
			<-worker.Done()
			close(updates)
		}()
		select {
		case <-worker.Done():
		case <-ctx.Done():
			return struct{}{}
		}
		connectionErr, joinErr := worker.Join(context.Background())
		var failure string
		switch {
		case joinErr != nil:
			failure = fmt.Sprintf("login connection task failed: %v", joinErr)
		case connectionErr != nil:
			failure = connectionErr.Error()
		default:
			return struct{}{}
		}
		if ctx.Err() != nil {
			// Aborted: Rust's job is dropped before it reports.
			return struct{}{}
		}
		message := "The daemon connection ended during login or harness preparation; retry Login. " + failure
		select {
		case updates <- loginTerminalUpdateFailed{message: message}:
		case <-ctx.Done():
			slog.Debug("closed login pane connection ended", "error", failure)
		}
		return struct{}{}
	})
	return &activeLogin{
		pane:       tui.NewLoginPane(profile, cols, rows),
		profile:    profile,
		generation: generation,
		input:      input,
		output:     updates,
		job:        abort,
	}, nil
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

// release is dropping the Rust login: its connection job is aborted.
func (l *activeLogin) release() {
	if l.job != nil {
		l.job()
	}
}

// accept applies one update to the pane; it answers how the login ended
// once it has.
func (l *activeLogin) accept(update loginTerminalUpdate) *loginResult {
	var result loginResult
	switch update := update.(type) {
	case loginTerminalUpdateReady:
		l.pane.Status("Sign in on the daemon. Browser localhost callbacks may need that computer.")
		return nil
	case loginTerminalUpdateFrame:
		switch output := update.output.(type) {
		case client.TerminalOutputBytes:
			if reply := l.pane.Output(output.Bytes); len(reply) != 0 {
				l.send(client.TerminalInputBytes{Bytes: reply})
			}
			return nil
		case client.TerminalOutputExited:
			if output.Code != 0 {
				result.err = fmt.Errorf("Login exited with code %d.", output.Code)
			}
		case client.TerminalOutputFailed:
			result.err = errors.New(output.Message)
		}
	case loginTerminalUpdateFailed:
		result.err = errors.New(update.message)
	case nil:
		result.err = errors.New("The daemon connection ended during login or harness preparation; retry Login.")
	}
	if result.err == nil {
		l.pane.Status("Signed in. Enter returns to Settings.")
	} else {
		l.pane.Status(result.err.Error() + " Enter returns to Settings.")
	}
	l.completed = &result
	completed := result
	return &completed
}

func (l *activeLogin) event(event tea.Msg) loginTerminalPaneAction {
	switch event := event.(type) {
	case tea.KeyPressMsg:
		if event.Mod.Contains(tea.ModCtrl) {
			if event.Code == 'q' {
				return loginTerminalPaneActionQuit
			}
			if event.Code == ']' {
				return loginTerminalPaneActionClose
			}
		}
		if l.completed != nil && (event.Code == tea.KeyEnter || event.Code == tea.KeyEscape) {
			return loginTerminalPaneActionClose
		}
		if l.completed == nil {
			if bytes := l.pane.Key(event); bytes != nil {
				l.send(client.TerminalInputBytes{Bytes: bytes})
			}
		}
	case tea.PasteMsg:
		if l.completed == nil {
			l.send(client.TerminalInputBytes{Bytes: l.pane.Paste(event.Content)})
		}
	case tea.WindowSizeMsg:
		cols, rows := tui.LoginPaneTerminalSize(event.Width, event.Height)
		l.pane.Resize(cols, rows)
		if l.completed == nil {
			l.send(client.TerminalInputResize{Cols: uint16(cols), Rows: uint16(rows)})
		}
	}
	return loginTerminalPaneActionContinue
}

// loginTerminalWait is Rust's `wait`: the next update of a login still
// running; a nil update when its channel closed. With no login, or a
// completed one, it waits until ctx ends and returns ctx's error.
func loginTerminalWait(ctx context.Context, login *activeLogin) (loginTerminalUpdate, error) {
	if login == nil || login.completed != nil {
		<-ctx.Done()
		return nil, ctx.Err()
	}
	select {
	case update, open := <-login.output:
		if !open {
			return nil, nil
		}
		return update, nil
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// loginTerminalPump is Rust's `pump`. Fair selection keeps an output-heavy
// login and warm chats independently live even though the enclosing
// dashboard select prioritizes terminal input. fromLogin is false when the
// chats were pumped instead (Rust's outer None).
//
// The chats are pumped on the calling goroutine, which owns them; only the
// wait for the login's output runs beside it. A login update that arrives
// ends the chat pump, which is cancel safe, and an update received is always
// returned, so none is lost. When a chat message and a login update land
// together, the chat message has been applied and the login update is the
// answer.
func loginTerminalPump(ctx context.Context, login *activeLogin, chats map[string]*chat.ActiveChat) (update loginTerminalUpdate, fromLogin bool, err error) {
	if login == nil || login.completed != nil {
		pumpChats(ctx, chats)
		return nil, false, ctx.Err()
	}
	chatsCtx, stopChats := context.WithCancel(ctx)
	defer stopChats()
	watchCtx, stopWatching := context.WithCancel(ctx)
	defer stopWatching()
	type received struct {
		update loginTerminalUpdate
		ok     bool
	}
	watched := make(chan received, 1)
	go func() {
		select {
		case update := <-login.output:
			// A closed channel yields nil, Rust's None.
			watched <- received{update: update, ok: true}
			stopChats()
		case <-watchCtx.Done():
			watched <- received{}
		}
	}()
	pumpChats(chatsCtx, chats)
	stopWatching()
	if got := <-watched; got.ok {
		return got.update, true, nil
	}
	return nil, false, ctx.Err()
}

func (c *dashboardContext) acceptLoginUpdate(update loginTerminalUpdate) {
	if c.login == nil {
		return
	}
	if result := c.login.accept(update); result != nil {
		c.dashboard.SetupLoginFinished(c.login.generation, c.login.profile, tui.SetupLoginOutcomeFromResult(result.err))
	}
}

func (c *dashboardContext) loginEvent(event tea.Msg) {
	if c.login == nil {
		panic("login pane owns input")
	}
	action := c.login.event(event)
	if action == loginTerminalPaneActionContinue {
		return
	}
	login := c.login
	c.login = nil
	if login.completed == nil {
		c.dashboard.SetupLoginFinished(login.generation, login.profile, tui.SetupLoginOutcomeCancelled{})
	}
	login.release()
	if action == loginTerminalPaneActionQuit {
		c.requestShutdown()
	}
}
