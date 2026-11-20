package rs_test

import (
	"cmp"
	"encoding/binary"
	"encoding/json"
	"os"
	"strconv"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/rstest/oracle"
)

// The tests in this file hold the sorts and searches (sort*.go) against
// Rust's, through the oracle module go/port/oracle/src/std_sort.rs. A
// request names a pattern, a length, a number of distinct keys and a seed;
// both sides build the same (key, tag) elements from them, and the reply
// carries the tags in the order the sort left them.

type sortOracleRequest struct {
	Op       string   `json:"op"`
	F        string   `json:"f,omitempty"`
	Ty       string   `json:"ty,omitempty"`
	Pattern  string   `json:"pattern,omitempty"`
	Len      int      `json:"len"`
	Distinct uint64   `json:"distinct,omitempty"`
	Seed     uint64   `json:"seed"`
	Probes   []uint64 `json:"probes,omitempty"`
}

// sortOracleRng is splitmix64, the generator both sides use.
type sortOracleRng uint64

func (r *sortOracleRng) next() uint64 {
	*r += 0x9E3779B97F4A7C15
	z := uint64(*r)
	z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9
	z = (z ^ (z >> 27)) * 0x94D049BB133111EB
	return z ^ (z >> 31)
}

// sortOracleKeys builds the keys of a request as `keys` in std_sort.rs does.
func sortOracleKeys(request sortOracleRequest) []uint64 {
	distinct := max(request.Distinct, 1)
	rng := sortOracleRng(request.Seed)
	n := uint64(request.Len)
	keys := make([]uint64, n)
	for i := range n {
		switch request.Pattern {
		case "random":
			keys[i] = rng.next() % distinct
		case "ascending":
			keys[i] = i * distinct / n
		case "descending":
			keys[i] = (n - 1 - i) * distinct / n
		case "sawtooth":
			keys[i] = i % distinct
		case "equal":
			keys[i] = 0
		case "pipe":
			keys[i] = min(i, n-1-i) * distinct / n
		case "push_front":
			if i == n-1 {
				keys[i] = 0
			} else {
				keys[i] = i*distinct/n + 1
			}
		default:
			panic("unknown pattern " + request.Pattern)
		}
	}
	return keys
}

func sortOracleHash(tags []int) string {
	h := uint64(0xcbf29ce484222325)
	var bytes [8]byte
	for _, tag := range tags {
		binary.LittleEndian.PutUint64(bytes[:], uint64(tag))
		for _, b := range bytes {
			h ^= uint64(b)
			h *= 0x00000100000001b3
		}
	}
	return strconv.FormatUint(h, 10)
}

func sortOracleReply(tags []int, calls int, counted bool) map[string]any {
	reply := map[string]any{"hash": sortOracleHash(tags)}
	if len(tags) <= 64 {
		reply["tags"] = tags
	}
	if counted {
		reply["calls"] = calls
	}
	return reply
}

// sortOracleElement is how a test reads one of the element types.
type sortOracleElement[E any] struct {
	make func(key uint64, tag int) E
	key  func(E) uint64
	tag  func(E) int
}

