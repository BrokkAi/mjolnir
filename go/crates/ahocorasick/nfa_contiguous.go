// Port of aho-corasick 1.1.4 src/nfa/contiguous.rs

package ahocorasick

// NfaContiguousNFA is aho_corasick::nfa::contiguous::NFA: an NFA whose
// states are packed one after another, built with the default options
// (MatchKind::Standard, no ASCII case insensitivity). It is used through the
// methods of aho_corasick::automaton::Automaton, as Mjolnir uses it.
//
// The crate builds it from a noncontiguous NFA and packs each state into a
// table of u32s; this package keeps the noncontiguous form. The two answer
// every Automaton method the same, except that state IDs are numbered
// differently (see AutomatonStateID).
type NfaContiguousNFA struct {
	nfa *noncontiguousNFA
}

// NfaContiguousNFANew is contiguous::NFA::new: create a new Aho-Corasick
// contiguous NFA using the default configuration.
//
// The crate's errors are for automata with more than 2^31 patterns or
// states, which cannot be built in memory here either.
func NfaContiguousNFANew(patterns [][]byte) (*NfaContiguousNFA, error) {
	nfa, err := noncontiguousBuild(patterns)
	if err != nil {
		return nil, err
	}
	return &NfaContiguousNFA{nfa: nfa}, nil
}

// NfaContiguousNFANewStrings is NfaContiguousNFANew for string patterns:
// `NFA::new(patterns.iter().map(|p| p.as_bytes()))`.
func NfaContiguousNFANewStrings(patterns []string) (*NfaContiguousNFA, error) {
	bytes := make([][]byte, len(patterns))
	for i, p := range patterns {
		bytes[i] = []byte(p)
	}
	return NfaContiguousNFANew(bytes)
}

// StartState is Automaton::start_state: the starting state for an
// unanchored or an anchored search. It never fails for this NFA.
func (n *NfaContiguousNFA) StartState(anchored Anchored) (AutomatonStateID, error) {
	if anchored.IsAnchored() {
		return n.nfa.startAnchoredId, nil
	}
	return n.nfa.startUnanchoredId, nil
}

// NextState is Automaton::next_state: the state that `b` leads to from
// `sid`, following failure transitions. In an anchored search a missing
// transition leads to the dead state.
func (n *NfaContiguousNFA) NextState(anchored Anchored, sid AutomatonStateID, b byte) AutomatonStateID {
	for {
		next := n.nfa.followTransition(sid, b)
		if next != noncontiguousFail {
			return next
		}
		if anchored.IsAnchored() {
			return noncontiguousDead
		}
		sid = n.nfa.states[sid].fail
	}
}

// IsDead is Automaton::is_dead.
func (n *NfaContiguousNFA) IsDead(sid AutomatonStateID) bool {
	return sid == noncontiguousDead
}

// IsMatch is Automaton::is_match: whether the state has matches.
func (n *NfaContiguousNFA) IsMatch(sid AutomatonStateID) bool {
	return !n.IsDead(sid) && len(n.nfa.states[sid].matches) > 0
}

// IsStart is Automaton::is_start.
func (n *NfaContiguousNFA) IsStart(sid AutomatonStateID) bool {
	return sid == n.nfa.startUnanchoredId || sid == n.nfa.startAnchoredId
}

// MatchKind is Automaton::match_kind.
func (n *NfaContiguousNFA) MatchKind() MatchKind {
	return MatchKind_Standard
}

// PatternsLen is Automaton::patterns_len.
func (n *NfaContiguousNFA) PatternsLen() int {
	return len(n.nfa.patternLens)
}

// PatternLen is Automaton::pattern_len.
func (n *NfaContiguousNFA) PatternLen(pid PatternID) int {
	return n.nfa.patternLens[pid]
}

// MinPatternLen is Automaton::min_pattern_len. With no patterns it is
// usize::MAX, as in the crate.
func (n *NfaContiguousNFA) MinPatternLen() int {
	return n.nfa.minPatternLen
}

// MaxPatternLen is Automaton::max_pattern_len.
func (n *NfaContiguousNFA) MaxPatternLen() int {
	return n.nfa.maxPatternLen
}

// MatchLen is Automaton::match_len: how many patterns match in the state.
func (n *NfaContiguousNFA) MatchLen(sid AutomatonStateID) int {
	return len(n.nfa.states[sid].matches)
}

// MatchPattern is Automaton::match_pattern: the pattern of the state's
// match at `index`. A state's own pattern comes first, then those of the
// states its failure transitions lead to, longest first.
func (n *NfaContiguousNFA) MatchPattern(sid AutomatonStateID, index int) PatternID {
	return n.nfa.states[sid].matches[index]
}
