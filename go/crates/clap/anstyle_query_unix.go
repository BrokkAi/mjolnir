// Port of anstyle-query 1.1.5 src/lib.rs and src/windows.rs, for targets
// other than Windows

//go:build !windows

package clap

import (
	"os"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// anstyleQueryTermSupportsColor is anstyle_query::term_supports_color:
// check `TERM` for color support.
func anstyleQueryTermSupportsColor() bool {
	value, ok := os.LookupEnv("TERM")
	// If TERM isn't set, then we are in a weird environment that
	// probably doesn't support colors.
	if !ok {
		return false
	}
	if value == "dumb" {
		return false
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
	return anstyleQueryTermSupportsColor()
}

// anstyleQueryEnableAnsiColors is anstyle_query::windows::enable_ansi_colors:
// enable ANSI escape codes (ENABLE_VIRTUAL_TERMINAL_PROCESSING). For
// non-windows systems, returns `None`.
func anstyleQueryEnableAnsiColors() rs.Option[bool] {
	return rs.None[bool]()
}
