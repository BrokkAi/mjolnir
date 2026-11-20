package rs_test

import (
	"slices"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// Differential tests of rs.Arc, rs.Weak and the sharing rules of rs.Clone
// against std::sync::Arc and Weak and #[derive(Clone)], through the oracle
// module std_cmp (go/port/oracle/src/std_cmp.rs); cmp_oracle_test.go says
// how to record and replay them.

// cmpoTrackLog is what happened to the cmpoTracked values of one request.
type cmpoTrackLog struct {
	nextID  uint32
	dropped []uint32
	cloned  [][2]uint32
}

func (l *cmpoTrackLog) next() uint32 {
	l.nextID++
	return l.nextID
}

func (l *cmpoTrackLog) takeDropped() []uint32 {
	dropped := l.dropped
	l.dropped = nil
	if dropped == nil {
		return []uint32{}
	}
	return dropped
}

// cmpoTracked is Tracked of std_cmp.rs: a value that says when it is cloned
// and dropped. Each value, the clones included, gets the next id; equality
// is by value alone.
type cmpoTracked struct {
	id    uint32
	value int64
	log   *cmpoTrackLog
}

func cmpoTrackedNew(log *cmpoTrackLog, value int64) cmpoTracked {
	return cmpoTracked{id: log.next(), value: value, log: log}
}

func (t *cmpoTracked) Clone() cmpoTracked {
	id := t.log.next()
	t.log.cloned = append(t.log.cloned, [2]uint32{t.id, id})
	return cmpoTracked{id: id, value: t.value, log: t.log}
}

func (t *cmpoTracked) Drop() { t.log.dropped = append(t.log.dropped, t.id) }

func (t *cmpoTracked) Equal(other *cmpoTracked) bool { return t.value == other.value }

func (t *cmpoTracked) json() any { return []any{t.id, t.value} }

// cmpoArcSteps are the steps of an Arc script, each as often as it is listed.
var cmpoArcSteps = []string{
	"new", "new", "clone", "clone", "clone", "drop", "drop", "downgrade", "downgrade",
	"weak_new", "upgrade", "upgrade", "weak_clone", "weak_drop", "weak_drop", "counts", "counts",
	"counts", "weak_counts", "weak_counts", "get_mut", "get_mut", "make_mut", "make_mut",
	"try_unwrap", "into_inner", "unwrap_or_clone", "ptr_eq", "weak_ptr_eq", "eq", "get", "forget",
	"weak_forget",
}

// arcScript generates a script of std_cmp.rs's arc_script. It keeps a model
// of the handles, the allocation each slot points to and the counts of each
// allocation, so as to pick slots that hold a handle; a slot without one is
// picked now and then too. The model only steers the choice: what each step
// observes is the oracle's to say.
func (r *cmpoRng) arcScript() []any {
	const (
		dead    = -1 // the slot holds no handle
		nowhere = -2 // Weak::new()
	)
	var strong, weak []int // by allocation
	var arcs, weaks []int  // the allocation of each slot
	allocate := func() int {
		strong, weak = append(strong, 1), append(weak, 0)
		return len(strong) - 1
	}
	pick := func(slots []int) int {
		var held []int
		for at, allocation := range slots {
			if allocation != dead {
				held = append(held, at)
			}
		}
		if len(held) > 0 && r.intn(16) != 0 {
			return cmpoPick(r, held)
		}
		return r.intn(len(slots))
	}
	value := func() int { return r.intn(4) }
	steps := []any{[]any{"new", value()}}
	arcs = append(arcs, allocate())
	held := func(slots []int) bool {
		return slices.ContainsFunc(slots, func(allocation int) bool { return allocation != dead })
	}
	for range 4 + r.intn(36) {
		step := cmpoPick(r, cmpoArcSteps)
		// A step that needs a handle where the model sees none makes one.
		switch step {
		case "upgrade", "weak_clone", "weak_drop", "weak_counts", "weak_ptr_eq", "weak_forget":
			if !held(weaks) {
				step = "downgrade"
			}
		}
		switch step {
		case "new", "weak_new":
		default:
			if !held(arcs) {
				step = "new"
			}
		}
		switch step {
		case "new":
			steps = append(steps, []any{step, value()})
			arcs = append(arcs, allocate())
		case "clone":
			at := pick(arcs)
			steps = append(steps, []any{step, at})
			if arcs[at] != dead {
				strong[arcs[at]]++
			}
			arcs = append(arcs, arcs[at])
		case "drop", "into_inner", "unwrap_or_clone", "forget":
			at := pick(arcs)
			steps = append(steps, []any{step, at})
			if arcs[at] != dead && step != "forget" {
				// A forgotten handle keeps its count.
				strong[arcs[at]]--
			}
			arcs[at] = dead
		case "downgrade":
			at := pick(arcs)
			steps = append(steps, []any{step, at})
			if arcs[at] != dead {
				weak[arcs[at]]++
			}
			weaks = append(weaks, arcs[at])
		case "weak_new":
			steps = append(steps, []any{step})
			weaks = append(weaks, nowhere)
		case "upgrade":
			at := pick(weaks)
			steps = append(steps, []any{step, at})
			if allocation := weaks[at]; allocation >= 0 && strong[allocation] > 0 {
				strong[allocation]++
				arcs = append(arcs, allocation)
			} else {
				arcs = append(arcs, dead)
			}
		case "weak_clone":
			at := pick(weaks)
			steps = append(steps, []any{step, at})
			if weaks[at] >= 0 {
				weak[weaks[at]]++
			}
			weaks = append(weaks, weaks[at])
		case "weak_drop", "weak_forget":
			at := pick(weaks)
			steps = append(steps, []any{step, at})
			if weaks[at] >= 0 && step == "weak_drop" {
				weak[weaks[at]]--
			}
			weaks[at] = dead
		case "try_unwrap":
			at := pick(arcs)
			steps = append(steps, []any{step, at})
			if allocation := arcs[at]; allocation != dead && strong[allocation] == 1 {
				strong[allocation] = 0
				arcs[at] = dead
			}
		case "make_mut":
			at := pick(arcs)
			steps = append(steps, []any{step, at, value()})
			if allocation := arcs[at]; allocation != dead {
				switch {
				case strong[allocation] > 1:
					strong[allocation]--
					arcs[at] = allocate()
				case weak[allocation] > 0:
					// The value moves out; the Weaks stay behind.
					strong[allocation] = 0
					arcs[at] = allocate()
				}
			}
		case "counts", "get":
			steps = append(steps, []any{step, pick(arcs)})
		case "weak_counts":
			steps = append(steps, []any{step, pick(weaks)})
		case "get_mut":
			steps = append(steps, []any{step, pick(arcs), value()})
		case "ptr_eq", "eq":
			steps = append(steps, []any{step, pick(arcs), pick(arcs)})
		case "weak_ptr_eq":
			steps = append(steps, []any{step, pick(weaks), pick(weaks)})
		default:
			panic("no generator for the step " + step)
		}
	}
	return steps
}

// cmpoRunArcScript runs a script on rs.Arc and rs.Weak as arc_script runs it
// on Arc and Weak. Where Rust drops a value at the end of a step (one taken
// out of its Arc), so does this.
func cmpoRunArcScript(steps []any) any {
	log := &cmpoTrackLog{}
	type arcSlot struct {
		arc  rs.Arc[cmpoTracked]
		live bool
	}
	type weakSlot struct {
		weak rs.Weak[cmpoTracked]
		live bool
	}
	var arcs []arcSlot
	var weaks []weakSlot
	observed := []any{}
	for _, raw := range steps {
		s := raw.([]any)
		index := func(k int) int { return int(cmpoInt(s[k])) }
		var result any
		switch op := s[0].(string); op {
		case "new":
			arc := rs.ArcNew(cmpoTrackedNew(log, cmpoInt(s[1])))
			result = arc.Get().id
			arcs = append(arcs, arcSlot{arc: arc, live: true})
		case "clone":
			slot := arcs[index(1)]
			if slot.live {
				arcs = append(arcs, arcSlot{arc: slot.arc.Clone(), live: true})
				result = true
			} else {
				arcs = append(arcs, arcSlot{})
			}
		case "drop":
			if slot := &arcs[index(1)]; slot.live {
				slot.arc.Drop()
				slot.live = false
			}
		case "downgrade":
			slot := arcs[index(1)]
			if slot.live {
				weaks = append(weaks, weakSlot{weak: rs.ArcDowngrade(slot.arc), live: true})
				result = true
			} else {
				weaks = append(weaks, weakSlot{})
			}
		case "weak_new":
			weaks = append(weaks, weakSlot{weak: rs.WeakNew[cmpoTracked](), live: true})
		case "upgrade":
			slot := weaks[index(1)]
			if slot.live {
				upgraded, some := slot.weak.Upgrade().Get()
				arcs = append(arcs, arcSlot{arc: upgraded, live: some})
				result = some
			} else {
				arcs = append(arcs, arcSlot{})
			}
		case "weak_clone":
			slot := weaks[index(1)]
			if slot.live {
				weaks = append(weaks, weakSlot{weak: slot.weak.Clone(), live: true})
				result = true
			} else {
				weaks = append(weaks, weakSlot{})
			}
		case "weak_drop":
			if slot := &weaks[index(1)]; slot.live {
				slot.weak.Drop()
				slot.live = false
			}
		case "counts":
			if slot := arcs[index(1)]; slot.live {
				result = []any{rs.ArcStrongCount(slot.arc), rs.ArcWeakCount(slot.arc)}
			}
		case "weak_counts":
			if slot := weaks[index(1)]; slot.live {
				result = []any{slot.weak.StrongCount(), slot.weak.WeakCount()}
			}
		case "get_mut":
			if slot := arcs[index(1)]; slot.live {
				tracked, some := rs.ArcGetMut(slot.arc).Get()
				if some {
					tracked.value = cmpoInt(s[2])
				}
				result = some
			}
		case "make_mut":
			if slot := &arcs[index(1)]; slot.live {
				tracked := rs.ArcMakeMut(&slot.arc)
				tracked.value = cmpoInt(s[2])
				result = tracked.id
			}
		case "try_unwrap":
			if slot := &arcs[index(1)]; slot.live {
				if tracked, ok := rs.ArcTryUnwrap(slot.arc); ok {
					slot.live = false
					result = tracked.json()
					tracked.Drop()
				} else {
					result = false
				}
			}
		case "into_inner":
			if slot := &arcs[index(1)]; slot.live {
				slot.live = false
				if tracked, some := rs.ArcIntoInner(slot.arc).Get(); some {
					result = tracked.json()
					tracked.Drop()
				} else {
					result = false
				}
			}
		case "unwrap_or_clone":
			if slot := &arcs[index(1)]; slot.live {
				slot.live = false
				tracked := rs.ArcUnwrapOrClone(slot.arc)
				result = tracked.json()
				tracked.Drop()
			}
		case "ptr_eq":
			if a, b := arcs[index(1)], arcs[index(2)]; a.live && b.live {
				result = rs.ArcPtrEq(a.arc, b.arc)
			}
		case "weak_ptr_eq":
			if a, b := weaks[index(1)], weaks[index(2)]; a.live && b.live {
				result = rs.WeakPtrEq(a.weak, b.weak)
			}
		case "eq":
			if a, b := arcs[index(1)], arcs[index(2)]; a.live && b.live {
				result = rs.Eq(a.arc, b.arc)
			}
		case "get":
			if slot := arcs[index(1)]; slot.live {
				result = slot.arc.Get().json()
			}
		case "forget":
			if slot := &arcs[index(1)]; slot.live {
				rs.Forget(slot.arc)
				slot.live = false
			}
		case "weak_forget":
			if slot := &weaks[index(1)]; slot.live {
				rs.Forget(slot.weak)
				slot.live = false
			}
		default:
			panic("not an Arc step: " + op)
		}
		observed = append(observed, []any{result, log.takeDropped()})
	}
	// The handles left go in order, the Arcs first.
	for i := range arcs {
		if arcs[i].live {
			arcs[i].arc.Drop()
		}
	}
	for i := range weaks {
		if weaks[i].live {
			weaks[i].weak.Drop()
		}
	}
	return map[string]any{"steps": observed, "final": log.takeDropped()}
}

func TestStdCmpOracleArc(t *testing.T) {
	cmpoCheck(t, "std_cmp_arc", func(yield func(any)) {
		r := &cmpoRng{state: 4}
		for range 500 * cmpoScale() {
			yield(map[string]any{"op": "arc", "steps": r.arcScript()})
		}
	}, func(request map[string]any) any {
		return cmpoRunArcScript(request["steps"].([]any))
	})
}

// cmpoHolder is Holder of std_cmp.rs: Tracked values held every way a derived
// Clone treats differently. The `arc` tags say which pointers are Arcs.
type cmpoHolder struct {
	boxed      *cmpoTracked                    `rs:"boxed"`
	shared     *cmpoTracked                    `rs:"shared,arc"`
	maybe      rs.Option[*cmpoTracked]         `rs:"maybe,arc"`
	maybeBoxed rs.Option[*cmpoTracked]         `rs:"maybe_boxed"`
	list       []*cmpoTracked                  `rs:"list,arc"`
	owned      []cmpoTracked                   `rs:"owned"`
	bytes      []byte                          `rs:"bytes,arc"`
	counted    rs.Arc[cmpoTracked]             `rs:"counted"`
	weak       rs.Weak[cmpoTracked]            `rs:"weak"`
	cell       rs.Cell[uint32]                 `rs:"cell"`
	refcell    rs.RefCell[[]uint32]            `rs:"refcell"`
	nested     []rs.Option[*cmpoTracked]       `rs:"nested,arc"`
	pair       rs.Tuple2[*cmpoTracked, string] `rs:"pair,arc"`
	boxes      rs.Tuple2[*cmpoTracked, uint8]  `rs:"boxes"`
}

var _ = rs.RegisterStruct[cmpoHolder]("Holder")

// Drop is Holder's drop glue: the fields in declaration order. Dropping an
// Arc that another handle shares drops nothing, so the plain pointers that
// stand for Arcs are left alone.
func (h *cmpoHolder) Drop() {
	h.boxed.Drop()
	if boxed, some := h.maybeBoxed.Get(); some {
		boxed.Drop()
	}
	for i := range h.owned {
		h.owned[i].Drop()
	}
	h.counted.Drop()
	h.weak.Drop()
	h.boxes.F0.Drop()
}

func cmpoCloneHolder(n int) any {
	log := &cmpoTrackLog{}
	tracked := func(value int64) *cmpoTracked {
		made := cmpoTrackedNew(log, value)
		return &made
	}
	// Built one value at a time, in the order std_cmp.rs builds them.
	boxed := tracked(1)
	shared := tracked(2)
	maybe := rs.Some(tracked(3))
	maybeBoxed := rs.Some(tracked(4))
	var list []*cmpoTracked
	for i := range n {
		list = append(list, tracked(10+int64(i)))
	}
	var owned []cmpoTracked
	for i := range n {
		owned = append(owned, cmpoTrackedNew(log, 20+int64(i)))
	}
	bytes := []byte("shared bytes")
	counted := rs.ArcNew(cmpoTrackedNew(log, 5))
	weak := rs.ArcDowngrade(counted)
	var nested []rs.Option[*cmpoTracked]
	for i := range n {
		if i%2 == 0 {
			nested = append(nested, rs.Some(tracked(30+int64(i))))
		} else {
			nested = append(nested, rs.None[*cmpoTracked]())
		}
	}
	pair := rs.T2(tracked(6), "pair")
	boxes := rs.T2(tracked(7), uint8(8))
	holder := cmpoHolder{
		boxed:      boxed,
		shared:     shared,
		maybe:      maybe,
		maybeBoxed: maybeBoxed,
		list:       list,
		owned:      owned,
		bytes:      bytes,
		counted:    counted,
		weak:       weak,
		cell:       rs.CellNew(uint32(9)),
		refcell:    rs.RefCellNew([]uint32{1, 2}),
		nested:     nested,
		pair:       pair,
		boxes:      boxes,
	}
	defer holder.counted.Drop()
	defer holder.weak.Drop()

	clone := rs.Clone(holder)
	cloned := log.cloned
	if cloned == nil {
		cloned = [][2]uint32{}
	}
	bothSome := func(a, b rs.Option[*cmpoTracked]) any {
		left, leftSome := a.Get()
		right, rightSome := b.Get()
		if !leftSome || !rightSome {
			return nil
		}
		return left == right
	}
	listShared := []any{}
	ownedShared := []any{}
	nestedShared := []any{}
	for i := range n {
		listShared = append(listShared, holder.list[i] == clone.list[i])
		ownedShared = append(ownedShared, &holder.owned[i] == &clone.owned[i])
		nestedShared = append(nestedShared, bothSome(holder.nested[i], clone.nested[i]))
	}
	shared2 := map[string]any{
		"boxed":       holder.boxed == clone.boxed,
		"shared":      holder.shared == clone.shared,
		"maybe":       bothSome(holder.maybe, clone.maybe),
		"maybe_boxed": holder.maybeBoxed.Unwrap() == clone.maybeBoxed.Unwrap(),
		"list":        listShared,
		"list_buffer": n > 0 && &holder.list[0] == &clone.list[0],
		"owned":       ownedShared,
		"bytes":       &holder.bytes[0] == &clone.bytes[0],
		"counted":     rs.ArcPtrEq(holder.counted, clone.counted),
		"weak":        rs.WeakPtrEq(holder.weak, clone.weak),
		"nested":      nestedShared,
		"pair":        holder.pair.F0 == clone.pair.F0,
		"boxes":       holder.boxes.F0 == clone.boxes.F0,
	}
	copiedOwned := []any{}
	for i := range clone.owned {
		copiedOwned = append(copiedOwned, clone.owned[i].json())
	}
	var copiedMaybe any
	if boxed, some := clone.maybeBoxed.Get(); some {
		copiedMaybe = boxed.json()
	}
	copied := map[string]any{
		"boxed":       clone.boxed.json(),
		"maybe_boxed": copiedMaybe,
		"owned":       copiedOwned,
		"cell":        clone.cell.Get(),
		"refcell":     *clone.refcell.Borrow(),
		"pair":        clone.pair.F1,
		"boxes":       []any{clone.boxes.F0.json(), clone.boxes.F1},
	}
	counts := []any{rs.ArcStrongCount(holder.counted), rs.ArcWeakCount(holder.counted)}
	clone.Drop()
	dropped := log.takeDropped()
	after := []any{rs.ArcStrongCount(holder.counted), rs.ArcWeakCount(holder.counted)}
	return map[string]any{
		"cloned":  cloned,
		"shared":  shared2,
		"copied":  copied,
		"counts":  counts,
		"dropped": dropped,
		"after":   after,
	}
}

func TestStdCmpOracleClone(t *testing.T) {
	cmpoCheck(t, "std_cmp_clone", func(yield func(any)) {
		for n := range 5 {
			yield(map[string]any{"op": "clone", "n": n})
		}
	}, func(request map[string]any) any {
		return cmpoCloneHolder(int(cmpoInt(request["n"])))
	})
}
