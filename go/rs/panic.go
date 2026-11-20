// Port of std 1.99.1 src/panicking.rs and src/panic.rs (the parts a program
// can observe).

package rs

import (
	"fmt"
	"os"
	"runtime"
	"runtime/debug"
	"sync"
)

// PanicInfo is what a Rust-style panic carries: the message and where it was
// raised. It is the value passed to Go's panic, so a deferred recover sees it.
type PanicInfo struct {
	// Message is the panic message, as `PanicHookInfo::payload` would give it.
	Message string
	// File and Line name the Go source location that raised the panic.
	File string
	Line int
	// Thread is the name of the thread or task that panicked, filled in by
	// whoever runs the hook.
	Thread string
	// Payload is the original value when a Go panic that did not come from
	// this package was caught (a runtime error, for example).
	Payload any

	hookRan bool
}

// Error makes a PanicInfo print its message when the Go runtime reports an
// uncaught panic.
func (p *PanicInfo) Error() string { return p.Message }

func newPanicInfo(skip int, message string) *PanicInfo {
	info := &PanicInfo{Message: message}
	if _, file, line, ok := runtime.Caller(skip + 1); ok {
		info.File = file
		info.Line = line
	}
	return info
}

// PanicAt panics with message, attributing it to the caller skip frames above
// the function that calls PanicAt. Runtime packages use it so the location is
// the user's call, as #[track_caller] arranges in Rust.
func PanicAt(skip int, message string) {
	panic(newPanicInfo(skip+1, message))
}

// Panicf builds the value for `panic!("...", args)`. The caller passes it to
// panic itself, so the Go compiler sees that control does not continue:
//
//	panic(rs.Panicf("unknown state {}", state))
func Panicf(format string, args ...any) *PanicInfo {
	return newPanicInfo(1, Format(format, args...))
}

// Todo builds the panic for code that has not been ported yet. It names the
// Rust item and why it is missing, and go/port/status.py counts every call:
//
//	panic(rs.Todo("mj_core::runtime::spawn"))
func Todo(what string) *PanicInfo {
	return newPanicInfo(1, "not yet ported: "+what)
}

// Unreachable builds the panic for `unreachable!()`, and for the default arm
// of a switch that stands for an exhaustive Rust match. With arguments it is
// `unreachable!("...", args)`.
func Unreachable(formatAndArgs ...any) *PanicInfo {
	message := "internal error: entered unreachable code"
	if len(formatAndArgs) > 0 {
		if format, ok := formatAndArgs[0].(string); ok {
			message += ": " + Format(format, formatAndArgs[1:]...)
		} else {
			message += ": " + DebugString(formatAndArgs[0])
		}
	}
	return newPanicInfo(1, message)
}

// TodoMacro builds the panic for Rust's `todo!()`.
func TodoMacro(formatAndArgs ...any) *PanicInfo {
	message := "not yet implemented"
	if len(formatAndArgs) > 0 {
		message += ": " + Format(formatAndArgs[0].(string), formatAndArgs[1:]...)
	}
	return newPanicInfo(1, message)
}

// Unimplemented builds the panic for Rust's `unimplemented!()`.
func Unimplemented(formatAndArgs ...any) *PanicInfo {
	message := "not implemented"
	if len(formatAndArgs) > 0 {
		message += ": " + Format(formatAndArgs[0].(string), formatAndArgs[1:]...)
	}
	return newPanicInfo(1, message)
}

// Assert is `assert!(cond)` and `assert!(cond, "...", args)` outside tests.
func Assert(cond bool, formatAndArgs ...any) {
	if cond {
		return
	}
	if len(formatAndArgs) == 0 {
		PanicAt(1, "assertion failed")
	}
	PanicAt(1, Format(formatAndArgs[0].(string), formatAndArgs[1:]...))
}

// DebugAssert is `debug_assert!`: Assert when DebugAssertions is on.
func DebugAssert(cond bool, formatAndArgs ...any) {
	if DebugAssertions && !cond {
		if len(formatAndArgs) == 0 {
			PanicAt(1, "assertion failed")
		}
		PanicAt(1, Format(formatAndArgs[0].(string), formatAndArgs[1:]...))
	}
}

// PanicInfoOf turns a value returned by recover into a PanicInfo. A panic
// raised by this package is returned as it is; any other value (a Go runtime
// error, a string) is wrapped with its text as the message.
func PanicInfoOf(recovered any) *PanicInfo {
	switch value := recovered.(type) {
	case *PanicInfo:
		return value
	case error:
		return &PanicInfo{Message: value.Error(), Payload: recovered}
	case string:
		return &PanicInfo{Message: value, Payload: recovered}
	default:
		return &PanicInfo{Message: fmt.Sprint(recovered), Payload: recovered}
	}
}

