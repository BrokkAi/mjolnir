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
	"bufio"
	"context"
	"fmt"
	"io"
	"os"
	"os/signal"
	"strings"
	"sync"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/store"
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
	// An explicit `[daemon]` table is a choice, even `runtime = "host"`.
	set, err := core.DaemonConfigIsSetIn(core.ConfigPath())
	if err != nil {
		return false, err
	}
	if set {
		return false, nil
	}
	return !pathExists(store.DatabasePath()) && !pathExists(client.MetadataPath()), nil
}

// pathExists is Rust's Path::exists: false when the path cannot be read.
func pathExists(path string) bool {
	_, err := os.Stat(path)
	return err == nil
}

// askWhereDaemonRuns asks where the daemon runs when this is the first
// launch, and saves an SSH answer. Choosing this computer saves nothing: the
// default placement is a local daemon, and starting it creates the store
// that ends the first launch.
func askWhereDaemonRuns(ctx context.Context) (placementChoice, error) {
	first, err := isFirstLaunch()
	if err != nil {
		return 0, err
	}
	if !first {
		return placementChoiceContinue, nil
	}
	for {
		fmt.Println("No Mjolnir daemon has run on this computer. Where should it run?")
		fmt.Println("  1) Start a daemon on this computer")
		fmt.Println("  2) Connect to a daemon on another computer over SSH")
		// Leaving, not the default: the default would start a daemon and a
		// store, which ends this prompt for good.
		answer, err := choiceAsk(ctx, "Choose 1 or 2 [1]: ")
		if err != nil {
			return 0, err
		}
		if answer.kind != choiceAnswerText {
			return placementChoiceCancelled, nil
		}
		switch answer.text {
		case "", "1":
			return placementChoiceContinue, nil
		case "2":
			remote, err := connectRemote(ctx)
			if err != nil {
				return 0, err
			}
			switch remote {
			case choiceRemoteSaved:
				return placementChoiceContinue, nil
			case choiceRemoteCancelled:
				return placementChoiceCancelled, nil
			}
		default:
			fmt.Println("Enter 1 or 2.")
		}
	}
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

// choiceStdin is the process's buffered standard input, as Rust's global
// Stdin is: a line read past by one question stays for the next.
var choiceStdin = sync.OnceValue(func() *bufio.Reader { return bufio.NewReader(os.Stdin) })

// choiceLine is one line read from standard input.
type choiceLine struct {
	text string
	read bool
	err  error
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
	listening, stop := context.WithCancel(ctx)
	defer stop()
	// The listener is registered before the question is on screen, so a
	// Ctrl-C typed as soon as the question appears is kept.
	interrupt := choiceInterrupted(listening)
	select {
	case <-interrupt:
		return choiceAnswer{kind: choiceAnswerInterrupted}, nil
	default:
	}
	// os.Stdout is unbuffered, so the label is on screen without the flush
	// Rust's line-buffered stdout needs.
	if _, err := fmt.Print(label); err != nil {
		return choiceAnswer{}, err
	}
	lines := make(chan choiceLine, 1)
	go func() {
		text, err := choiceStdin().ReadString('\n')
		if err == io.EOF {
			lines <- choiceLine{text: text, read: text != ""}
			return
		}
		if err != nil {
			lines <- choiceLine{err: fmt.Errorf("read terminal response: %w", err)}
			return
		}
		lines <- choiceLine{text: text, read: true}
	}()
	select {
	case line := <-lines:
		if line.err != nil {
			return choiceAnswer{}, line.err
		}
		if !line.read {
			return choiceAnswer{kind: choiceAnswerEOF}, nil
		}
		return choiceAnswer{kind: choiceAnswerText, text: strings.TrimSpace(line.text)}, nil
	case <-interrupt:
		return choiceAnswer{kind: choiceAnswerInterrupted}, nil
	}
}

// choiceInterrupted is closed on Ctrl-C (Rust `interrupted`). A handler that
// cannot be registered never closes it: that must not read as an interrupt.
// The listener is registered before it returns, so a Ctrl-C typed as soon as
// the question appears is kept. It stops listening when ctx ends.
func choiceInterrupted(ctx context.Context) <-chan struct{} {
	interrupted := make(chan struct{})
	signals := make(chan os.Signal, 1)
	signal.Notify(signals, os.Interrupt)
	go func() {
		defer signal.Stop(signals)
		select {
		case <-signals:
			close(interrupted)
		case <-ctx.Done():
		}
	}()
	return interrupted
}

func connectRemote(ctx context.Context) (choiceRemote, error) {
	for {
		answer, err := choiceAsk(ctx, "SSH destination (user@host, or a Host from your SSH config; empty to go back): ")
		if err != nil {
			return 0, err
		}
		if answer.kind != choiceAnswerText {
			return choiceRemoteCancelled, nil
		}
		host := answer.text
		if host == "" {
			return choiceRemoteBack, nil
		}
		if strings.HasPrefix(host, "-") {
			fmt.Println("An SSH destination cannot start with '-'.")
			continue
		}
		placement := core.DefaultDaemonConfig()
		placement.Runtime = core.DaemonRuntimeSsh
		placement.Host = &host
		fmt.Printf("Connecting to %s, starting its daemon if needed... (Ctrl-C to cancel)\n", host)
		if err := tryConnect(ctx, &placement); err != nil {
			fmt.Printf("Could not use the daemon on %s: %s\n\n", host, err)
			continue
		}
		// The file must still have no `[daemon]` table, as when this prompt
		// was shown: a second first launch that chose another host first is
		// not replaced.
		if err := placement.ReplacePlacement(core.ConfigPath(), nil); err != nil {
			// The connection worked; only the saving did not. Report it and
			// leave the menu open so local remains a choice.
			fmt.Printf("The connection works, but it could not be saved: %s\n\n", err)
			continue
		}
		fmt.Printf("This computer will use the daemon on %s. To change that, edit [daemon] in %s.\n", host, core.ConfigPath())
		return choiceRemoteSaved, nil
	}
}

// tryConnect makes the connection every later launch makes: discovery, the
// tunnel, and the proof of the daemon's identity. The tunnel is released
// again; the remote daemon keeps running. Ctrl-C cancels it and reaps the
// SSH process group, so a server that accepts the connection and then
// stalls can be left.
func tryConnect(ctx context.Context, placement *core.DaemonConfig) error {
	var instance *string
	if name, ok := placement.RemoteInstance(); ok {
		instance = &name
	}
	listening, stop := context.WithCancel(ctx)
	defer stop()
	lease, err := client.Open(ctx, *placement, instance, client.DiscoveryIntentStart, choiceInterrupted(listening))
	if err != nil {
		return err
	}
	return lease.Terminate(ctx)
}