var (
	sortOracleU32x2 = sortOracleElement[rs.Tuple2[uint32, uint32]]{
		make: func(key uint64, tag int) rs.Tuple2[uint32, uint32] { return rs.T2(uint32(key), uint32(tag)) },
		key:  func(e rs.Tuple2[uint32, uint32]) uint64 { return uint64(e.F0) },
		tag:  func(e rs.Tuple2[uint32, uint32]) int { return int(e.F1) },
	}
	sortOracleU64x2 = sortOracleElement[rs.Tuple2[uint64, uint64]]{
		make: func(key uint64, tag int) rs.Tuple2[uint64, uint64] { return rs.T2(key, uint64(tag)) },
		key:  func(e rs.Tuple2[uint64, uint64]) uint64 { return e.F0 },
		tag:  func(e rs.Tuple2[uint64, uint64]) int { return int(e.F1) },
	}
	sortOracleU64String = sortOracleElement[rs.Tuple2[uint64, string]]{
		make: func(key uint64, tag int) rs.Tuple2[uint64, string] { return rs.T2(key, strconv.Itoa(tag)) },
		key:  func(e rs.Tuple2[uint64, string]) uint64 { return e.F0 },
		tag: func(e rs.Tuple2[uint64, string]) int {
			tag, err := strconv.Atoi(e.F1)
			if err != nil {
				panic(err)
			}
			return tag
		},
	}
	sortOracleBig = sortOracleElement[rs.Tuple2[uint64, [11]uint64]]{
		make: func(key uint64, tag int) rs.Tuple2[uint64, [11]uint64] {
			return rs.T2(key, [11]uint64{uint64(tag)})
		},
		key: func(e rs.Tuple2[uint64, [11]uint64]) uint64 { return e.F0 },
		tag: func(e rs.Tuple2[uint64, [11]uint64]) int { return int(e.F1[0]) },
	}
	sortOracleHuge = sortOracleElement[rs.Tuple2[uint64, [12]uint64]]{
		make: func(key uint64, tag int) rs.Tuple2[uint64, [12]uint64] {
			return rs.T2(key, [12]uint64{uint64(tag)})
		},
		key: func(e rs.Tuple2[uint64, [12]uint64]) uint64 { return e.F0 },
		tag: func(e rs.Tuple2[uint64, [12]uint64]) int { return int(e.F1[0]) },
	}
	sortOracleCell = sortOracleElement[rs.Tuple2[uint32, rs.Cell[uint32]]]{
		make: func(key uint64, tag int) rs.Tuple2[uint32, rs.Cell[uint32]] {
			return rs.T2(uint32(key), rs.CellNew(uint32(tag)))
		},
		key: func(e rs.Tuple2[uint32, rs.Cell[uint32]]) uint64 { return uint64(e.F0) },
		tag: func(e rs.Tuple2[uint32, rs.Cell[uint32]]) int { return int(e.F1.Get()) },
	}
)

func sortOracleSort[E any](request sortOracleRequest, element sortOracleElement[E]) any {
	keys := sortOracleKeys(request)
	v := make([]E, len(keys))
	for tag, key := range keys {
		v[tag] = element.make(key, tag)
	}
	calls := 0
	counted := false
	switch request.F {
	case "unstable":
		rs.SortUnstableBy(v, func(a, b E) rs.Ordering {
			calls++
			return rs.OrderingOf(cmp.Compare(element.key(a), element.key(b)))
		})
		counted = true
	case "stable":
		rs.SortStableBy(v, func(a, b E) int { return cmp.Compare(element.key(a), element.key(b)) })
	case "cached":
		rs.SortStableByCachedKey(v, element.key)
	default:
		return map[string]string{"oracle_error": "unknown sort " + request.F}
	}
	tags := make([]int, len(v))
	for i, e := range v {
		tags[i] = element.tag(e)
	}
	return sortOracleReply(tags, calls, counted)
}

func sortOracleRandomCmp[E any](request sortOracleRequest, element sortOracleElement[E]) any {
	return oracle.Catch(func() any {
		v := make([]E, request.Len)
		for tag := range v {
			v[tag] = element.make(0, tag)
		}
		rng := sortOracleRng(request.Seed)
		calls := 0
		rs.SortUnstableBy(v, func(_, _ E) rs.Ordering {
			calls++
			switch rng.next() % 3 {
			case 0:
				return rs.Ordering_Less
			case 1:
				return rs.Ordering_Equal
			}
			return rs.Ordering_Greater
		})
		tags := make([]int, len(v))
		for i, e := range v {
			tags[i] = element.tag(e)
		}
		return sortOracleReply(tags, calls, true)
	})
}

func sortOracleByType(request sortOracleRequest, random bool) any {
	switch request.Ty {
	case "u32x2":
		if random {
			return sortOracleRandomCmp(request, sortOracleU32x2)
		}
		return sortOracleSort(request, sortOracleU32x2)
	case "u64x2":
		if random {
			return sortOracleRandomCmp(request, sortOracleU64x2)
		}
		return sortOracleSort(request, sortOracleU64x2)
	case "u64_string":
		if random {
			return sortOracleRandomCmp(request, sortOracleU64String)
		}
		return sortOracleSort(request, sortOracleU64String)
	case "big":
		if random {
			return sortOracleRandomCmp(request, sortOracleBig)
		}
		return sortOracleSort(request, sortOracleBig)
	case "huge":
		if random {
			return sortOracleRandomCmp(request, sortOracleHuge)
		}
		return sortOracleSort(request, sortOracleHuge)
	case "cell":
		if random {
			return sortOracleRandomCmp(request, sortOracleCell)
		}
		return sortOracleSort(request, sortOracleCell)
	}
	return map[string]string{"oracle_error": "unknown type " + request.Ty}
}