var (
	hookMu     sync.Mutex
	panicHook  func(*PanicInfo)
	firstPanic = true
)

// SetPanicHook is std::panic::set_hook.
func SetPanicHook(hook func(*PanicInfo)) {
	hookMu.Lock()
	defer hookMu.Unlock()
	panicHook = hook
}

// TakePanicHook is std::panic::take_hook: it returns the current hook and
// puts the default one back.
func TakePanicHook() func(*PanicInfo) {
	hookMu.Lock()
	defer hookMu.Unlock()
	hook := panicHook
	panicHook = nil
	if hook == nil {
		return defaultPanicHook
	}
	return hook
}

// panicBacktrace is std::panic::BacktraceStyle.
type panicBacktrace uint8

const (
	panicBacktraceShort panicBacktrace = iota
	panicBacktraceFull
	panicBacktraceOff
)

// panicBacktraceStyle is panic::get_backtrace_style: RUST_BACKTRACE, read
// at the first panic and kept, as Rust keeps it.
var panicBacktraceStyle = sync.OnceValue(func() panicBacktrace {
	value, set := os.LookupEnv("RUST_BACKTRACE")
	switch {
	case set && value == "0":
		return panicBacktraceOff
	case set && value == "full":
		return panicBacktraceFull
	case set:
		return panicBacktraceShort
	default:
		return panicBacktraceOff
	}
})

// hookOutputMu is backtrace::lock(), which the default hook holds while it
// writes, so that the reports of two panics do not interleave.
var hookOutputMu sync.Mutex

// defaultPanicHook is std::panicking::default_hook.
//
// Rust prints a full backtrace for a panic that happens while another is
// unwinding the same thread (a double panic); this port does not count the
// panics in progress, so the style is always the one RUST_BACKTRACE asks for.
// Where Rust prints the frames of the panicking thread, the port prints the
// panicking goroutine's Go stack, and the location is the Go file and line
// (Go keeps no column).
func defaultPanicHook(info *PanicInfo) {
	backtrace := panicBacktraceStyle()

	thread := info.Thread
	if thread == "" {
		thread = "<unnamed>"
	}

	hookOutputMu.Lock()
	defer hookOutputMu.Unlock()

	// We add a newline to ensure the panic message appears at the start of a line.
	fmt.Fprintf(os.Stderr, "\nthread '%s' (%d) panicked at %s:%d:\n%s\n", thread, currentOSThreadID(), info.File, info.Line, info.Message)

	switch backtrace {
	case panicBacktraceShort, panicBacktraceFull:
		os.Stderr.Write(append([]byte("stack backtrace:\n"), debug.Stack()...))
		if backtrace == panicBacktraceShort {
			fmt.Fprintln(os.Stderr, "note: Some details are omitted, run with `RUST_BACKTRACE=full` for a verbose backtrace.")
		}
	default:
		hookMu.Lock()
		first := firstPanic
		firstPanic = false
		hookMu.Unlock()
		if first {
			fmt.Fprintln(os.Stderr, "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace")
		}
	}
}

// RunPanicHook runs the panic hook for info once, naming the thread or task
// it happened on. The functions that start threads and tasks call it when a
// panic reaches them; Rust runs the hook at the moment of the panic, before
// unwinding, so here it runs after the deferred calls instead.
func RunPanicHook(info *PanicInfo, thread string) {
	if info.hookRan {
		return
	}
	info.hookRan = true
	if info.Thread == "" {
		info.Thread = thread
	}
	hookMu.Lock()
	hook := panicHook
	hookMu.Unlock()
	if hook == nil {
		hook = defaultPanicHook
	}
	hook(info)
}

// CatchUnwind is std::panic::catch_unwind: it runs f and returns its result,
// or the panic that stopped it. The panic hook has run by the time it
// returns, as in Rust. A goroutine that is being cancelled is not a panic and
// is not caught.
func CatchUnwind[T any](f func() T) (result T, caught *PanicInfo) {
	defer func() {
		if recovered := recover(); recovered != nil {
			caught = PanicInfoOf(recovered)
			RunPanicHook(caught, "")
		}
	}()
	return f(), nil
}

// ResumeUnwind is std::panic::resume_unwind: it panics again with a caught
// panic without running the hook a second time.
func ResumeUnwind(info *PanicInfo) {
	info.hookRan = true
	panic(info)
}

// Main runs a program's main function the way the Rust runtime runs `fn
// main`: a panic that reaches it runs the hook on thread 'main' and ends the
// process with status 101.
func Main(run func()) {
	defer func() {
		if recovered := recover(); recovered != nil {
			RunPanicHook(PanicInfoOf(recovered), "main")
			os.Exit(101)
		}
	}()
	run()
}
