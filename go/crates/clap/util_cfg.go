// The build configuration clap's code branches on, and helpers the port
// shares across files.

package clap

import (
	"reflect"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// debugAssertions is `cfg!(debug_assertions)` (and `debug_assert!`): on in
// builds with the tag mj_debug, as Rust's development profile has it.
const debugAssertions = rs.DebugAssertions

// validUtf8 is `to_str().is_some()` on an OsStr: whether the argument is
// valid UTF-8.
func validUtf8(s string) bool {
	return utf8.ValidString(s)
}

// integerLimitsOfKind returns the limits of a defined integer type from its
// underlying kind.
func integerLimitsOfKind(value any) (int64, uint64) {
	t := reflect.TypeOf(value)
	switch t.Kind() {
	case reflect.Int8, reflect.Int16, reflect.Int32, reflect.Int64, reflect.Int:
		bits := t.Bits()
		return -(int64(1) << (bits - 1)), uint64(1)<<(bits-1) - 1
	case reflect.Uint8, reflect.Uint16, reflect.Uint32, reflect.Uint64, reflect.Uint:
		bits := t.Bits()
		if bits == 64 {
			return 0, ^uint64(0)
		}
		return 0, uint64(1)<<bits - 1
	default:
		panic(rs.Unreachable(t.String()))
	}
}

// index is `impl Index<&Id> for Command`: the argument with this id, which
// must exist.
func (c *Command) index(key Id) *Arg {
	arg := c.find(key)
	if arg == nil {
		rs.PanicAt(1, "called `Option::unwrap()` on a `None` value: "+internalErrorMsg)
	}
	return arg
}
