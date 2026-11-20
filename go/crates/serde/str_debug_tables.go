package serde

// escapedInDebug reports whether `char::escape_debug_ext` writes r as a
// `\u{..}` escape: a control, private-use, white-space, grapheme-extending,
// default-ignorable, format-control or unassigned character.
//
// PLACEHOLDER: only the ASCII controls are decided here until the tables of
// the pinned toolchain are generated into this file.
func escapedInDebug(r rune) bool {
	return r < 0x20 || r == 0x7F
}
