// Port of anstyle-query 1.1.5 src/lib.rs and src/windows.rs, for Windows

//go:build windows

package clap

import (
	"os"

	"golang.org/x/sys/windows"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// anstyleQueryTermSupportsColor is anstyle_query::term_supports_color:
// check `TERM` for color support.
func anstyleQueryTermSupportsColor() bool {
	// On Windows, if TERM isn't set, then we shouldn't automatically
	// assume that colors aren't allowed. This is unlike Unix environments
	// where TERM is more rigorously set.
	if value, ok := os.LookupEnv("TERM"); ok {
		if value == "dumb" {
			return false
		}
	}
	return true
}

// anstyleQueryTermSupportsAnsiColor is
// anstyle_query::term_supports_ansi_color: check `TERM` for ANSI color
// support.
//
// On Windows, you might need to also check windows::enable_ansi_colors as
// ANSI color support is opt-in, rather than assumed.
func anstyleQueryTermSupportsAnsiColor() bool {
	value, ok := os.LookupEnv("TERM")
	if !ok {
		return false
	}
	// cygwin doesn't seem to support ANSI escape sequences
	// and instead has its own variety. However, the Windows
	// console API may be available.
	if value == "dumb" || value == "cygwin" {
		return false
	}
	return true
}

func anstyleQueryEnableVt(handle windows.Handle) error {
	if handle == 0 {
		// The console is detached.
		return windows.ERROR_BROKEN_PIPE
	}

	var dwmode uint32
	if err := windows.GetConsoleMode(handle, &dwmode); err != nil {
		return err
	}

	dwmode |= windows.ENABLE_VIRTUAL_TERMINAL_PROCESSING

	return windows.SetConsoleMode(handle, dwmode)
}

// anstyleQueryEnableVirtualTerminalProcessing is
// anstyle_query::windows::enable_virtual_terminal_processing.
func anstyleQueryEnableVirtualTerminalProcessing() error {
	stdoutHandle, _ := windows.GetStdHandle(windows.STD_OUTPUT_HANDLE)
	stderrHandle, _ := windows.GetStdHandle(windows.STD_ERROR_HANDLE)

	if err := anstyleQueryEnableVt(stdoutHandle); err != nil {
		return err
	}
	if stdoutHandle != stderrHandle {
		if err := anstyleQueryEnableVt(stderrHandle); err != nil {
			return err
		}
	}

	return nil
}

// anstyleQueryEnableAnsiColors is anstyle_query::windows::enable_ansi_colors:
// enable ANSI escape codes (ENABLE_VIRTUAL_TERMINAL_PROCESSING).
//
// For non-windows systems, returns `None`.
func anstyleQueryEnableAnsiColors() rs.Option[bool] {
	return rs.Some(anstyleQueryEnableVirtualTerminalProcessing() == nil)
}
