// Port of aho-corasick 1.1.4 src/nfa/noncontiguous.rs

package ahocorasick

import "github.com/BrokkAi/mjolnir/go/rs"

// The noncontiguous NFA is the crate's first automaton: a trie of the
// patterns with failure transitions, from which the contiguous NFA is
// packed. Only what building a contiguous NFA with the default options
// needs is ported (MatchKind::Standard, no ASCII case insensitivity); the
// prefilter, the dense rows and the byte classes are performance devices
// that do not change which states are reached or which matches they hold.

const (
	// noncontiguousDead is NFA::DEAD: the dead state, fixed to id 0.
	noncontiguousDead AutomatonStateID = 0
	// noncontiguousFail is NFA::FAIL: the fail state, which is never
	// entered and fixed to id 1. A transition to it means "follow the
	// failure transition".
	noncontiguousFail AutomatonStateID = 1
)

// noncontiguousTransition is Transition: one entry of a state's sorted
// list of transitions.
type noncontiguousTransition struct {
	byte byte
	next AutomatonStateID
}

// noncontiguousState is State.
type noncontiguousState struct {
	// trans is the sparse transition list, sorted by byte.
	trans []noncontiguousTransition
	// matches lists the patterns that match in this state, in order.
	matches []PatternID
	fail    AutomatonStateID
	depth   int
}

// noncontiguousNFA is nfa::noncontiguous::NFA.
type noncontiguousNFA struct {
	states            []noncontiguousState
	patternLens       []int
	minPatternLen     int
	maxPatternLen     int
	startUnanchoredId AutomatonStateID
	startAnchoredId   AutomatonStateID
}

func (n *noncontiguousNFA) allocState(depth int) AutomatonStateID {
	id := AutomatonStateID(len(n.states))
	n.states = append(n.states, noncontiguousState{fail: n.startUnanchoredId, depth: depth})
	return id
}

// followTransition is NFA::follow_transition: the state that `byte` leads
// to from `sid`, or FAIL.
func (n *noncontiguousNFA) followTransition(sid AutomatonStateID, b byte) AutomatonStateID {
	for _, t := range n.states[sid].trans {
		if t.byte == b {
			return t.next
		}
		if t.byte > b {
			break
		}
	}
	return noncontiguousFail
}

// addTransition is NFA::add_transition: set the transition on `b` from
// `prev`, keeping the list sorted by byte.
func (n *noncontiguousNFA) addTransition(prev AutomatonStateID, b byte, next AutomatonStateID) {
	trans := n.states[prev].trans
	for i := range trans {
		if trans[i].byte == b {
			trans[i].next = next
			return
		}
		if trans[i].byte > b {
			trans = append(trans, noncontiguousTransition{})
			copy(trans[i+1:], trans[i:])
			trans[i] = noncontiguousTransition{byte: b, next: next}
			n.states[prev].trans = trans
			return
		}
	}
	n.states[prev].trans = append(trans, noncontiguousTransition{byte: b, next: next})
}

// initFullState is NFA::init_full_state: give a state a transition on
// every byte to `next`.
func (n *noncontiguousNFA) initFullState(prev AutomatonStateID, next AutomatonStateID) {
	rs.Assert(len(n.states[prev].trans) == 0, "state must have zero transitions")
	trans := make([]noncontiguousTransition, 256)
	for b := range 256 {
		trans[b] = noncontiguousTransition{byte: byte(b), next: next}
	}
	n.states[prev].trans = trans
}

// addMatch is NFA::add_match: add a pattern at the end of a state's match
// list.
func (n *noncontiguousNFA) addMatch(sid AutomatonStateID, pid PatternID) {
	n.states[sid].matches = append(n.states[sid].matches, pid)
}

// copyMatches is NFA::copy_matches: append the matches of `src` to those of
// `dst`.
func (n *noncontiguousNFA) copyMatches(src AutomatonStateID, dst AutomatonStateID) {
	n.states[dst].matches = append(n.states[dst].matches, n.states[src].matches...)
}

