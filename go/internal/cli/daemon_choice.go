// Port of mj-cli/src/daemon/choice.rs.
//
// The first launch on a computer that has never run a daemon asks where the
// daemon should run, before anything connects to one.
//
// The answer is the `[daemon]` placement every later launch reads, so an SSH
// destination is only saved once a real connection to it has worked: a
// mistyped host would otherwise fail every following launch.

package cli

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// placementChoice is how the first-launch question ended.
type placementChoice int

const (
	// placementChoiceContinue: a placement is settled; start the dashboard.
	placementChoiceContinue placementChoice = iota
	// placementChoiceCancelled: the user ended input (Ctrl-D or Ctrl-C)
	// without choosing.
	placementChoiceCancelled
)

// choiceRemote is how asking for an SSH destination ended (Rust `Remote`).
type choiceRemote int

const (
	choiceRemoteSaved choiceRemote = iota
	choiceRemoteBack
	choiceRemoteCancelled
)

// isFirstLaunch is whether no daemon has ever run on this computer: no
// `[daemon]` table is set, and there is neither a store nor an endpoint.
// Either means a local daemon already ran, so a computer that stopped its
// daemon is not asked again. Nothing else counts: a configuration copied to
// a new computer, or a standalone `mj setup`, has not started a daemon
// there.
func isFirstLaunch() (bool, error) {
	panic("unported: mj-cli/src/daemon/choice.rs is_first_launch")
}

// askWhereDaemonRuns asks where the daemon runs when this is the first
// launch, and saves an SSH answer. Choosing this computer saves nothing: the
// default placement is a local daemon, and starting it creates the store
// that ends the first launch.
func askWhereDaemonRuns(ctx context.Context) (placementChoice, error) {
	panic("unported: mj-cli/src/daemon/choice.rs ask_where_daemon_runs")
}

type choiceAnswerKind int

const (
	choiceAnswerText choiceAnswerKind = iota
	// choiceAnswerEOF: end of input (Ctrl-D).
	choiceAnswerEOF
	// choiceAnswerInterrupted: Ctrl-C. The terminal read cannot be cancelled
	// and would take the next line typed, so this ends the first launch
	// rather than asking again.
	choiceAnswerInterrupted
)

// choiceAnswer is one answer to a question (Rust `Answer`); text is the
// trimmed line of a choiceAnswerText.
type choiceAnswer struct {
	kind choiceAnswerKind
	text string
}

// choiceAsk reads one line from the terminal (Rust `ask`). The terminal's
// own line editing is enough for a choice and a host name, and plain reading
// needs nothing from the terminal (no cursor-position reply) and writes to
// stdout like the dashboard, so redirecting stderr does not hide the
// question.
//
// Ctrl-C is listened for here, not left to the default action: once an SSH
// attempt has installed a handler, the default no longer applies, and a
// prompt that ignored the signal would leave the user without a way out.
func choiceAsk(ctx context.Context, label string) (choiceAnswer, error) {
	panic("unported: mj-cli/src/daemon/choice.rs ask")
}

// choiceInterrupted is closed on Ctrl-C (Rust `interrupted`). A handler that
// cannot be registered never closes it: that must not read as an interrupt.
// The listener is registered before it returns, so a Ctrl-C typed as soon as
// the question appears is kept.
func choiceInterrupted(ctx context.Context) <-chan struct{} {
	panic("unported: mj-cli/src/daemon/choice.rs interrupted")
}

func connectRemote(ctx context.Context) (choiceRemote, error) {
	panic("unported: mj-cli/src/daemon/choice.rs connect_remote")
}

// tryConnect makes the connection every later launch makes: discovery, the
// tunnel, and the proof of the daemon's identity. The tunnel is released
// again; the remote daemon keeps running. Ctrl-C cancels it and reaps the
// SSH process group, so a server that accepts the connection and then
// stalls can be left.
func tryConnect(ctx context.Context, placement *core.DaemonConfig) error {
	panic("unported: mj-cli/src/daemon/choice.rs try_connect")
}
