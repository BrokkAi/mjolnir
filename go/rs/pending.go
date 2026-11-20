package rs

import (
	"cmp"
	"fmt"
)

// The bodies below are placeholders so that packages built on rs compile
// while the formatter and the structural operations are being written. Each
// is replaced by its real implementation (fmt_*.go, clone.go, eq.go, cmp.go).

func formatToString(format string, args []any) string {
	if format == "{:?}" && len(args) == 1 {
		return fmt.Sprintf("%#v", args[0])
	}
	if format == "{}" && len(args) == 1 {
		return fmt.Sprint(args[0])
	}
	panic(Todo("core::fmt: Rust format strings (" + format + ")"))
}

func cloneValue[T any](value T) T {
	panic(Todo("core::clone: structural clone"))
}

func equalValues[T any](a, b T) bool {
	if order, ok := compareOrdered(any(a), any(b)); ok {
		return order == 0
	}
	panic(Todo("core::cmp: structural equality"))
}

func compareValues[T any](a, b T) int {
	if order, ok := compareOrdered(any(a), any(b)); ok {
		return order
	}
	panic(Todo("core::cmp: structural ordering"))
}

// compareOrdered orders the built-in ordered types. It stays when the
// structural comparison replaces the placeholders above: it is the fast path.
func compareOrdered(a, b any) (int, bool) {
	switch x := a.(type) {
	case string:
		return cmp.Compare(x, b.(string)), true
	case int:
		return cmp.Compare(x, b.(int)), true
	case int8:
		return cmp.Compare(x, b.(int8)), true
	case int16:
		return cmp.Compare(x, b.(int16)), true
	case int32:
		return cmp.Compare(x, b.(int32)), true
	case int64:
		return cmp.Compare(x, b.(int64)), true
	case uint:
		return cmp.Compare(x, b.(uint)), true
	case uint8:
		return cmp.Compare(x, b.(uint8)), true
	case uint16:
		return cmp.Compare(x, b.(uint16)), true
	case uint32:
		return cmp.Compare(x, b.(uint32)), true
	case uint64:
		return cmp.Compare(x, b.(uint64)), true
	case bool:
		y := b.(bool)
		switch {
		case x == y:
			return 0, true
		case !x:
			return -1, true
		default:
			return 1, true
		}
	}
	return 0, false
}
