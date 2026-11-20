// Package ahocorasick stands in for the Rust crate aho-corasick 1.1.4, as
// far as Mjolnir uses it: mj-transcript builds an
// `aho_corasick::nfa::contiguous::NFA` from identifiers with `NFA::new` and
// steps it a byte at a time through the `aho_corasick::automaton::Automaton`
// trait.
//
// # Names
//
//	nfa::contiguous::NFA::new(patterns)  NfaContiguousNFANew(patterns)
//	automaton::StateID, StateID::ZERO    AutomatonStateID, AutomatonStateIDZero
//	Anchored::No, Anchored::Yes          Anchored_No, Anchored_Yes
//	a.start_state(Anchored::No)          a.StartState(Anchored_No)
//	a.next_state(anchored, sid, byte)    a.NextState(anchored, sid, byte)
//	a.is_match(sid), a.match_len(sid)    a.IsMatch(sid), a.MatchLen(sid)
//	a.match_pattern(sid, i).as_usize()   a.MatchPattern(sid, i).AsUsize()
//
// # Covered
//
// Building the automaton with the default options (MatchKind::Standard, no
// ASCII case insensitivity), and every Automaton method that reads it:
// start_state, next_state, is_dead, is_match, is_start,
// match_kind, patterns_len, pattern_len, min_pattern_len, max_pattern_len,
// match_len, match_pattern. The trie, the failure transitions and the order
// of each state's matches are the crate's (nfa_noncontiguous.go).
//
// # Not covered
//
// The AhoCorasick searcher and its iterators, the leftmost match kinds,
// ASCII case insensitivity, the DFA and the noncontiguous NFA as public
// types, prefilters, and the packed (Teddy) searcher. None of these is used.
// Automaton::is_special is left out with them: its answer depends on whether
// the crate chose a prefilter for the patterns.
//
// State IDs are numbered by this package, not by the crate's packed table:
// only the automaton that returned a state ID can interpret it, as with the
// crate. StateID::ZERO is the dead state here too.
//
// The comparison with the Rust crate is in go/crates/regex's oracle tests,
// whose oracle module (go/port/oracle/src/regex_oracle.rs) holds both
// crates.
package ahocorasick
