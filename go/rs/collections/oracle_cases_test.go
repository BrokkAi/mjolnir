package collections

// The requests of the differential tests against the oracle module
// go/port/oracle/src/std_collections.rs. Each request is a script of
// operations on one collection; oracle_test.go runs it on the port, and the
// oracle on the real std::collections. The Rust reply holds what each step
// returned and the content of the collection after it, so the two are
// compared step by step.
//
// The recorded corpora (testdata/oracle/*.jsonl.gz) are replayed by every
// `go test`:
//
//	MJ_ORACLE=record go/port/gotest.sh ./go/rs/collections/ -run TestDiffAgainstRust
//
// MJ_ORACLE=live asks Rust again, and then also runs oracleLiveScale times as
// many scripts from other seeds, which the corpora are too small to hold.

import (
	"math"
	"math/rand/v2"
	"os"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// oracleLiveScale is how many times as many scripts a live run sends as
// the recorded corpora hold.
const oracleLiveScale = 10

// oracleGen produces the steps of one script. kind is the key or element
// type: "int" or "str" for maps and sets, "i64" or "u8" for deques.
type oracleGen struct {
	r    *rand.Rand
	kind string
	// span is the number of distinct ordinary keys; a small span makes
	// operations meet existing keys often.
	span int
	// dequeLen follows the length of the deque roughly, so that indexes
	// are mostly in range. It does not need to be exact.
	dequeLen int
}

func newOracleGen(seed uint64, kind string) *oracleGen {
	g := &oracleGen{r: rand.New(rand.NewPCG(seed, 0x636f6c6c656374)), kind: kind}
	g.span = []int{4, 12, 40, 200, 3000}[g.r.IntN(5)]
	return g
}

func (g *oracleGen) intn(n int) int { return g.r.IntN(n) }

func oraclePick[T any](g *oracleGen, items ...T) T { return items[g.intn(len(items))] }

// oracleAlphabet holds the characters of the generated string keys: ASCII on
// both sides of 'm' and 'c' (the inverted pair of the range probe), a space,
// two-, three- and four-byte UTF-8, and NUL.
var oracleAlphabet = []string{"a", "b", "c", "m", "z", "A", " ", "é", "ß", "日", "😀", "\x00"}

// item returns a key or element.
func (g *oracleGen) item() any {
	switch g.kind {
	case "int", "i64":
		switch g.intn(25) {
		case 0:
			return oraclePick(g, int64(math.MinInt64), int64(math.MaxInt64), int64(math.MinInt64+1), int64(math.MaxInt64-1), int64(0), int64(-1))
		case 1:
			return int64(g.intn(1<<20)) - 1<<19
		default:
			return int64(g.intn(g.span)) - int64(g.span/4)
		}
	case "u8":
		if g.intn(8) == 0 {
			return oraclePick(g, 0, 1, 254, 255)
		}
		return g.intn(min(g.span, 256))
	default:
		// A small span uses few characters, so that keys repeat and share
		// prefixes.
		letters := oracleAlphabet[:min(len(oracleAlphabet), 2+g.span/40)]
		if g.intn(12) == 0 {
			letters = oracleAlphabet
		}
		length := g.intn(4)
		if g.intn(30) == 0 {
			length = 8 + g.intn(40)
		}
		var text strings.Builder
		for range length {
			text.WriteString(oraclePick(g, letters...))
		}
		return text.String()
	}
}

// less orders two items as Rust orders the key type.
func oracleLess(a, b any) bool {
	switch a := a.(type) {
	case int64:
		return a < b.(int64)
	case int:
		return a < b.(int)
	default:
		return a.(string) < b.(string)
	}
}

func (g *oracleGen) value() int64 { return int64(g.intn(200)) - 100 }

func (g *oracleGen) delta() int64 { return int64(g.intn(21)) - 10 }

// bound is one end of a range: Unbounded, Included or Excluded.
func (g *oracleGen) bound(key any) []any {
	switch g.intn(5) {
	case 0:
		return []any{"u"}
	case 1, 2:
		return []any{"i", key}
	default:
		return []any{"e", key}
	}
}

// bounds returns the two ends of a range: usually in order, sometimes the
// same key on both ends (which Rust refuses when both are excluded), and
// sometimes in any order (which Rust refuses when the start is greater).
func (g *oracleGen) bounds() (lo, hi []any) {
	a, b := g.item(), g.item()
	switch g.intn(10) {
	case 0:
		b = a
	case 1:
	default:
		if oracleLess(b, a) {
			a, b = b, a
		}
	}
	return g.bound(a), g.bound(b)
}

// size picks the size of an argument collection.
func (g *oracleGen) size() int {
	return oraclePick(g, 0, 1, 2, 3, 5, 8, 13, 30, 70)
}

func (g *oracleGen) pairs(count int) []any {
	pairs := make([]any, 0, count)
	for range count {
		pairs = append(pairs, []any{g.item(), g.value()})
	}
	return pairs
}

func (g *oracleGen) items(count int) []any {
	items := make([]any, 0, count)
	for range count {
		items = append(items, g.item())
	}
	return items
}

// sortedDistinct returns count distinct items in order.
func (g *oracleGen) sortedDistinct(count int) []any {
	var items []any
	for attempt := 0; len(items) < count && attempt < count*20; attempt++ {
		candidate := g.item()
		if !slices.ContainsFunc(items, func(existing any) bool { return !oracleLess(existing, candidate) && !oracleLess(candidate, existing) }) {
			items = append(items, candidate)
		}
	}
	slices.SortFunc(items, func(a, b any) int {
		switch {
		case oracleLess(a, b):
			return -1
		case oracleLess(b, a):
			return 1
		default:
			return 0
		}
	})
	return items
}

// related returns a collection related to base: equal, a subset, a
// superset, overlapping, disjoint, or much larger or smaller, which are the
// cases set algebra and comparison branch on. It is never nil, which JSON
// would write as null.
func (g *oracleGen) related(base []any) []any {
	if related := g.relatedOrNil(base); related != nil {
		return related
	}
	return []any{}
}

func (g *oracleGen) relatedOrNil(base []any) []any {
	switch g.intn(8) {
	case 0:
		return slices.Clone(base)
	case 1:
		var subset []any
		for _, element := range base {
			if g.intn(3) != 0 {
				subset = append(subset, element)
			}
		}
		return subset
	case 2:
		return append(slices.Clone(base), g.items(1+g.intn(5))...)
	case 3:
		// Much larger, around the same keys: the paths that search instead
		// of stepping.
		return append(slices.Clone(base), g.items(20*max(1, len(base))+g.intn(20))...)
	case 4:
		// Shares only the smallest or the greatest element.
		sorted := g.sortedDistinct(len(base) + 3)
		if len(base) == 0 || len(sorted) == 0 {
			return sorted
		}
		if g.intn(2) == 0 {
			return append([]any{base[0]}, sorted...)
		}
		return append(sorted, base[len(base)-1])
	case 5:
		if len(base) > 0 {
			return base[:g.intn(len(base))]
		}
		return nil
	default:
		return g.items(g.size())
	}
}

// relatedPairs is related for the pairs of a map, with the values sometimes
// changed.
func (g *oracleGen) relatedPairs(base []any) []any {
	keys := make([]any, 0, len(base))
	for _, pair := range base {
		keys = append(keys, pair.([]any)[0])
	}
	pairs := []any{}
	for _, key := range g.related(keys) {
		value := g.value()
		for _, pair := range base {
			if p := pair.([]any); !oracleLess(p[0], key) && !oracleLess(key, p[0]) && g.intn(4) != 0 {
				value = p[1].(int64)
			}
		}
		pairs = append(pairs, []any{key, value})
	}
	return pairs
}

func (g *oracleGen) modulus() (int64, int64) {
	modulus := int64(2 + g.intn(4))
	return modulus, int64(g.intn(int(modulus)))
}

// mapStep is one random step of a BTreeMap script (ordered) or a HashMap
// script.
func (g *oracleGen) mapStep(ordered bool) []any {
	for {
		switch g.intn(40) {
		case 0, 1, 2, 3, 4, 5:
			return []any{"insert", g.item(), g.value()}
		case 6, 7:
			return []any{oraclePick(g, "remove", "remove_entry"), g.item()}
		case 8, 9:
			return []any{oraclePick(g, "get", "get_key_value", "contains_key"), g.item()}
		case 10:
			return []any{"get_mut_add", g.item(), g.delta()}
		case 11:
			return []any{"index", g.item()}
		case 12:
			return []any{oraclePick(g, "entry_or_insert", "entry_or_insert_with", "entry_insert_entry"), g.item(), g.value()}
		case 13:
			return []any{oraclePick(g, "entry_or_insert_with_key", "entry_or_default", "entry_key"), g.item()}
		case 14:
			return []any{"entry_and_modify", g.item(), g.delta(), g.value()}
		case 15, 16:
			return []any{"entry_match", g.item(), oraclePick(g, "get", "insert", "remove", "remove_entry", "into_mut"), g.value()}
		case 17:
			modulus, rest := g.modulus()
			if g.intn(2) == 0 {
				return []any{"retain", modulus, rest}
			}
			return []any{"retain_mut", modulus, rest, g.delta()}
		case 18:
			return []any{"extend", g.pairs(g.size())}
		case 19:
			if g.intn(3) == 0 {
				return []any{"from_iter", g.pairs(g.size())}
			}
		case 20:
			if g.intn(4) == 0 {
				return []any{oraclePick(g, "clear", "clone")}
			}
		case 21:
			return []any{"len"}
		case 22:
			return []any{"values_mut_add", g.delta()}
		}
		if ordered {
			switch g.intn(24) {
			case 0:
				return []any{oraclePick(g, "first_key_value", "last_key_value")}
			case 1, 2:
				return []any{oraclePick(g, "pop_first", "pop_last")}
			case 3:
				return []any{oraclePick(g, "first_entry_add", "last_entry_add"), g.delta()}
			case 4:
				return []any{oraclePick(g, "first_entry_remove", "last_entry_remove")}
			case 5, 6, 7:
				lo, hi := g.bounds()
				return []any{oraclePick(g, "range", "range_rev"), lo, hi}
			case 8:
				lo, hi := g.bounds()
				return []any{"range_mut_add", lo, hi, g.delta()}
			case 9, 10:
				lo, hi := g.bounds()
				modulus, rest := g.modulus()
				var take any
				if g.intn(3) == 0 {
					take = g.intn(4)
				}
				return []any{"extract_if", lo, hi, modulus, rest, take}
			case 11:
				return []any{"split_off", g.item()}
			case 12:
				return []any{"append", g.pairs(g.size())}
			case 13:
				if g.intn(3) == 0 {
					return []any{"take"}
				}
			case 14:
				return []any{oraclePick(g, "iter_rev", "keys", "keys_rev", "values", "values_rev")}
			case 15:
				return []any{"cmp", g.pairs(g.size())}
			case 16:
				return []any{"probe"}
			}
			continue
		}
		switch g.intn(12) {
		case 0, 1:
			modulus, rest := g.modulus()
			return []any{"extract_if", modulus, rest}
		case 2:
			if g.intn(3) == 0 {
				return []any{"drain"}
			}
		case 3:
			if g.intn(3) == 0 {
				return []any{"with_capacity", g.intn(100)}
			}
		case 4:
			return []any{oraclePick(g, "keys", "values")}
		case 5:
			return []any{"iter_mut_add", g.delta()}
		case 6:
			return []any{"eq", g.pairs(g.size())}
		}
	}
}

// mapComparison is a few steps that build a map from known pairs and
// compare it with related ones, so that equal maps and maps that differ in
// one place are compared too.
func (g *oracleGen) mapComparison(ordered bool) [][]any {
	base := g.pairs(g.size())
	steps := [][]any{{"from_iter", base}}
	for range 1 + g.intn(3) {
		other := g.relatedPairs(base)
		if ordered {
			steps = append(steps, []any{"cmp", other})
		} else {
			steps = append(steps, []any{"eq", other})
		}
	}
	return steps
}

// setStep is one random step of a BTreeSet script (ordered) or a HashSet
// script.
func (g *oracleGen) setStep(ordered bool) []any {
	for {
		switch g.intn(30) {
		case 0, 1, 2, 3, 4, 5:
			return []any{"insert", g.item()}
		case 6:
			return []any{"replace", g.item()}
		case 7, 8:
			return []any{oraclePick(g, "remove", "take"), g.item()}
		case 9, 10:
			return []any{oraclePick(g, "contains", "get"), g.item()}
		case 11:
			modulus, rest := g.modulus()
			return []any{"retain", modulus, rest}
		case 12:
			return []any{"extend", g.items(g.size())}
		case 13:
			if g.intn(3) == 0 {
				return []any{"from_iter", g.items(g.size())}
			}
		case 14:
			if g.intn(4) == 0 {
				return []any{oraclePick(g, "clear", "clone")}
			}
		case 15:
			return []any{"len"}
		case 16, 17:
			return []any{oraclePick(g, "union", "intersection", "difference", "symmetric_difference",
				"is_subset", "is_superset", "is_disjoint", "bitor", "bitand", "bitxor", "sub"), g.items(g.size())}
		}
		if ordered {
			switch g.intn(14) {
			case 0:
				return []any{oraclePick(g, "first", "last")}
			case 1, 2:
				return []any{oraclePick(g, "pop_first", "pop_last")}
			case 3, 4:
				lo, hi := g.bounds()
				return []any{oraclePick(g, "range", "range_rev"), lo, hi}
			case 5, 6:
				lo, hi := g.bounds()
				modulus, rest := g.modulus()
				var take any
				if g.intn(3) == 0 {
					take = g.intn(4)
				}
				return []any{"extract_if", lo, hi, modulus, rest, take}
			case 7:
				return []any{"split_off", g.item()}
			case 8:
				return []any{"append", g.items(g.size())}
			case 9:
				return []any{"iter_rev"}
			case 10:
				return []any{"cmp", g.items(g.size())}
			case 11:
				return []any{"probe"}
			}
			continue
		}
		switch g.intn(6) {
		case 0, 1:
			modulus, rest := g.modulus()
			return []any{"extract_if", modulus, rest}
		case 2:
			if g.intn(3) == 0 {
				return []any{"drain"}
			}
		case 3:
			return []any{"eq", g.items(g.size())}
		}
	}
}

// setAlgebra builds a set from known elements and runs the set operations
// against related sets, whose sizes and overlaps are the ones the
// algorithms of set.rs choose between.
func (g *oracleGen) setAlgebra(ordered bool) [][]any {
	base := g.items(g.size())
	steps := [][]any{{"from_iter", base}}
	operations := []string{"union", "intersection", "difference", "symmetric_difference",
		"is_subset", "is_superset", "is_disjoint", "bitor", "bitand", "bitxor", "sub"}
	if ordered {
		operations = append(operations, "cmp")
	} else {
		operations = append(operations, "eq")
	}
	for range 2 + g.intn(6) {
		steps = append(steps, []any{oraclePick(g, operations...), g.related(base)})
	}
	return steps
}

// index is an index into the deque, mostly in range.
func (g *oracleGen) index() int {
	switch {
	case g.dequeLen > 0 && g.intn(10) < 7:
		return g.intn(g.dequeLen)
	case g.intn(2) == 0:
		return g.dequeLen
	default:
		return g.dequeLen + 1 + g.intn(3)
	}
}

// indexBounds returns the two ends of a range of indexes, usually one that
// fits the deque.
func (g *oracleGen) indexBounds() (lo, hi []any) {
	a, b := g.intn(g.dequeLen+1), g.intn(g.dequeLen+1)
	switch g.intn(10) {
	case 0:
		b = g.dequeLen + 1 + g.intn(2)
	case 1:
	default:
		if b < a {
			a, b = b, a
		}
	}
	bound := func(at int) []any {
		switch g.intn(4) {
		case 0:
			return []any{"u"}
		case 1:
			return []any{"i", at}
		default:
			return []any{"e", at}
		}
	}
	return bound(a), bound(b)
}

// dequeElements returns count elements, in order when sorted is set.
func (g *oracleGen) dequeElements(count int, sorted bool) []any {
	elements := g.items(count)
	if sorted {
		slices.SortFunc(elements, func(a, b any) int {
			switch {
			case oracleLess(a, b):
				return -1
			case oracleLess(b, a):
				return 1
			default:
				return 0
			}
		})
	}
	return elements
}

// dequeStep is one random step of a VecDeque script.
func (g *oracleGen) dequeStep() []any {
	grow := func(by int) { g.dequeLen += by }
	shrink := func(to int) { g.dequeLen = max(0, min(g.dequeLen, to)) }
	for {
		switch g.intn(64) {
		case 0, 1, 2, 3, 4:
			grow(1)
			return []any{oraclePick(g, "push_back", "push_front"), g.item()}
		case 5:
			grow(1)
			return []any{oraclePick(g, "push_back_mut_add", "push_front_mut_add"), g.item(), g.delta()}
		case 6, 7:
			shrink(g.dequeLen - 1)
			return []any{oraclePick(g, "pop_back", "pop_front")}
		case 8:
			return []any{oraclePick(g, "pop_back_if", "pop_front_if"), g.intn(256)}
		case 9, 10:
			at := g.index()
			if at <= g.dequeLen {
				grow(1)
			}
			if g.intn(2) == 0 {
				return []any{"insert", at, g.item()}
			}
			return []any{"insert_mut_add", at, g.item(), g.delta()}
		case 11, 12:
			at := g.index()
			if at < g.dequeLen {
				shrink(g.dequeLen - 1)
			}
			return []any{oraclePick(g, "remove", "swap_remove_back", "swap_remove_front"), at}
		case 13:
			return []any{"swap", g.index(), g.index()}
		case 14:
			at := g.index()
			shrink(at)
			return []any{oraclePick(g, "truncate", "retain_back"), at}
		case 15:
			if g.intn(4) == 0 {
				g.dequeLen = 0
				return []any{"clear"}
			}
		case 16:
			additional := g.intn(40)
			if g.kind == "i64" && g.intn(10) == 0 {
				// More than isize::MAX bytes: Rust panics with "capacity
				// overflow" before it allocates. (A u8 deque cannot be
				// asked for that without first overflowing the length.)
				additional = 1 << 62
			}
			return []any{oraclePick(g, "reserve", "reserve_exact"), additional}
		case 17:
			if g.intn(2) == 0 {
				return []any{"shrink_to_fit"}
			}
			return []any{"shrink_to", g.intn(g.dequeLen + 12)}
		case 18, 19:
			return []any{oraclePick(g, "rotate_left", "rotate_right"), g.intn(g.dequeLen + 2)}
		case 20:
			return []any{"make_contiguous"}
		case 21, 22:
			lo, hi := g.indexBounds()
			g.dequeLen = max(0, g.dequeLen/2)
			return []any{oraclePick(g, "drain", "drain_rev"), lo, hi}
		case 23:
			at := g.index()
			shrink(at)
			return []any{"split_off", at}
		case 24:
			elements := g.items(g.size())
			grow(len(elements))
			return []any{"append", elements, g.intn(len(elements) + 1)}
		case 25, 26:
			elements := g.items(g.size())
			grow(len(elements))
			return []any{"extend", elements}
		case 27:
			size := g.intn(g.dequeLen + 20)
			g.dequeLen = size
			return []any{oraclePick(g, "resize", "resize_with"), size, g.item()}
		case 28:
			modulus, rest := g.modulus()
			g.dequeLen -= g.dequeLen / int(modulus)
			if g.intn(2) == 0 {
				return []any{"retain", modulus, rest}
			}
			return []any{"retain_mut", modulus, rest, g.delta()}
		case 29, 30:
			lo, hi := g.indexBounds()
			return []any{oraclePick(g, "range", "range_rev"), lo, hi}
		case 31:
			lo, hi := g.indexBounds()
			return []any{"range_mut_add", lo, hi, g.delta()}
		case 32, 33:
			return []any{oraclePick(g, "get", "index"), g.index()}
		case 34:
			return []any{oraclePick(g, "get_mut_add", "index_mut_add"), g.index(), g.delta()}
		case 35:
			return []any{oraclePick(g, "front", "back")}
		case 36:
			return []any{oraclePick(g, "front_mut_add", "back_mut_add"), g.delta()}
		case 37:
			return []any{"contains", g.item()}
		case 38:
			return []any{"binary_search", g.item()}
		case 39:
			return []any{"binary_search_by_key", int64(g.intn(g.span+2)) - int64(g.span/8)}
		case 40:
			return []any{"partition_point", int64(g.intn(g.span+2)) - int64(g.span/4)}
		case 41:
			return []any{oraclePick(g, "iter", "iter_rev", "len")}
		case 42:
			return []any{"iter_mut_add", g.delta()}
		case 43:
			return []any{"eq", g.items(g.size())}
		case 44:
			elements := g.items(g.size())
			return []any{oraclePick(g, "eq_deque", "cmp"), elements, g.intn(len(elements) + 1)}
		case 45:
			if g.intn(3) == 0 {
				elements := g.items(g.size())
				g.dequeLen = len(elements)
				return []any{oraclePick(g, "from", "from_iter"), elements}
			}
		case 46:
			if g.intn(4) == 0 {
				g.dequeLen = 0
				if g.intn(2) == 0 {
					return []any{"new"}
				}
				size := g.intn(40)
				if g.kind == "i64" && g.intn(8) == 0 {
					size = 1 << 62
				}
				return []any{"with_capacity", size}
			}
		case 47:
			if g.intn(3) == 0 {
				return []any{"clone"}
			}
		case 48:
			if g.intn(4) == 0 {
				g.dequeLen = 0
				return []any{"into_vec"}
			}
		}
	}
}

// dequeSorted builds a sorted deque whose storage wraps around, and
// searches it: binary_search and partition_point look at the two halves of
// the ring separately.
func (g *oracleGen) dequeSorted() [][]any {
	elements := g.dequeElements(g.size(), true)
	split := g.intn(len(elements) + 1)
	steps := [][]any{{"with_capacity", len(elements) + g.intn(3)}}
	for _, element := range elements[split:] {
		steps = append(steps, []any{"push_back", element})
	}
	for index := split - 1; index >= 0; index-- {
		steps = append(steps, []any{"push_front", elements[index]})
	}
	g.dequeLen = len(elements)
	for range 3 + g.intn(6) {
		probe := g.item()
		if len(elements) > 0 && g.intn(2) == 0 {
			probe = elements[g.intn(len(elements))]
		}
		switch g.intn(4) {
		case 0, 1:
			steps = append(steps, []any{"binary_search", probe})
		case 2:
			steps = append(steps, []any{"binary_search_by_key", int64(g.intn(g.span+2)) - int64(g.span/8)})
		default:
			steps = append(steps, []any{"partition_point", int64(g.intn(g.span+2)) - int64(g.span/4)})
		}
	}
	return steps
}

// dequeComparison builds a deque and compares it with related sequences
// stored with different splits.
func (g *oracleGen) dequeComparison() [][]any {
	elements := g.items(g.size())
	steps := [][]any{{"from", elements}}
	g.dequeLen = len(elements)
	for range 1 + g.intn(4) {
		other := slices.Clone(elements)
		switch g.intn(4) {
		case 0:
			if len(other) > 0 {
				other[g.intn(len(other))] = g.item()
			}
		case 1:
			other = append(other, g.item())
		case 2:
			if len(other) > 0 {
				other = other[:len(other)-1]
			}
		}
		switch g.intn(3) {
		case 0:
			steps = append(steps, []any{"eq", other})
		case 1:
			steps = append(steps, []any{"eq_deque", other, g.intn(len(other) + 1)})
		default:
			steps = append(steps, []any{"cmp", other, g.intn(len(other) + 1)})
		}
		if g.intn(2) == 0 {
			steps = append(steps, []any{oraclePick(g, "rotate_left", "rotate_right"), g.intn(len(elements) + 1)})
		}
	}
	return steps
}

// oracleScript is one request.
type oracleScript struct {
	Op    string  `json:"op"`
	Key   string  `json:"key,omitempty"`
	Elem  string  `json:"elem,omitempty"`
	State string  `json:"state"`
	Steps [][]any `json:"steps"`
}

// script makes one request of the given collection. Short scripts report
// the whole content after each step; long ones a digest of it.
func (g *oracleGen) script(op string) oracleScript {
	length := oraclePick(g, 6, 15, 30, 60, 150)
	state := "digest"
	if length <= 30 && g.span <= 200 {
		state = "full"
	}
	script := oracleScript{Op: op, State: state}
	if op == "vec_deque" {
		script.Elem = g.kind
	} else {
		script.Key = g.kind
	}
	ordered := strings.HasPrefix(op, "btree")
	for len(script.Steps) < length {
		switch op {
		case "btree_map", "hash_map":
			if g.intn(25) == 0 {
				script.Steps = append(script.Steps, g.mapComparison(ordered)...)
			} else {
				script.Steps = append(script.Steps, g.mapStep(ordered))
			}
		case "btree_set", "hash_set":
			if g.intn(20) == 0 {
				script.Steps = append(script.Steps, g.setAlgebra(ordered)...)
			} else {
				script.Steps = append(script.Steps, g.setStep(ordered))
			}
		case "vec_deque":
			switch g.intn(30) {
			case 0:
				script.Steps = append(script.Steps, g.dequeSorted()...)
			case 1:
				script.Steps = append(script.Steps, g.dequeComparison()...)
			default:
				script.Steps = append(script.Steps, g.dequeStep())
			}
		}
	}
	return script
}

// oracleFixedScripts are scripts written by hand for what random ones meet
// too rarely: the bounds a map checks only once it has a root, which an
// emptied map keeps and a new or cleared one lacks; equal keys collected
// from an iterator; extract_if over bounds that make no range, which it does
// not check; the deque's capacity overflow and the panics of its index
// checks.
func oracleFixedScripts(op, kind string) []oracleScript {
	high, absent, low := any(int64(5)), any(int64(4)), any(int64(3))
	if kind == "str" {
		high, absent, low = "m", "d", "c"
	}
	inverted := []any{"range", []any{"i", high}, []any{"e", low}}
	equalExcluded := []any{"range", []any{"e", low}, []any{"e", low}}
	keyed := func(steps ...[]any) oracleScript {
		return oracleScript{Op: op, Key: kind, State: "full", Steps: steps}
	}
	switch op {
	case "btree_map":
		return []oracleScript{
			keyed(inverted, equalExcluded, []any{"probe"},
				[]any{"insert", low, 1}, []any{"remove", low}, inverted, equalExcluded, []any{"probe"},
				[]any{"clone"}, []any{"probe"}, []any{"insert", high, 2}, []any{"pop_first"}, []any{"probe"},
				[]any{"take"}, []any{"probe"}, []any{"from_iter", []any{}}, []any{"probe"},
				[]any{"from_iter", []any{[]any{low, 1}}}, []any{"remove", low}, []any{"probe"},
				[]any{"clear"}, []any{"probe"}, []any{"entry_or_insert", low, 4}, []any{"probe"},
				[]any{"split_off", high}, []any{"remove", low}, []any{"split_off", high}, []any{"probe"},
				[]any{"append", []any{}}, []any{"append", []any{[]any{low, 3}}}, []any{"probe"}),
			keyed([]any{"from_iter", []any{[]any{high, 1}, []any{low, 2}, []any{high, 3}, []any{low, 4}, []any{high, 5}}},
				[]any{"extend", []any{[]any{high, 6}, []any{low, 7}, []any{high, 8}}},
				[]any{"append", []any{[]any{high, 9}, []any{high, 10}}},
				[]any{"extract_if", []any{"u"}, []any{"u"}, 2, 0, 0},
				[]any{"extract_if", []any{"i", high}, []any{"e", low}, 1, 0, nil},
				[]any{"extract_if", []any{"e", low}, []any{"e", low}, 1, 0, nil},
				[]any{"index", high}, []any{"index", absent}, []any{"get_mut_add", absent, 1},
				[]any{"first_entry_remove"}, []any{"last_entry_remove"}, []any{"first_entry_add", 1},
				[]any{"last_entry_remove"}, []any{"first_key_value"}, []any{"last_entry_add", 1}),
		}
	case "btree_set":
		return []oracleScript{
			keyed(inverted, []any{"probe"}, []any{"insert", low}, []any{"pop_last"},
				[]any{"range", []any{"i", high}, []any{"e", low}}, []any{"probe"},
				[]any{"replace", low}, []any{"take", low}, []any{"probe"}, []any{"clear"}, []any{"probe"},
				[]any{"replace", low}, []any{"remove", low}, []any{"probe"}, []any{"split_off", low},
				[]any{"bitor", []any{}}, []any{"bitand", []any{high, low}}, []any{"sub", []any{low}},
				[]any{"from_iter", []any{high, low, high, low}}, []any{"bitxor", []any{low}},
				[]any{"extract_if", []any{"i", high}, []any{"i", low}, 1, 0, nil}, []any{"is_subset", []any{}},
				[]any{"is_superset", []any{}}, []any{"is_disjoint", []any{}}, []any{"cmp", []any{}}),
		}
	case "vec_deque":
		elem := func(steps ...[]any) oracleScript {
			return oracleScript{Op: op, Elem: kind, State: "full", Steps: steps}
		}
		maximum := any(math.MaxInt64)
		return []oracleScript{
			elem([]any{"push_back", 1}, []any{"reserve", maximum}, []any{"reserve_exact", maximum},
				[]any{"extend", []any{2, 3, 4, 5, 6, 7, 8}}, []any{"push_front", 9}, []any{"reserve", maximum},
				[]any{"insert", 10, 1}, []any{"swap", 0, 9}, []any{"swap", 9, 0}, []any{"rotate_left", 10},
				[]any{"rotate_right", 10}, []any{"split_off", 10}, []any{"index", 9}, []any{"index_mut_add", 9, 1},
				[]any{"drain", []any{"i", 3}, []any{"e", 2}}, []any{"drain", []any{"i", 0}, []any{"i", 9}},
				[]any{"range", []any{"e", 9}, []any{"u"}}, []any{"range", []any{"e", 8}, []any{"u"}},
				[]any{"range", []any{"i", 10}, []any{"u"}}, []any{"range", []any{"u"}, []any{"e", 10}},
				[]any{"drain", []any{"i", 2}, []any{"e", 7}}, []any{"shrink_to_fit"}, []any{"into_vec"},
				[]any{"pop_front"}, []any{"pop_back"}, []any{"front"}, []any{"back"}, []any{"rotate_left", 0},
				[]any{"make_contiguous"}, []any{"drain", []any{"u"}, []any{"u"}}, []any{"truncate", 0}),
		}
	}
	return nil
}

// oracleScripts yields the scripts of one corpus: the fixed ones and count
// random ones from each seed, for every key or element type.
func oracleScripts(op string, kinds []string, count int, seed uint64) oracle.Generate {
	return func(yield func(any)) {
		for _, kind := range kinds {
			for _, script := range oracleFixedScripts(op, kind) {
				yield(script)
			}
		}
		scale := 1
		if os.Getenv("MJ_ORACLE") == "live" {
			scale = oracleLiveScale
		}
		for round := range scale {
			for index := range count {
				for _, kind := range kinds {
					g := newOracleGen(seed+uint64(round)<<32+uint64(index), kind)
					yield(g.script(op))
				}
			}
		}
	}
}

// oracleHasherRequests yields the DefaultHasher requests: writes of every
// width with values at the edges, byte strings across the 8-byte blocks
// SipHash works in, and the Hash of fixed values.
func oracleHasherRequests(yield func(any)) {
	yield(map[string]any{"op": "hash_value"})
	g := newOracleGen(0x736970, "str")
	kinds := []string{"bytes", "str", "u8", "u16", "u32", "u64", "usize", "i8", "i16", "i32", "i64", "isize", "length_prefix"}
	for index := range 120 {
		var writes []any
		for range 1 + index%9 + g.intn(4) {
			switch kind := oraclePick(g, kinds...); kind {
			case "bytes":
				length := g.intn(20)
				if g.intn(5) == 0 {
					length = g.intn(100)
				}
				bytes := make([]int, length)
				for at := range bytes {
					bytes[at] = g.intn(256)
				}
				writes = append(writes, []any{kind, bytes})
			case "str":
				writes = append(writes, []any{kind, g.item()})
			case "u64":
				writes = append(writes, []any{kind, oraclePick(g, "0", "1", "18446744073709551615", "9223372036854775808",
					"12345678901234567890", "4294967296")})
			case "u8", "u16", "u32", "usize":
				writes = append(writes, []any{kind, oraclePick(g, 0, 1, 127, 128, 255, 256, 65535, 65536, 4294967295, math.MaxInt64)})
			case "length_prefix":
				writes = append(writes, []any{kind, g.intn(300)})
			default:
				writes = append(writes, []any{kind, oraclePick(g, 0, -1, 1, -128, 127, -32768, 32767, math.MinInt32, math.MaxInt32,
					math.MinInt64, math.MaxInt64, 1<<40)})
			}
		}
		yield(map[string]any{"op": "sip", "writes": writes})
	}
}

func TestDiffAgainstRustBTreeMap(t *testing.T) {
	oracle.Check(t, "std_collections", "btree_map", oracleScripts("btree_map", []string{"int", "str"}, 75, 0x1000), oracleAnswer(t))
}

func TestDiffAgainstRustBTreeSet(t *testing.T) {
	oracle.Check(t, "std_collections", "btree_set", oracleScripts("btree_set", []string{"int", "str"}, 60, 0x2000), oracleAnswer(t))
}

func TestDiffAgainstRustHashMap(t *testing.T) {
	oracle.Check(t, "std_collections", "hash_map", oracleScripts("hash_map", []string{"int", "str"}, 40, 0x3000), oracleAnswer(t))
}

func TestDiffAgainstRustHashSet(t *testing.T) {
	oracle.Check(t, "std_collections", "hash_set", oracleScripts("hash_set", []string{"int", "str"}, 40, 0x4000), oracleAnswer(t))
}

func TestDiffAgainstRustVecDeque(t *testing.T) {
	oracle.Check(t, "std_collections", "vec_deque", oracleScripts("vec_deque", []string{"i64", "u8"}, 80, 0x5000), oracleAnswer(t))
}

func TestDiffAgainstRustDefaultHasher(t *testing.T) {
	oracle.Check(t, "std_collections", "default_hasher", oracleHasherRequests, oracleAnswer(t))
}
