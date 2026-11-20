package collections

import (
	"fmt"
	"math/rand/v2"
	"slices"
	"sort"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// check verifies everything the tree promises: keys strictly ascending by
// rs.Cmp, the AVL balance, recorded heights, parent links, and the length.
// It is the port's counterpart of BTreeMap::check in map/tests.rs.
//
// A nil t turns a broken invariant into a panic, which the oracle steps
// report as a difference from Rust.
func (m BTreeMap[K, V]) check(t testing.TB) {
	if t != nil {
		t.Helper()
	}
	m.report(t, m.checkTree(true))
}

// checkStructure is check without the order of the keys: the counterpart
// of BTreeMap::check_invariants, for maps whose keys have an inconsistent
// order.
func (m BTreeMap[K, V]) checkStructure(t testing.TB) {
	t.Helper()
	m.report(t, m.checkTree(false))
}

func (m BTreeMap[K, V]) report(t testing.TB, err error) {
	if err == nil {
		return
	}
	if t == nil {
		panic("tree invariant broken: " + err.Error())
	}
	t.Helper()
	t.Fatalf("tree invariant broken: %v", err)
}

func (m BTreeMap[K, V]) checkInvariants() error { return m.checkTree(true) }

func (m BTreeMap[K, V]) checkTree(ordered bool) error {
	if m.root != nil && m.root.parent != nil {
		return fmt.Errorf("root has a parent")
	}
	if m.root != nil && !m.rooted {
		return fmt.Errorf("entries in a map that is not rooted")
	}
	count := 0
	var previous *btreeNode[K, V]
	var walk func(n, parent *btreeNode[K, V]) (int8, error)
	walk = func(n, parent *btreeNode[K, V]) (int8, error) {
		if n == nil {
			return 0, nil
		}
		if n.parent != parent {
			return 0, fmt.Errorf("node %v has the wrong parent", n.key)
		}
		left, err := walk(n.left, n)
		if err != nil {
			return 0, err
		}
		if ordered && previous != nil && rs.Cmp(previous.key, n.key) >= 0 {
			return 0, fmt.Errorf("keys out of order: %v then %v", previous.key, n.key)
		}
		previous = n
		count++
		right, err := walk(n.right, n)
		if err != nil {
			return 0, err
		}
		if left-right > 1 || right-left > 1 {
			return 0, fmt.Errorf("node %v is out of balance: %d against %d", n.key, left, right)
		}
		height := max(left, right) + 1
		if n.height != height {
			return 0, fmt.Errorf("node %v records height %d, has %d", n.key, n.height, height)
		}
		return height, nil
	}
	if _, err := walk(m.root, nil); err != nil {
		return err
	}
	if count != m.length {
		return fmt.Errorf("length is %d, tree holds %d", m.length, count)
	}
	// The same entries must come out of the stepping functions.
	forward := 0
	for n := m.firstNode(); n != nil; n = n.next() {
		forward++
	}
	backward := 0
	for n := m.lastNode(); n != nil; n = n.prev() {
		backward++
	}
	if forward != count || backward != count {
		return fmt.Errorf("stepping visits %d forward and %d backward of %d", forward, backward, count)
	}
	return nil
}

// treeHeight is the height of the tree, for the tests that want maps of
// several levels.
func (m BTreeMap[K, V]) treeHeight() int {
	return int(btreeHeight(m.root))
}

// TestBTreeMapAgainstModel drives a map and a plain sorted model with the
// same random operations and compares them after every step.
func TestBTreeMapAgainstModel(t *testing.T) {
	for seed := uint64(1); seed <= 40; seed++ {
		random := rand.New(rand.NewPCG(seed, 0x9e3779b97f4a7c15))
		var m BTreeMap[int, int]
		model := map[int]int{}
		keyRange := 8 << (seed % 8)
		steps := 400 + int(seed)*60
		for step := 0; step < steps; step++ {
			key := random.IntN(keyRange)
			switch random.IntN(12) {
			case 0, 1, 2, 3:
				old := m.Insert(key, step)
				want, had := model[key]
				if old.IsSome() != had || (had && old.Unwrap() != want) {
					t.Fatalf("seed %d step %d: Insert(%d) returned %v, model had %v %v", seed, step, key, old, want, had)
				}
				model[key] = step
			case 4, 5:
				old := m.Remove(key)
				want, had := model[key]
				if old.IsSome() != had || (had && old.Unwrap() != want) {
					t.Fatalf("seed %d step %d: Remove(%d) returned %v, model had %v %v", seed, step, key, old, want, had)
				}
				delete(model, key)
			case 6:
				*m.Entry(key).OrInsert(-step) += 1
				if _, had := model[key]; !had {
					model[key] = -step
				}
				model[key]++
			case 7:
				first := m.PopFirst()
				if len(model) == 0 {
					if first.IsSome() {
						t.Fatalf("seed %d step %d: PopFirst on an empty map returned %v", seed, step, first)
					}
				} else {
					smallest := slices.Min(modelKeys(model))
					if got := first.Unwrap(); got.F0 != smallest || got.F1 != model[smallest] {
						t.Fatalf("seed %d step %d: PopFirst returned %v, want %d", seed, step, got, smallest)
					}
					delete(model, smallest)
				}
			case 8:
				last := m.PopLast()
				if len(model) == 0 {
					if last.IsSome() {
						t.Fatalf("seed %d step %d: PopLast on an empty map returned %v", seed, step, last)
					}
				} else {
					greatest := slices.Max(modelKeys(model))
					if got := last.Unwrap(); got.F0 != greatest || got.F1 != model[greatest] {
						t.Fatalf("seed %d step %d: PopLast returned %v, want %d", seed, step, got, greatest)
					}
					delete(model, greatest)
				}
			case 9:
				modulus := 2 + random.IntN(4)
				m.Retain(func(k int, v *int) bool { return k%modulus != 0 })
				for k := range model {
					if k%modulus == 0 {
						delete(model, k)
					}
				}
			case 10:
				right := m.SplitOff(key)
				right.check(t)
				for k, v := range right.Iter() {
					if k < key || model[k] != *v {
						t.Fatalf("seed %d step %d: SplitOff(%d) moved %d", seed, step, key, k)
					}
				}
				for k := range m.Keys() {
					if k >= key {
						t.Fatalf("seed %d step %d: SplitOff(%d) left %d behind", seed, step, key, k)
					}
				}
				if m.Len()+right.Len() != len(model) {
					t.Fatalf("seed %d step %d: SplitOff lost entries", seed, step)
				}
				// Put them back, some through Append and some through Extend.
				if step%2 == 0 {
					m.Append(&right)
					if !right.IsEmpty() {
						t.Fatalf("seed %d step %d: Append left entries in its argument", seed, step)
					}
				} else {
					m.Extend(right.IntoIter())
				}
			case 11:
				got, ok := m.Get(key)
				want, had := model[key]
				if ok != had || (had && *got != want) {
					t.Fatalf("seed %d step %d: Get(%d) = %v %v, want %v %v", seed, step, key, got, ok, want, had)
				}
			}
			m.check(t)
			compareWithModel(t, &m, model, fmt.Sprintf("seed %d step %d", seed, step))
		}
	}
}

func modelKeys(model map[int]int) []int {
	keys := make([]int, 0, len(model))
	for k := range model {
		keys = append(keys, k)
	}
	sort.Ints(keys)
	return keys
}

func compareWithModel(t *testing.T, m *BTreeMap[int, int], model map[int]int, where string) {
	t.Helper()
	keys := modelKeys(model)
	if m.Len() != len(keys) {
		t.Fatalf("%s: Len is %d, model holds %d", where, m.Len(), len(keys))
	}
	index := 0
	for k, v := range m.Iter() {
		if index >= len(keys) || k != keys[index] || *v != model[k] {
			t.Fatalf("%s: entry %d is (%d, %d), model disagrees", where, index, k, *v)
		}
		index++
	}
	if index != len(keys) {
		t.Fatalf("%s: iteration stopped after %d of %d", where, index, len(keys))
	}
	index = len(keys)
	for k := range m.KeysRev() {
		index--
		if index < 0 || k != keys[index] {
			t.Fatalf("%s: reverse entry is %d, model disagrees", where, k)
		}
	}
	if index != 0 {
		t.Fatalf("%s: reverse iteration stopped early", where)
	}
}