func sortOracleSearch(request sortOracleRequest) any {
	request.Pattern = "ascending"
	keys := sortOracleKeys(request)
	wrapped := make([]rs.Tuple2[uint64, int], len(keys))
	for tag, key := range keys {
		wrapped[tag] = rs.T2(key, tag)
	}
	results := []any{}
	visits := [][]int{}
	for _, x := range request.Probes {
		visited := []int{}
		switch request.F {
		case "binary_search":
			// Probe positions are recovered from the element's index in a
			// parallel slice of tags, since the search passes values.
			index, found := rs.BinarySearchBy(wrapped, func(e rs.Tuple2[uint64, int]) int {
				visited = append(visited, e.F1)
				return cmp.Compare(e.F0, x)
			})
			plainIndex, plainFound := rs.BinarySearch(keys, x)
			if plainIndex != index || plainFound != found {
				return map[string]string{"oracle_error": "BinarySearch and BinarySearchBy disagree"}
			}
			results = append(results, []any{found, index})
		case "binary_search_by_key":
			index, found := rs.BinarySearchByKey(wrapped, x, func(e rs.Tuple2[uint64, int]) uint64 {
				visited = append(visited, e.F1)
				return e.F0
			})
			results = append(results, []any{found, index})
		case "partition_point":
			results = append(results, rs.PartitionPoint(wrapped, func(e rs.Tuple2[uint64, int]) bool {
				visited = append(visited, e.F1)
				return e.F0 < x
			}))
		default:
			return map[string]string{"oracle_error": "unknown search " + request.F}
		}
		visits = append(visits, visited)
	}
	return map[string]any{"results": results, "visits": visits}
}

var sortOracleTypes = []string{"u32x2", "u64x2", "u64_string", "big", "huge", "cell"}

var sortOraclePatterns = []string{"random", "ascending", "descending", "sawtooth", "equal", "pipe", "push_front"}

// sortOracleLengths cross the thresholds the sort branches on: insertion
// sort up to 20, the small-sort thresholds 16 and 32, the networks' 9, 13
// and 18, the pivot's recursion at 64 and its sampling at 8.
var sortOracleLengths = []int{
	0, 1, 2, 3, 4, 5, 7, 8, 9, 12, 13, 16, 17, 18, 19, 20, 21, 22, 24, 25, 31, 32, 33, 34,
	35, 40, 47, 48, 49, 50, 63, 64, 65, 66, 100, 127, 128, 129, 200, 255, 256, 257, 400,
	500, 1000, 1023, 1024, 2000, 4096, 5000,
}

