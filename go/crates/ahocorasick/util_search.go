// Port of aho-corasick 1.1.4 src/util/search.rs

package ahocorasick

import "github.com/BrokkAi/mjolnir/go/rs"

// Anchored is aho_corasick::Anchored: the type of anchored search to
// perform.
type Anchored uint8

const (
	// Anchored_No: run an unanchored search. This means a match may occur
	// anywhere at or after the start position of the search.
	Anchored_No Anchored = iota
	// Anchored_Yes: run an anchored search. This means that a match must
	// begin at the start position of the search.
	Anchored_Yes
)

var _ = rs.RegisterEnum[Anchored]("Anchored",
	rs.Const(Anchored_No, "No"),
	rs.Const(Anchored_Yes, "Yes"),
)

// IsAnchored is Anchored::is_anchored.
func (a Anchored) IsAnchored() bool {
	return a == Anchored_Yes
}

// MatchKind is aho_corasick::MatchKind: a knob for controlling the match
// semantics of an automaton. Only Standard, the default, is covered.
type MatchKind uint8

const (
	// MatchKind_Standard: use standard match semantics, which support
	// overlapping matches. When used with non-overlapping matches, matches
	// are reported as they are seen.
	MatchKind_Standard MatchKind = iota
)

var _ = rs.RegisterEnum[MatchKind]("MatchKind",
	rs.Const(MatchKind_Standard, "Standard"),
)
