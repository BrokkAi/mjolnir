package collections

// rs.Clone of a struct holding collections, which is how `#[derive(Clone)]`
// on a product type reaches them: the collections are rebuilt through
// rs.ReflectMap and rs.ReflectSeq, so a field tagged `arc` (an Arc<T> held
// as a *T) shares what its pointers point to, and an untagged one (a
// Box<T>) copies it.

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

type clonedState struct {
	Shared      BTreeMap[string, *int] `rs:"shared,arc"`
	Owned       BTreeMap[string, *int] `rs:"owned"`
	SharedSet   BTreeSet[*int]         `rs:"shared_set,arc"`
	SharedHash  HashMap[string, *int]  `rs:"shared_hash,arc"`
	SharedQueue VecDeque[*int]         `rs:"shared_queue,arc"`
	OwnedQueue  VecDeque[*int]         `rs:"owned_queue"`
}

func TestCloneSharesArcsInsideCollections(t *testing.T) {
	one, two := 1, 2
	var state clonedState
	state.Shared.Insert("a", &one)
	state.Owned.Insert("a", &one)
	state.SharedSet.Insert(&two)
	state.SharedHash.Insert("a", &one)
	state.SharedQueue.PushBack(&one)
	state.OwnedQueue.PushBack(&one)

	cloned := rs.Clone(state)

	shared, _ := cloned.Shared.Get("a")
	expect(t, *shared == &one, "an Arc in a BTreeMap is shared")
	owned, _ := cloned.Owned.Get("a")
	expect(t, *owned != &one && **owned == 1, "a Box in a BTreeMap is copied")
	element, _ := cloned.SharedSet.First()
	expect(t, element == &two, "an Arc in a BTreeSet is shared")
	hashed, _ := cloned.SharedHash.Get("a")
	expect(t, *hashed == &one, "an Arc in a HashMap is shared")
	front, _ := cloned.SharedQueue.Front()
	expect(t, *front == &one, "an Arc in a VecDeque is shared")
	ownedFront, _ := cloned.OwnedQueue.Front()
	expect(t, *ownedFront != &one && **ownedFront == 1, "a Box in a VecDeque is copied")

	// The clones are collections of their own: changing one leaves the
	// other as it was.
	cloned.Shared.Insert("b", &two)
	cloned.SharedQueue.PushFront(&two)
	expectEq(t, state.Shared.Len(), 1)
	expectEq(t, state.SharedQueue.Len(), 1)
}
