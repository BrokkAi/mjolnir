// Port of std 1.99.1 src/sys/args/windows.rs (parse_lp_cmd_line) and
// src/sys/helpers/wstr.rs (WStrUnits)
//
// The parser only reads the code units it is given, so it is built on every
// platform and its tests run on any host. The Windows command line is just a
// string
// <https://docs.microsoft.com/en-us/archive/blogs/larryosterman/the-windows-command-line-is-just-a-string>;
// this turns it into a list of arguments.

package rsenv

import "github.com/BrokkAi/mjolnir/go/rs/rspath"

// wstrUnits is sys::helpers::WStrUnits: the code units of a wide string up
// to its terminating zero, read one at a time.
type wstrUnits struct {
	units []uint16
}

func (w *wstrUnits) peek() (uint16, bool) {
	if len(w.units) == 0 {
		return 0, false
	}
	return w.units[0], true
}

func (w *wstrUnits) next() (uint16, bool) {
	unit, ok := w.peek()
	if ok {
		w.units = w.units[1:]
	}
	return unit, ok
}

// advanceWhile advances the iterator while predicate returns true, and
// returns the number of items it advanced by.
func (w *wstrUnits) advanceWhile(predicate func(uint16) bool) int {
	counter := 0
	for {
		unit, ok := w.peek()
		if !ok || !predicate(unit) {
			return counter
		}
		counter++
		w.next()
	}
}

// parseLpCmdLine implements the Windows command-line argument parsing
// algorithm.
//
// Microsoft's documentation for the Windows CLI argument format can be found at
// <https://docs.microsoft.com/en-us/cpp/cpp/main-function-command-line-args?view=msvc-160#parsing-c-command-line-arguments>
//
// A more in-depth explanation is here:
// <https://daviddeley.com/autohotkey/parameters/parameters.htm#WIN>
//
// Windows includes a function to do command line parsing in shell32.dll.
// However, this is not used for two reasons:
//
//  1. Linking with that DLL causes the process to be registered as a GUI
//     application. GUI applications add a bunch of overhead, even if no
//     windows are drawn. See
//     <https://randomascii.wordpress.com/2018/12/03/a-not-called-function-can-cause-a-5x-slowdown/>.
//
//  2. It does not follow the modern C/C++ argv rules outlined in the first
//     two links above.
//
// This function was tested for equivalence to the C/C++ parsing rules using
// an extensive test suite available at
// <https://github.com/ChrisDenton/winarg/tree/std>.
//
// present says whether the command line pointer was not null, and lpCmdLine
// holds its code units up to the terminating zero.
func parseLpCmdLine(present bool, lpCmdLine []uint16, exeName func() string) []string {
	const (
		backslash = uint16('\\')
		quote     = uint16('"')
		tab       = uint16('\t')
		space     = uint16(' ')
	)
	isWhitespace := func(w uint16) bool { return w == space || w == tab }

	var retVal []string
	// If the cmd line pointer is null or it points to an empty string then
	// return the name of the executable as argv[0].
	if !present || len(lpCmdLine) == 0 {
		retVal = append(retVal, exeName())
		return retVal
	}
	codeUnits := wstrUnits{units: lpCmdLine}

	// The executable name at the beginning is special.
	inQuotes := false
	var cur []uint16
executableName:
	for {
		w, ok := codeUnits.next()
		if !ok {
			break
		}
		switch {
		// A quote mark always toggles `in_quotes` no matter what because
		// there are no escape characters when parsing the executable name.
		case w == quote:
			inQuotes = !inQuotes
		// If not `in_quotes` then whitespace ends argv[0].
		case isWhitespace(w) && !inQuotes:
			break executableName
		// In all other cases the code unit is taken literally.
		default:
			cur = append(cur, w)
		}
	}
	// Skip whitespace.
	codeUnits.advanceWhile(isWhitespace)
	retVal = append(retVal, rspath.OsStringFromWide(cur))

	// Parse the arguments according to these rules:
	// * All code units are taken literally except space, tab, quote and backslash.
	// * When not `in_quotes`, space and tab separate arguments. Consecutive spaces and tabs are
	// treated as a single separator.
	// * A space or tab `in_quotes` is taken literally.
	// * A quote toggles `in_quotes` mode unless it's escaped. An escaped quote is taken literally.
	// * A quote can be escaped if preceded by an odd number of backslashes.
	// * If any number of backslashes is immediately followed by a quote then the number of
	// backslashes is halved (rounding down).
	// * Backslashes not followed by a quote are all taken literally.
	// * If `in_quotes` then a quote can also be escaped using another quote
	// (i.e. two consecutive quotes become one literal quote).
	cur = nil
	inQuotes = false
arguments:
	for {
		w, ok := codeUnits.next()
		if !ok {
			break
		}
		switch {
		// If not `in_quotes`, a space or tab ends the argument.
		case isWhitespace(w) && !inQuotes:
			retVal = append(retVal, rspath.OsStringFromWide(cur))
			cur = cur[:0]

			// Skip whitespace.
			codeUnits.advanceWhile(isWhitespace)
		// Backslashes can escape quotes or backslashes but only if consecutive backslashes are followed by a quote.
		case w == backslash:
			backslashCount := codeUnits.advanceWhile(func(w uint16) bool { return w == backslash }) + 1
			if next, ok := codeUnits.peek(); ok && next == quote {
				cur = appendRepeated(cur, backslash, backslashCount/2)
				// The quote is escaped if there are an odd number of backslashes.
				if backslashCount%2 == 1 {
					codeUnits.next()
					cur = append(cur, quote)
				}
			} else {
				// If there is no quote on the end then there is no escaping.
				cur = appendRepeated(cur, backslash, backslashCount)
			}
		// If `in_quotes` and not backslash escaped (see above) then a quote either
		// unsets `in_quote` or is escaped by another quote.
		case w == quote && inQuotes:
			next, ok := codeUnits.peek()
			switch {
			// Two consecutive quotes when `in_quotes` produces one literal quote.
			case ok && next == quote:
				cur = append(cur, quote)
				codeUnits.next()
			// Otherwise set `in_quotes`.
			case ok:
				inQuotes = false
			// The end of the command line.
			// Push `cur` even if empty, which we do by breaking while `in_quotes` is still set.
			default:
				break arguments
			}
		// If not `in_quotes` and not BACKSLASH escaped (see above) then a quote sets `in_quote`.
		case w == quote:
			inQuotes = true
		// Everything else is always taken literally.
		default:
			cur = append(cur, w)
		}
	}
	// Push the final argument, if any.
	if len(cur) != 0 || inQuotes {
		retVal = append(retVal, rspath.OsStringFromWide(cur))
	}
	return retVal
}

func appendRepeated(units []uint16, unit uint16, count int) []uint16 {
	for range count {
		units = append(units, unit)
	}
	return units
}
