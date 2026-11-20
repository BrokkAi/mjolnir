// Port of anyhow 1.0.102 src/macros.rs, src/kind.rs and the format_err of
// src/lib.rs

package anyhow

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Errorf is `anyhow!("..", args)`, and with a return, `bail!("..", args)`:
// an error whose message is the format string filled in. format is a Rust
// format string, exactly as it is written in the Rust source; arguments are
// passed as for rs.Format, a captured `{name}` as rs.Arg("name", value).
//
//	bail!("unknown session {id}");
//	return anyhow.Errorf("unknown session {id}", rs.Arg("id", id))
//
// The other forms of the macro take one expression and no format string:
// `anyhow!(error)` for an error value is New(error), and `anyhow!(message)`
// for a string or another Display value is Msg(message).
//
// `ensure!(cond, "..", args)` is `if !cond { return anyhow.Errorf("..", args) }`.
func Errorf(format string, args ...any) error {
	if len(args) == 0 && !strings.ContainsAny(format, "{}") {
		// format_err: `format_args!("literal").as_str()` is the literal.
		return Msg(format)
	}
	return Msg(rs.Format(format, args...))
}
