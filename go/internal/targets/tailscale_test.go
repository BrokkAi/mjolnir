// Port of the tests in mj-targets/src/tailscale.rs.

package targets

import (
	"errors"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

type tailscaleFakeExecutor struct {
	core.CommandExecutorDefaults
	mu         sync.Mutex
	outputs    []core.CommandOutput
	commands   []core.CommandSpec
	issueFiles bool
}

func newTailscaleFakeExecutor(outputs []core.CommandOutput, issueFiles bool) *tailscaleFakeExecutor {
	return &tailscaleFakeExecutor{outputs: outputs, issueFiles: issueFiles}
}

func (e *tailscaleFakeExecutor) PauseDeadline() *core.DeadlinePause { return core.DeadlinePauseNone() }

func (e *tailscaleFakeExecutor) CancellationRequestedWithoutDeadline() bool {
	return e.CancellationRequested()
}

func (e *tailscaleFakeExecutor) ExecuteWithoutDeadline(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *tailscaleFakeExecutor) ExecuteWithStdinWithoutDeadline(command *core.CommandSpec, input io.Reader) (core.CommandOutput, error) {
	return e.ExecuteWithStdin(command, input)
}

func (e *tailscaleFakeExecutor) ExecuteCleanup(command *core.CommandSpec) (core.CommandOutput, error) {
	return e.Execute(command)
}

func (e *tailscaleFakeExecutor) ExecuteCleanupWithTimeout(command *core.CommandSpec, _ time.Duration) (core.CommandOutput, error) {
	return e.ExecuteCleanup(command)
}

func (e *tailscaleFakeExecutor) Execute(command *core.CommandSpec) (core.CommandOutput, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	e.commands = append(e.commands, *command)
	if e.issueFiles && len(command.Args) > 0 && command.Args[0] == "cert" {
		if err := os.WriteFile(command.Args[2], []byte("-----BEGIN CERTIFICATE-----\ncert\n"), 0o666); err != nil {
			return core.CommandOutput{}, err
		}
		if err := os.WriteFile(command.Args[4], []byte("-----BEGIN PRIVATE KEY-----\nkey\n"), 0o666); err != nil {
			return core.CommandOutput{}, err
		}
	}
	if len(e.outputs) == 0 {
		return core.CommandOutput{}, errors.New("fake command output missing")
	}
	output := e.outputs[0]
	e.outputs = e.outputs[1:]
	return output, nil
}

func tailscaleOutput(status int, stdout, stderr string) core.CommandOutput {
	return core.CommandOutput{Status: status, Stdout: []byte(stdout), Stderr: []byte(stderr)}
}

func tailscaleIsFile(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular()
}

func TestDiscoversDomainAndMintsExpectedCertificate(t *testing.T) {
	directory := t.TempDir()
	executor := newTailscaleFakeExecutor([]core.CommandOutput{
		tailscaleOutput(0, `{"BackendState":"Running","CertDomains":["minas.tail.ts.net."]}`, ""),
		tailscaleOutput(0, "", ""),
	}, true)

	tls, err := prepareTailscaleTLSWithBinary(directory, "/usr/bin/tailscale", executor)
	if err != nil {
		t.Fatal(err)
	}

	if got := tls.CertDomain(); got != "minas.tail.ts.net" {
		t.Fatalf("cert domain = %q", got)
	}
	if !tailscaleIsFile(tls.CertPath()) || !tailscaleIsFile(tls.KeyPath()) {
		t.Fatal("the certificate pair was not persisted")
	}
	commands := executor.commands
	if !slices.Equal(commands[0].Args, []string{"status", "--json"}) {
		t.Fatalf("first command = %q", commands[0].Args)
	}
	if commands[1].Args[0] != "cert" {
		t.Fatalf("second command = %q", commands[1].Args)
	}
	if commands[1].Args[5] != "minas.tail.ts.net" {
		t.Fatalf("second command = %q", commands[1].Args)
	}
}

func TestMissingCertificateDomainsHasActionableGuidance(t *testing.T) {
	executor := newTailscaleFakeExecutor([]core.CommandOutput{tailscaleOutput(0, `{"BackendState":"Running"}`, "")}, false)
	_, err := discoverWithBinary("tailscale", executor)
	if err == nil {
		t.Fatal("discovery without certificate domains succeeded")
	}
	if !strings.Contains(err.Error(), "MagicDNS") || !strings.Contains(err.Error(), "mj daemon restart") {
		t.Fatal(err)
	}
}

func TestStoppedOrMalformedStatusIsRejected(t *testing.T) {
	stopped := newTailscaleFakeExecutor([]core.CommandOutput{
		tailscaleOutput(0, `{"BackendState":"Stopped","CertDomains":["host.ts.net"]}`, ""),
	}, false)
	if _, err := discoverWithBinary("tailscale", stopped); err == nil || !strings.Contains(err.Error(), "not running") {
		t.Fatalf("stopped status: %v", err)
	}

	malformed := newTailscaleFakeExecutor([]core.CommandOutput{tailscaleOutput(0, "not-json", "")}, false)
	if _, err := discoverWithBinary("tailscale", malformed); err == nil || !strings.Contains(err.Error(), "parse") {
		t.Fatalf("malformed status: %v", err)
	}
}

func TestCommandFailureDoesNotReplaceExistingPair(t *testing.T) {
	directory := t.TempDir()
	certPath := filepath.Join(directory, certFile)
	keyPath := filepath.Join(directory, keyFile)
	if err := os.WriteFile(certPath, []byte("old-cert"), 0o666); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(keyPath, []byte("old-key"), 0o666); err != nil {
		t.Fatal(err)
	}
	executor := newTailscaleFakeExecutor([]core.CommandOutput{tailscaleOutput(1, "", "issuance failed")}, false)
	identity := tailscale{binary: "tailscale", certDomain: "host.ts.net"}

	if err := mintCertificate(&identity, certPath, keyPath, executor); err == nil {
		t.Fatal("a failed issuance succeeded")
	}
	if cert, _ := os.ReadFile(certPath); string(cert) != "old-cert" {
		t.Fatalf("certificate = %q", cert)
	}
	if key, _ := os.ReadFile(keyPath); string(key) != "old-key" {
		t.Fatalf("key = %q", key)
	}
}

func TestPersistedPrivateKeyIsOwnerOnly(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("Unix only, as in Rust")
	}
	directory := t.TempDir()
	executor := newTailscaleFakeExecutor([]core.CommandOutput{
		tailscaleOutput(0, `{"BackendState":"Running","CertDomains":["host.ts.net"]}`, ""),
		tailscaleOutput(0, "", ""),
	}, true)
	tls, err := prepareTailscaleTLSWithBinary(directory, "tailscale", executor)
	if err != nil {
		t.Fatal(err)
	}

	info, err := os.Stat(tls.KeyPath())
	if err != nil {
		t.Fatal(err)
	}
	if mode := info.Mode().Perm(); mode != 0o600 {
		t.Fatalf("key mode = %o, want 600", mode)
	}
}
