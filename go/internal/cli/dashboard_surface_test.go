// Port of the tests in mj-cli/src/dashboard/surface.rs.

package cli

import (
	"errors"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/tui"
)

func TestSshSelectionUsesOnlyTheConnectingTerminalClipboard(t *testing.T) {
	for _, indicator := range []string{"SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"} {
		overSSH := runningOverSSH(func(name string) (string, bool) {
			if name == indicator || name == "DISPLAY" {
				return "present", true
			}
			return "", false
		})
		var copied *string
		if err := copySelectedText(
			"selected text\nsecond line",
			overSSH,
			func(string) { panic("SSH selection must not open the remote desktop clipboard") },
			func(text string) error {
				copied = &text
				return nil
			},
		); err != nil {
			t.Fatal(err)
		}
		if copied == nil || *copied != "selected text\nsecond line" {
			t.Fatalf("%s: terminal copy got %v", indicator, copied)
		}
	}
}

func TestLocalSelectionKeepsBothClipboardRoutesWithAbsentOrEmptySshVariables(t *testing.T) {
	for _, present := range []bool{false, true} {
		overSSH := runningOverSSH(func(string) (string, bool) { return "", present })
		var systemText, terminalText *string
		if err := copySelectedText(
			"local selection",
			overSSH,
			func(text string) { systemText = &text },
			func(text string) error {
				terminalText = &text
				return nil
			},
		); err != nil {
			t.Fatal(err)
		}
		if systemText == nil || *systemText != "local selection" {
			t.Fatalf("present=%v: system copy got %v", present, systemText)
		}
		if terminalText == nil || *terminalText != "local selection" {
			t.Fatalf("present=%v: terminal copy got %v", present, terminalText)
		}
	}
}

func TestSshTerminalWriteFailureIsReportedWithoutTryingTheRemoteClipboard(t *testing.T) {
	err := copySelectedText(
		"selection",
		true,
		func(string) { panic("SSH selection must not open the remote desktop clipboard") },
		func(string) error { return errors.New("terminal write failed") },
	)
	if err == nil || err.Error() != "terminal write failed" {
		t.Fatalf("error = %v", err)
	}
}

func TestCopyingASessionIdNamesItsShortFormOrTheClipboardFailure(t *testing.T) {
	sessionID := "0123456789abcdef"
	notice := copiedSessionIDNotice(sessionID)
	dashboard := tui.NewDashboardState(core.DefaultConfig(), core.NewStateState(), map[string]client.ProfileQuota{})
	var systemText, terminalText *string
	copyAndReport(
		dashboard,
		sessionID,
		notice,
		false,
		func(text string) { systemText = &text },
		func(text string) error {
			terminalText = &text
			return nil
		},
	)
	if systemText == nil || *systemText != sessionID {
		t.Fatalf("system copy got %v", systemText)
	}
	if terminalText == nil || *terminalText != sessionID {
		t.Fatalf("terminal copy got %v", terminalText)
	}
	if current, ok := dashboard.Notice(); !ok || current != "Copied session ID 01234567" {
		t.Fatalf("notice = %q, %v", current, ok)
	}

	copyAndReport(
		dashboard,
		sessionID,
		notice,
		true,
		func(string) { panic("SSH copy must not open the remote desktop clipboard") },
		func(string) error { return errors.New("terminal write failed") },
	)
	if current, ok := dashboard.Notice(); !ok || current != "Copy to the terminal clipboard failed: terminal write failed" {
		t.Fatalf("notice = %q, %v", current, ok)
	}
}
