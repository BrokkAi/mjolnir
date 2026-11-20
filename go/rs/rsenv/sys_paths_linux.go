// Port of std 1.99.1 src/sys/paths/unix.rs: current_exe and temp_dir for
// Linux

package rsenv

import (
	"os"

	"github.com/BrokkAi/mjolnir/go/rs/rsio"
)

// sysCurrentExe is sys::paths::current_exe: the target of /proc/self/exe.
//
// It keeps the " (deleted)" suffix Linux gives an executable that has been
// removed; os.Executable takes it off.
func sysCurrentExe() (string, error) {
	path, err := os.Readlink("/proc/self/exe")
	if err != nil {
		failure := rsio.From(err)
		if failure.Kind() == rsio.ErrorKind_NotFound {
			return "", rsio.ErrorFromStaticMessage(rsio.ErrorKind_Uncategorized,
				"no /proc/self/exe available. Is /proc mounted?")
		}
		return "", failure
	}
	return path, nil
}

// sysDefaultTempDir is the temporary directory when TMPDIR is not set.
func sysDefaultTempDir() string {
	return "/tmp"
}