// noncontiguousBuild is `Compiler::new(builder)?.compile(patterns)` for the
// default Builder.
func noncontiguousBuild(patterns [][]byte) (*noncontiguousNFA, error) {
	n := &noncontiguousNFA{minPatternLen: rs.UsizeMax, maxPatternLen: 0}
	// the dead state, only used for leftmost and fixed to id==0
	n.allocState(0)
	// the fail state, which is never entered and fixed to id==1
	n.allocState(0)
	// unanchored start state
	n.startUnanchoredId = n.allocState(0)
	// anchored start state
	n.startAnchoredId = n.allocState(0)
	// Initialize the unanchored starting state in order to make it dense,
	// and thus make transition lookups on this state faster.
	n.initFullState(n.startUnanchoredId, noncontiguousFail)
	n.initFullState(n.startAnchoredId, noncontiguousFail)
	// Set all transitions on the DEAD state to point to itself. This way,
	// the DEAD state can never be escaped. It MUST be used as a sentinel
	// in any correct search.
	n.initFullState(noncontiguousDead, noncontiguousDead)
	// Build the base trie from the given patterns.
	n.buildTrie(patterns)
	// Add transitions (and maybe matches) to the anchored starting state.
	n.setAnchoredStartState()
	// Rewrite transitions to the FAIL state on the unanchored start state
	// as self-transitions. This keeps the start state active at all times.
	n.addUnanchoredStartStateLoop()
	// The meat of the Aho-Corasick algorithm: compute and write failure
	// transitions. i.e., the state to move to when a transition isn't
	// defined in the current state. These are epsilon transitions and thus
	// make this formulation an NFA.
	n.fillFailureTransitions()
	return n, nil
}

func (n *noncontiguousNFA) buildTrie(patterns [][]byte) {
	for i, pat := range patterns {
		pid := PatternID(i)
		n.minPatternLen = min(n.minPatternLen, len(pat))
		n.maxPatternLen = max(n.maxPatternLen, len(pat))
		n.patternLens = append(n.patternLens, len(pat))

		prev := n.startUnanchoredId
		for depth, b := range pat {
			// If the transition from prev using the current byte already
			// exists, then just move through it. Otherwise, add a new
			// state.
			next := n.followTransition(prev, b)
			if next != noncontiguousFail {
				prev = next
			} else {
				next := n.allocState(depth)
				n.addTransition(prev, b, next)
				prev = next
			}
		}
		// Once the pattern has been added, log the match in the final
		// state that it reached.
		n.addMatch(prev, pid)
	}
}

// setAnchoredStartState copies the unanchored start state's transitions
// and matches to the anchored one, whose failure transition is the dead
// state.
func (n *noncontiguousNFA) setAnchoredStartState() {
	uTrans := n.states[n.startUnanchoredId].trans
	aTrans := n.states[n.startAnchoredId].trans
	for i := range uTrans {
		aTrans[i].next = uTrans[i].next
	}
	n.copyMatches(n.startUnanchoredId, n.startAnchoredId)
	// This is the main difference between the unanchored and anchored
	// starting states. If a lookup on an anchored starting state fails,
	// then the search should stop.
	n.states[n.startAnchoredId].fail = noncontiguousDead
}

func (n *noncontiguousNFA) addUnanchoredStartStateLoop() {
	trans := n.states[n.startUnanchoredId].trans
	for i := range trans {
		if trans[i].next == noncontiguousFail {
			trans[i].next = n.startUnanchoredId
		}
	}
}

func (n *noncontiguousNFA) fillFailureTransitions() {
	startUid := n.startUnanchoredId
	// Initialize the queue for breadth first search with all transitions
	// out of the start state. We handle the start state specially because
	// we only want to follow non-self transitions. If we followed self
	// transitions, then this would never terminate.
	var queue []AutomatonStateID
	for _, t := range n.states[startUid].trans {
		// Skip any self-transitions on the start state.
		if t.next == startUid {
			continue
		}
		queue = append(queue, t.next)
	}
	for len(queue) > 0 {
		id := queue[0]
		queue = queue[1:]
		for _, t := range n.states[id].trans {
			queue = append(queue, t.next)

			fail := n.states[id].fail
			for n.followTransition(fail, t.byte) == noncontiguousFail {
				fail = n.states[fail].fail
			}
			fail = n.followTransition(fail, t.byte)
			n.states[t.next].fail = fail
			n.copyMatches(fail, t.next)
		}
		// If the start state is a match state, then this automaton can
		// match the empty string. This implies all states are match states
		// since every position matches the empty string, so copy the
		// matches from the start state to every state.
		n.copyMatches(startUid, id)
	}
}
