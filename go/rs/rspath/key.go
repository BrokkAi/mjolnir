// Port of std 1.99.1 src/path.rs (impl PartialEq, Ord, Hash and Debug for
// PathBuf, as a value of its own)

package rspath

import "github.com/BrokkAi/mjolnir/go/rs"

// Key is a PathBuf where Rust compares, orders or hashes it: the key of a
// HashMap, HashSet, BTreeMap or BTreeSet, or a field of a type that derives
// PartialEq, Ord or Hash.
//
// A Go string compares its bytes. Rust compares paths by component, so
// `a/b`, `a//b`, `a/./b` and `a/b/` are equal, and `a/b` sorts before `a-b`
// (byte order puts it after). Key carries Rust's answers to rs.Eq, rs.Cmp
// and the collections: Equal and Compare are Eq and Cmp, and HashKey is a
// form of the path that two keys share exactly when they are equal. Like a
// PathBuf, it follows the syntax of the platform the program is built for.
//
// Convert with Key(path) and string(key); nothing else changes.
type Key string

// Equal is `impl PartialEq for PathBuf`: the paths have the same components.
func (k Key) Equal(other Key) bool {
	return Eq(string(k), string(other))
}

// Compare is `impl Ord for PathBuf`: the paths ordered component by
// component.
func (k Key) Compare(other Key) int {
	return Cmp(string(k), string(other))
}

// HashKey is the key a Go map or set holding Keys is keyed by: two keys have
// the same HashKey exactly when they are Equal. It stands for what `impl
// Hash for Path` feeds the hasher (the prefix and the components); it is not
// those bytes, and it is not a path.
func (k Key) HashKey() string {
	return HashKey(string(k))
}

// FmtDebug is `impl Debug for PathBuf`: the path written as an OsStr is.
func (k Key) FmtDebug(f *rs.Formatter) error {
	return Debug(string(k)).FmtDebug(f)
}
