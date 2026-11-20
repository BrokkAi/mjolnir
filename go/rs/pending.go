package rs

import (
	"cmp"
)

// The bodies below are placeholders so that packages built on rs compile
// while the formatter and the structural operations are being written. Each
// is replaced by its real implementation (fmt_*.go, clone.go, eq.go, cmp.go).

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