func sortOracleGenerate(full bool) oracle.Generate {
	return func(yield func(any)) {
		seed := uint64(1)
		for _, ty := range sortOracleTypes {
			for _, pattern := range sortOraclePatterns {
				for _, n := range sortOracleLengths {
					distincts := []uint64{max(uint64(n)/4, 2)}
					if pattern == "random" {
						distincts = []uint64{2, 3, max(uint64(n)/4, 2), uint64(n) + 1}
					}
					if pattern == "equal" && n > 300 && !full {
						continue
					}
					for _, distinct := range distincts {
						seed++
						yield(sortOracleRequest{Op: "sort", F: "unstable", Ty: ty, Pattern: pattern, Len: n, Distinct: distinct, Seed: seed})
					}
				}
			}
		}
		// The stable sorts leave one order whatever the algorithm; a few
		// cases show the Go ones are stable.
		for _, f := range []string{"stable", "cached"} {
			for _, pattern := range sortOraclePatterns {
				for _, n := range []int{0, 1, 2, 5, 20, 21, 33, 64, 100, 1000} {
					seed++
					yield(sortOracleRequest{Op: "sort", F: f, Ty: "u64_string", Pattern: pattern, Len: n, Distinct: max(uint64(n)/5, 2), Seed: seed})
				}
			}
		}
		// A comparison that answers at random makes the result depend on
		// every step, and runs into the check for a broken order.
		for _, ty := range sortOracleTypes {
			for _, n := range []int{2, 3, 8, 20, 21, 22, 25, 30, 32, 33, 40, 48, 64, 65, 100, 200, 500, 1000} {
				for range 2 {
					seed++
					yield(sortOracleRequest{Op: "sort_random_cmp", Ty: ty, Len: n, Seed: seed})
				}
			}
		}
		// Searches over ascending keys with ties: every key, and the ones
		// either side.
		rng := sortOracleRng(99)
		for _, f := range []string{"binary_search", "binary_search_by_key", "partition_point"} {
			for _, n := range []int{0, 1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 100, 1000, 5000} {
				for _, distinct := range []uint64{1, 2, 3, 10, 100} {
					probes := []uint64{}
					for x := range distinct + 2 {
						if distinct <= 10 || rng.next()%5 == 0 {
							probes = append(probes, x)
						}
					}
					seed++
					yield(sortOracleRequest{Op: "search", F: f, Len: n, Distinct: distinct, Seed: seed, Probes: probes})
				}
			}
		}
	}
}

func TestSortAgainstRust(t *testing.T) {
	// The recorded corpus leaves out the longest all-equal inputs; a live run
	// sends them too.
	full := os.Getenv("MJ_ORACLE") == "live"
	oracle.Check(t, "std_sort", "sort", sortOracleGenerate(full), func(raw json.RawMessage) any {
		var request sortOracleRequest
		if err := json.Unmarshal(raw, &request); err != nil {
			t.Fatalf("bad request %s: %v", raw, err)
		}
		switch request.Op {
		case "sort":
			return sortOracleByType(request, false)
		case "sort_random_cmp":
			return sortOracleByType(request, true)
		case "search":
			return sortOracleSearch(request)
		}
		return map[string]string{"oracle_error": "unknown op " + request.Op}
	})
}

// TestSortLayoutOfOracleTypes pins the layouts the oracle's element types
// are derived to, since the oracle tests go through the derivation.
func TestSortLayoutOfOracleTypes(t *testing.T) {
	cases := []struct {
		name string
		got  rs.SortLayout
		want rs.SortLayout
	}{
		{"(u32, u32)", rs.SortLayoutOf[rs.Tuple2[uint32, uint32]](), rs.SortLayout{Size: 8, Copy: true, Freeze: true}},
		{"(u64, u64)", rs.SortLayoutOf[rs.Tuple2[uint64, uint64]](), rs.SortLayout{Size: 16, Copy: true, Freeze: true}},
		{"(u64, String)", rs.SortLayoutOf[rs.Tuple2[uint64, string]](), rs.SortLayout{Size: 32, Copy: false, Freeze: true}},
		{"(u64, [u64; 11])", rs.SortLayoutOf[rs.Tuple2[uint64, [11]uint64]](), rs.SortLayout{Size: 96, Copy: true, Freeze: true}},
		{"(u64, [u64; 12])", rs.SortLayoutOf[rs.Tuple2[uint64, [12]uint64]](), rs.SortLayout{Size: 104, Copy: true, Freeze: true}},
		{"(u32, Cell<u32>)", rs.SortLayoutOf[rs.Tuple2[uint32, rs.Cell[uint32]]](), rs.SortLayout{Size: 8, Copy: false, Freeze: false}},
		{"Option<u32>", rs.SortLayoutOf[rs.Option[uint32]](), rs.SortLayout{Size: 8, Copy: true, Freeze: true}},
		{"Option<String>", rs.SortLayoutOf[rs.Option[string]](), rs.SortLayout{Size: 24, Copy: false, Freeze: true}},
		{"(u8, u64, u8)", rs.SortLayoutOf[rs.Tuple3[uint8, uint64, uint8]](), rs.SortLayout{Size: 16, Copy: true, Freeze: true}},
	}
	for _, c := range cases {
		if c.got != c.want {
			t.Errorf("%s: got %+v, want %+v", c.name, c.got, c.want)
		}
	}
}
