// Port of aho-corasick 1.1.4 src/util/primitives.rs

package ahocorasick

import "strconv"

// AutomatonStateID is aho_corasick::automaton::StateID (util::primitives::
// StateID): the identifier of a state in an automaton.
//
// The numbers are this package's own: the crate's contiguous NFA numbers a
// state by its offset in a packed table, which nothing outside the crate
// can rely on. A state ID is only meaningful to the automaton that gave it.
type AutomatonStateID uint32

// AutomatonStateIDZero is StateID::ZERO, the state ID with value 0. In the
// NFAs of this package it is the dead state.
const AutomatonStateIDZero AutomatonStateID = 0

// AsUsize is StateID::as_usize.
func (sid AutomatonStateID) AsUsize() int { return int(sid) }

// PatternID is aho_corasick::PatternID: the identifier of a pattern, its
// position in the list the automaton was built from.
type PatternID uint32

// AsUsize is PatternID::as_usize.
func (pid PatternID) AsUsize() int { return int(pid) }

// String is the Debug form, `PatternID(3)`.
func (pid PatternID) String() string {
	return "PatternID(" + strconv.Itoa(int(pid)) + ")"
}
