// Port of anyhow 1.0.102 src/fmt.rs

package anyhow

import (
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// DisplayString is `format!("{}", error)` for an anyhow error: the outermost
// message alone.
func DisplayString(err error) string {
	return displayString(errorObject(err))
}

// AlternateString is `format!("{:#}", error)` for an anyhow error: the
// message of every error in the chain, joined by ": ".
func AlternateString(err error) string {
	object := errorObject(err)
	var out strings.Builder
	out.WriteString(displayString(object))
	for cause := source(object); cause != nil; cause = source(cause) {
		out.WriteString(": ")
		out.WriteString(displayString(cause))
	}
	return out.String()
}

// DebugString is `format!("{:?}", error)` for an anyhow error: the outermost
// message, and when the error has a cause, a "Caused by:" section holding the
// cause alone or the causes numbered from 0. The lines of a cause after its
// first are indented under it.
//
// There is no backtrace section: see the package documentation.
func DebugString(err error) string {
	object := errorObject(err)
	var out strings.Builder
	out.WriteString(displayString(object))

	if cause := source(object); cause != nil {
		out.WriteString("\n\nCaused by:")
		multiple := source(cause) != nil
		n := 0
		for cause := cause; cause != nil; cause = source(cause) {
			out.WriteByte('\n')
			writer := indented{inner: &out}
			if multiple {
				writer.number = rs.Some(n)
			}
			writer.writeStr(displayString(cause))
			n++
		}
	}

	return out.String()
}

// PrettyDebugString is `format!("{:#?}", error)` for an anyhow error: the
// Debug form of the error object, which for an error with context is
//
//	Error {
//	    context: "..",
//	    source: ..,
//	}
func PrettyDebugString(err error) string {
	if impl, ok := err.(*errorImpl); ok {
		return rs.PrettyDebugString(impl)
	}
	return rs.PrettyDebugString(err)
}

// display is ErrorImpl::display.
func (e *errorImpl) display(f *rs.Formatter) error {
	// `write!(f, "{}", error)` formats with the default flags, so a width or
	// precision given for the anyhow error is not applied.
	if err := f.WriteStr(displayString(e.object)); err != nil {
		return err
	}

	if f.Alternate() {
		for cause := source(e.object); cause != nil; cause = source(cause) {
			if err := f.WriteStr(": " + displayString(cause)); err != nil {
				return err
			}
		}
	}

	return nil
}

// debug is ErrorImpl::debug.
func (e *errorImpl) debug(f *rs.Formatter) error {
	if f.Alternate() {
		return rs.DebugFmt(e.object, f)
	}

	return f.WriteStr(DebugString(e))
}

// indented is the crate's Indented writer: it puts a cause under "Caused
// by:", after its number when the causes are numbered.
type indented struct {
	inner   *strings.Builder
	number  rs.Option[int]
	started bool
}

func (w *indented) writeStr(s string) {
	for i, line := range strings.Split(s, "\n") {
		if !w.started {
			w.started = true
			if number, ok := w.number.Get(); ok {
				// "{: >5}: "
				digits := strconv.Itoa(number)
				for pad := len(digits); pad < 5; pad++ {
					w.inner.WriteByte(' ')
				}
				w.inner.WriteString(digits)
				w.inner.WriteString(": ")
			} else {
				w.inner.WriteString("    ")
			}
		} else if i > 0 {
			w.inner.WriteByte('\n')
			if w.number.IsSome() {
				w.inner.WriteString("       ")
			} else {
				w.inner.WriteString("    ")
			}
		}

		w.inner.WriteString(line)
	}
}
