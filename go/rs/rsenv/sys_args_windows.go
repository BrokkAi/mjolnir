// Port of std 1.99.1 src/sys/args/windows.rs (args)

package rsenv

import (
	"unsafe"

	"golang.org/x/sys/windows"
)

// sysArgsNew is sys::args::args: the command line GetCommandLineW gives,
// split as parseLpCmdLine splits it. os.Args splits it by older rules.
func sysArgsNew() sysArgs {
	commandLine := windows.GetCommandLine()
	var units []uint16
	if commandLine != nil {
		for p := unsafe.Pointer(commandLine); *(*uint16)(p) != 0; p = unsafe.Add(p, 2) {
			units = append(units, *(*uint16)(p))
		}
	}
	parsedArgsList := parseLpCmdLine(commandLine != nil, units, func() string {
		exe, err := sysCurrentExe()
		if err != nil {
			return ""
		}
		return exe
	})
	return sysArgs{iter: parsedArgsList}
}
