// Port of mj-core/src/daemon_exit.rs: unix_signal_name.

//go:build unix

package core

import "syscall"

// UnixSignalName names the signals a daemon's waiter reports.
func UnixSignalName(signal int32) (string, bool) {
	names := map[syscall.Signal]string{
		syscall.SIGHUP:  "SIGHUP",
		syscall.SIGINT:  "SIGINT",
		syscall.SIGQUIT: "SIGQUIT",
		syscall.SIGILL:  "SIGILL",
		syscall.SIGTRAP: "SIGTRAP",
		syscall.SIGABRT: "SIGABRT",
		syscall.SIGBUS:  "SIGBUS",
		syscall.SIGFPE:  "SIGFPE",
		syscall.SIGKILL: "SIGKILL",
		syscall.SIGUSR1: "SIGUSR1",
		syscall.SIGSEGV: "SIGSEGV",
		syscall.SIGUSR2: "SIGUSR2",
		syscall.SIGPIPE: "SIGPIPE",
		syscall.SIGALRM: "SIGALRM",
		syscall.SIGTERM: "SIGTERM",
		syscall.SIGXCPU: "SIGXCPU",
		syscall.SIGXFSZ: "SIGXFSZ",
		syscall.SIGSYS:  "SIGSYS",
	}
	name, ok := names[syscall.Signal(signal)]
	return name, ok
}
