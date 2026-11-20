package rs

import "fmt"

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
	panic(Todo("core::cmp: structural equality"))
}

func compareValues[T any](a, b T) int {
	panic(Todo("core::cmp: structural ordering"))
}
