package rspath

import "github.com/BrokkAi/mjolnir/go/rs"

// A struct field that holds a path carries the option `path` in its `rs` tag
// (`rs:"workspace,path"`); the declarations generator writes it on PathBuf,
// &Path, Option<PathBuf> and Vec<PathBuf> fields. Registering Cmp and Eq for
// it makes rs.Eq, rs.Cmp and rs.PartialCmp compare those fields by
// component, as Rust compares paths, instead of byte by byte. Comparing such
// a field in a program that does not link this package panics.
func init() {
	rs.RegisterTagComparer("path", Cmp, Eq)
}
