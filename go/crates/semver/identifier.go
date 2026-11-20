// Port of semver 1.0.28 src/identifier.rs

package semver

// This module implements Identifier, a short-optimized string allowed to
// contain only the ASCII characters hyphen, dot, 0-9, A-Z, a-z.
//
// In Rust the type is a single pointer-sized word that holds a string of up
// to 8 bytes inline and a longer one on the heap, with its own Clone, Drop,
// PartialEq and allocation layout. None of that representation can be seen
// from outside, and Go strings are already immutable and shared, so here an
// Identifier is its text.
type identifierIdentifier string

func identifierIdentifierEmpty() identifierIdentifier {
	return ""
}

// SAFETY: string must be ASCII and not contain \0 bytes.
func identifierIdentifierNewUnchecked(string_ string) identifierIdentifier {
	return identifierIdentifier(string_)
}

func (self identifierIdentifier) isEmpty() bool {
	return self == ""
}

func (self identifierIdentifier) asStr() string {
	return string(self)
}

// ptrEq is Identifier::ptr_eq: whether the two identifiers are the same
// machine word. For the empty identifier and for one held inline (up to 8
// bytes) that is whether their text is equal. For a longer one Rust compares
// the address of the allocation, which only tells it that the two are
// certainly equal; the comparisons that ask go on to compare the text when
// the answer is no, and equal texts compare equal there too. Comparing the
// text therefore gives every caller the result it would get in Rust.
func (self identifierIdentifier) ptrEq(rhs identifierIdentifier) bool {
	return self == rhs
}
