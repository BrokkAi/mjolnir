package anyhow

import (
	"fmt"
	"reflect"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/strs"
)

// displayString is `value.to_string()` for a value this package was given as
// a message, a context or an error: its Display output with the default
// flags.
//
// A string is itself. An error's Error method and a Display type's String
// method hold that text by the port's convention, so the three plain forms of
// an anyhow error are built without the formatter. Any other value (a
// number, a char) goes through rs.ToString.
func displayString(value any) string {
	switch value := value.(type) {
	case string:
		return value
	case error:
		return value.Error()
	case fmt.Stringer:
		return value.String()
	}
	return rs.ToString(value)
}

// escapeDebug is `s.escape_debug().to_string()`.
func escapeDebug(s string) string {
	return strs.EscapeDebug(s)
}

// asValue is the step of errors.As for one value held by an error of this
// package: it stores value in *target when value is an error of that type.
// It does not follow value's own sources; the errors package does that
// through Unwrap where they are part of the chain.
func asValue(value any, target any) bool {
	if _, isError := value.(error); !isError {
		return false
	}
	if custom, ok := value.(interface{ As(any) bool }); ok && custom.As(target) {
		return true
	}
	pointer := reflect.ValueOf(target)
	if pointer.Kind() != reflect.Pointer || pointer.IsNil() {
		return false
	}
	stored := reflect.ValueOf(value)
	if !stored.Type().AssignableTo(pointer.Type().Elem()) {
		return false
	}
	pointer.Elem().Set(stored)
	return true
}

// isValue is the step of errors.Is for one value held by an error of this
// package.
func isValue(value any, target error) bool {
	held, isError := value.(error)
	if !isError {
		return false
	}
	if reflect.TypeOf(held).Comparable() && reflect.TypeOf(target).Comparable() && held == target {
		return true
	}
	custom, ok := held.(interface{ Is(error) bool })
	return ok && custom.Is(target)
}
