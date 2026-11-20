// Package assets reads the files both implementations compile in. A Rust
// `include_str!("../assets/x.md")` in mj-core/src/foo.rs is
// assets.MustString("mj-core/assets/x.md") here: the path from the repository
// root of the file the Rust macro names.
package assets

import (
	"io/fs"

	mjolnir "github.com/BrokkAi/mjolnir"
)

// MustString returns an embedded file as a string. The path is checked when
// the program is built in Rust; here a wrong path panics, and the package's
// test reads every path the Rust sources include.
func MustString(path string) string {
	return string(MustBytes(path))
}

// MustBytes returns an embedded file's bytes.
func MustBytes(path string) []byte {
	data, err := fs.ReadFile(mjolnir.Files, path)
	if err != nil {
		panic("assets: " + path + " is not embedded; run python3 go/port/gen_assets.py: " + err.Error())
	}
	return data
}

// Paths returns every embedded path, sorted.
func Paths() []string {
	var paths []string
	err := fs.WalkDir(mjolnir.Files, ".", func(path string, entry fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		if !entry.IsDir() {
			paths = append(paths, path)
		}
		return nil
	})
	if err != nil {
		panic("assets: walk embedded files: " + err.Error())
	}
	return paths
}
