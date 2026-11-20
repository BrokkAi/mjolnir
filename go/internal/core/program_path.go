// Port of mj-core/src/program_path.rs.

// Finding a program on a PATH value the way the operating system would run
// it.

package core

import (
	"context"
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

// defaultPathext is the extensions Windows tries when `PATHEXT` is unset.
const defaultPathext = ".COM;.EXE;.BAT;.CMD"

// namesWithExtensions is the file names to try in each PATH directory, in
// order, for program on a system whose executable extensions are listed in
// pathext (a `;`-separated list such as `.COM;.EXE`). A name that already
// carries an extension is tried as written first, as `CreateProcess` does.
func namesWithExtensions(program, pathext string) []string {
	var names []string
	if filepath.Ext(program) != "" {
		names = append(names, program)
	}
	for _, extension := range strings.Split(pathext, ";") {
		extension = strings.TrimSpace(extension)
		if extension == "" {
			continue
		}
		names = append(names, program+"."+strings.TrimLeft(extension, "."))
	}
	return names
}

func candidateNames(program string) []string {
	if runtime.GOOS != "windows" {
		return []string{program}
	}
	pathext := os.Getenv("PATHEXT")
	if strings.TrimSpace(pathext) == "" {
		pathext = defaultPathext
	}
	return namesWithExtensions(program, pathext)
}

// FindProgramOnPath is the first file named program (with the platform's
// executable extensions) in a directory of path, a PATH value. A missing PATH
// (ok false) finds nothing. On Unix this does not check the execute bit.
func FindProgramOnPath(program string, path string, pathOK bool) (string, bool) {
	if !pathOK {
		return "", false
	}
	names := candidateNames(program)
	for _, directory := range filepath.SplitList(path) {
		for _, name := range names {
			candidate := filepath.Join(directory, name)
			if info, err := os.Stat(candidate); err == nil && info.Mode().IsRegular() {
				return candidate, true
			}
		}
	}
	return "", false
}

// IsExecutableFile reports whether path is a file this process could run: on
// Unix, one with an execute bit set.
func IsExecutableFile(path string) bool {
	info, err := os.Stat(path)
	if err != nil || !info.Mode().IsRegular() {
		return false
	}
	return runtime.GOOS == "windows" || info.Mode().Perm()&0o111 != 0
}

// FindExecutableOnPath is the program a spawn of program with path as its
// PATH would run: like FindProgramOnPath, but skipping files that are not
// executable, as the system's own search does.
func FindExecutableOnPath(program string, path string, pathOK bool) (string, bool) {
	if !pathOK {
		return "", false
	}
	names := candidateNames(program)
	for _, directory := range filepath.SplitList(path) {
		for _, name := range names {
			if candidate := filepath.Join(directory, name); IsExecutableFile(candidate) {
				return candidate, true
			}
		}
	}
	return "", false
}

// FindAntigravityProgram is an ambient Antigravity CLI (`agy`), including
// its conventional install directory when a login shell has not added that
// directory to PATH.
func FindAntigravityProgram(ctx context.Context, environment map[string]string) (string, bool, error) {
	panic("unported: mj-core/src/program_path.rs find_antigravity_program")
}
