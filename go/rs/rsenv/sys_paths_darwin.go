// Port of std 1.99.1 src/sys/paths/unix.rs: current_exe and darwin_temp_dir
// for macOS

package rsenv

import (
	"os"
	"os/exec"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// sysCurrentExe is sys::paths::current_exe: the path _NSGetExecutablePath
// gives, which is the path the kernel was asked to run, made absolute from
// the directory the program started in.
//
// os.Executable gives that path too, from the same kernel argument, with one
// difference: a relative path that began with "./" loses the "./" where
// Rust's keeps it ("/start/./prog" against "/start/prog"). Both name the
// same file.
func sysCurrentExe() (string, error) {
	path, err := os.Executable()
	if err != nil {
		return "", rsio.From(err)
	}
	return path, nil
}

// sysDefaultTempDir is darwin_temp_dir: the per-user temporary directory
// that confstr(_CS_DARWIN_USER_TEMP_DIR) names, or /tmp when that fails "for
// whatever reason (there are several possible reasons)".
//
// confstr is in the C library, which a Go program without C cannot call;
// getconf(1) asks it the same question and prints the answer. Rust asks on
// every call, and confstr creates the directory when it is missing, so the
// answer is not kept.
func sysDefaultTempDir() string {
	output, err := exec.Command("/usr/bin/getconf", "DARWIN_USER_TEMP_DIR").Output()
	if err != nil {
		// It failed for whatever reason (there are several possible reasons),
		// so return the global one.
		return "/tmp"
	}
	return strings.TrimSuffix(string(output), "\n")
}
