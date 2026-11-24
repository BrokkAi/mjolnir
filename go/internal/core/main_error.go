// The Go equivalent of the report a Rust binary prints for the error its
// `main` returns.
//
// Mjolnir's Rust binaries return `anyhow::Result` from `main`, so a failure
// prints `Error: ` and anyhow's Debug form of the error: the outermost
// message, then each cause under "Caused by:". The Go binaries (`mj` and
// `mj-worker`) print the same text through MainErrorReport.

package core

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"reflect"
	"strings"
	"syscall"
)

// MainErrorReport is what Rust prints to stderr for the error `main`
// returns: `Error: ` and AnyhowDebug, ending with a newline.
func MainErrorReport(err error) string {
	return "Error: " + AnyhowDebug(err) + "\n"
}

// AnyhowDebug is anyhow's Debug form of err (`{error:?}`): its outermost
// message, then each cause under "Caused by:", numbered when there is more
// than one, with a cause's later lines indented under its first.
func AnyhowDebug(err error) string {
	chain := ErrorChain(err)
	text := chain[0]
	causes := chain[1:]
	if len(causes) == 0 {
		return text
	}
	text += "\n\nCaused by:"
	for index, cause := range causes {
		prefix, indent := "    ", "    "
		if len(causes) > 1 {
			prefix, indent = fmt.Sprintf("%5d: ", index), "       "
		}
		text += "\n" + prefix + strings.ReplaceAll(cause, "\n", "\n"+indent)
	}
	return text
}

// ErrorChain is err's messages from the outermost to its root cause, each
// without the causes it wraps, as anyhow's chain holds them: a layer
// `context: cause` made with %w is "context" followed by the chain of cause.
// A layer whose text does not end with its cause's is kept whole and ends
// the chain, and so is an error of Go's standard library. A Go file or
// system call error reads as Rust's io::Error does (rustIOErrorText).
func ErrorChain(err error) []string {
	var messages []string
	for err != nil {
		next := errors.Unwrap(err)
		if next == nil || !isContextLayer(err) {
			return append(messages, rustIOErrorText(err))
		}
		own, ok := strings.CutSuffix(err.Error(), ": "+next.Error())
		if !ok {
			return append(messages, err.Error())
		}
		messages = append(messages, own)
		err = next
	}
	return messages
}

// isContextLayer reports whether err adds context over the error it wraps:
// fmt's %w wrapper, or a Mjolnir error type. Standard library wrappers such
// as *fs.PathError are one error whose text names their cause.
func isContextLayer(err error) bool {
	kind := reflect.TypeOf(err)
	for kind.Kind() == reflect.Pointer {
		kind = kind.Elem()
	}
	path := kind.PkgPath()
	if path == "fmt" {
		return true
	}
	first, _, _ := strings.Cut(path, "/")
	return strings.Contains(first, ".")
}

// rustIOErrorText is the text of one error in the chain. A Go file or system
// call error is where Rust has an io::Error, whose text is only the system's
// description and number (`No such file or directory (os error 2)`): Go's
// adds the operation and path, which Rust leaves to the context around it.
func rustIOErrorText(err error) string {
	switch err.(type) {
	case *fs.PathError, *os.LinkError, *os.SyscallError, syscall.Errno:
		return ioErrorDisplay(err)
	}
	return err.Error()
}
