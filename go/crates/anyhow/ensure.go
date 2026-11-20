// Port of anyhow 1.0.102 src/ensure.rs

package anyhow

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// EnsureFailed is the error of `ensure!(condition)` with no message, for a
// condition that is not a comparison, or whose operands are not both Debug:
//
//	ensure!(lines[0].starts_with("<--bazelrc="));
//	if !strings.HasPrefix(lines[0], "<--bazelrc=") {
//		return anyhow.EnsureFailed(`lines[0].starts_with("<--bazelrc=")`)
//	}
//
// condition is the condition's text as Rust's stringify! gives it.
func EnsureFailed(condition string) error {
	return Msg("Condition failed: `" + condition + "`")
}

// EnsureFailedDebug is the error of `ensure!(lhs op rhs)` with no message,
// where the condition is a comparison (==, !=, <, <=, >, >=) at its top level
// and both operands are Debug. The message then also shows the two values,
//
//	Condition failed: `v + v == 1` (2 vs 1)
//
// unless the Debug text of either holds a space or a newline or is longer
// than 40 bytes, in which case it is the message of EnsureFailed.
func EnsureFailedDebug(condition string, lhs, rhs any) error {
	return render("Condition failed: `"+condition+"`", lhs, rhs)
}

// bufLen is the size of the crate's Buf, the fixed buffer an operand's Debug
// text must fit in.
const bufLen = 40

// bufWrite is Buf::write_str over a whole Debug text: it refuses text with a
// space or a newline, and text that does not fit.
func bufWrite(s string) (string, bool) {
	if strings.ContainsAny(s, " \n") {
		return "", false
	}
	if len(s) > bufLen {
		return "", false
	}
	return s, true
}

func render(msg string, lhs, rhs any) error {
	if lhsStr, ok := bufWrite(rs.DebugString(lhs)); ok {
		if rhsStr, ok := bufWrite(rs.DebugString(rhs)); ok {
			// "{msg} ({lhs} vs {rhs})"
			return Msg(msg + " (" + lhsStr + " vs " + rhsStr + ")")
		}
	}
	return Msg(msg)
}
