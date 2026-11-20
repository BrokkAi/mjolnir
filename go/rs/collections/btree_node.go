// Stands in for alloc 1.99.1 src/collections/btree/node.rs, navigate.rs,
// search.rs, remove.rs, fix.rs, split.rs and append.rs.
//
// Rust keeps a B-tree whose nodes hold up to eleven entries inline, and
// relies on the borrow checker to stop anyone holding a reference into a node
// while the node is rearranged. Go has no such check, and the port hands out
// pointers to values (Get, GetMut, Entry, iteration), so here every entry has
// its own node and a node never moves: the tree is an AVL tree whose nodes are
// relinked, never copied. Nothing a caller can observe depends on the shape
// of Rust's nodes; what the map does with keys and values follows map.rs.

package collections

import (
	"github.com/BrokkAi/mjolnir/go/rs"
)

// btreeNode is one entry of a BTreeMap, and its place in the tree.
type btreeNode[K, V any] struct {
	key    K
	value  V
	left   *btreeNode[K, V]
	right  *btreeNode[K, V]
	parent *btreeNode[K, V]
	// height is the height of the subtree rooted here: 1 for a node without
	// children. It is 0 once the node has been removed from its map.
	height int8
}

const btreeIterationMessage = "collections: a BTreeMap or BTreeSet entry was removed while the collection was being iterated"

func btreeHeight[K, V any](n *btreeNode[K, V]) int8 {
	if n == nil {
		return 0
	}
	return n.height
}

// first returns the leftmost node of the subtree rooted at n.
func (n *btreeNode[K, V]) first() *btreeNode[K, V] {
	for n.left != nil {
		n = n.left
	}
	return n
}

// last returns the rightmost node of the subtree rooted at n.
func (n *btreeNode[K, V]) last() *btreeNode[K, V] {
	for n.right != nil {
		n = n.right
	}
	return n
}

// next returns the node with the next greater key, or nil.
func (n *btreeNode[K, V]) next() *btreeNode[K, V] {
	if n.right != nil {
		return n.right.first()
	}
	for parent := n.parent; parent != nil; n, parent = parent, parent.parent {
		if parent.left == n {
			return parent
		}
	}
	return nil
}

// prev returns the node with the next smaller key, or nil.
func (n *btreeNode[K, V]) prev() *btreeNode[K, V] {
	if n.left != nil {
		return n.left.last()
	}
	for parent := n.parent; parent != nil; n, parent = parent, parent.parent {
		if parent.right == n {
			return parent
		}
	}
	return nil
}

// firstNode returns the node with the smallest key, or nil.
func (m BTreeMap[K, V]) firstNode() *btreeNode[K, V] {
	if m.root == nil {
		return nil
	}
	return m.root.first()
}

// lastNode returns the node with the greatest key, or nil.
func (m BTreeMap[K, V]) lastNode() *btreeNode[K, V] {
	if m.root == nil {
		return nil
	}
	return m.root.last()
}

// find returns the node holding key, or nil. Like search_tree in search.rs it
// compares the key looked for against the stored key, in that order.
func (m BTreeMap[K, V]) find(key K) *btreeNode[K, V] {
	n := m.root
	for n != nil {
		order := rs.Cmp(key, n.key)
		switch {
		case order < 0:
			n = n.left
		case order > 0:
			n = n.right
		default:
			return n
		}
	}
	return nil
}

// locate returns the node holding key or, when there is none, the place a
// node for key attaches: the parent (nil in an empty tree) and the side.
func (m BTreeMap[K, V]) locate(key K) (found, parent *btreeNode[K, V], right bool) {
	n := m.root
	for n != nil {
		order := rs.Cmp(key, n.key)
		switch {
		case order < 0:
			parent, right = n, false
			n = n.left
		case order > 0:
			parent, right = n, true
			n = n.right
		default:
			return n, nil, false
		}
	}
	return nil, parent, right
}

// lowerBound returns the first node inside a range that starts at the given
// bound, or nil when every key lies before it.
func (m BTreeMap[K, V]) lowerBound(kind boundKind, key K) *btreeNode[K, V] {
	if kind == boundUnbounded {
		return m.firstNode()
	}
	var result *btreeNode[K, V]
	n := m.root
	for n != nil {
		order := rs.Cmp(key, n.key)
		switch {
		case order < 0:
			result = n
			n = n.left
		case order > 0:
			n = n.right
		case kind == boundIncluded:
			return n
		default:
			n = n.right
		}
	}
	return result
}

// upperBound returns the last node inside a range that ends at the given
// bound, or nil when every key lies after it.
func (m BTreeMap[K, V]) upperBound(kind boundKind, key K) *btreeNode[K, V] {
	if kind == boundUnbounded {
		return m.lastNode()
	}
	var result *btreeNode[K, V]
	n := m.root
	for n != nil {
		order := rs.Cmp(key, n.key)
		switch {
		case order > 0:
			result = n
			n = n.right
		case order < 0:
			n = n.left
		case kind == boundIncluded:
			return n
		default:
			n = n.left
		}
	}
	return result
}

// replaceChild makes replacement the child of parent that old was, or the
// root when parent is nil.
func (m *BTreeMap[K, V]) replaceChild(parent, old, replacement *btreeNode[K, V]) {
	switch {
	case parent == nil:
		m.root = replacement
	case parent.left == old:
		parent.left = replacement
	default:
		parent.right = replacement
	}
	if replacement != nil {
		replacement.parent = parent
	}
}

func (n *btreeNode[K, V]) fixHeight() {
	n.height = max(btreeHeight(n.left), btreeHeight(n.right)) + 1
}

// rotateRight lifts n's left child into n's place and returns it.
func (m *BTreeMap[K, V]) rotateRight(n *btreeNode[K, V]) *btreeNode[K, V] {
	lifted := n.left
	n.left = lifted.right
	if n.left != nil {
		n.left.parent = n
	}
	m.replaceChild(n.parent, n, lifted)
	lifted.right = n
	n.parent = lifted
	n.fixHeight()
	lifted.fixHeight()
	return lifted
}

// rotateLeft lifts n's right child into n's place and returns it.
func (m *BTreeMap[K, V]) rotateLeft(n *btreeNode[K, V]) *btreeNode[K, V] {
	lifted := n.right
	n.right = lifted.left
	if n.right != nil {
		n.right.parent = n
	}
	m.replaceChild(n.parent, n, lifted)
	lifted.left = n
	n.parent = lifted
	n.fixHeight()
	lifted.fixHeight()
	return lifted
}

// retrace restores the balance and the recorded heights from n up to the
// root after a node was attached below n or detached from below it. It stops
// at the first ancestor whose height did not change.
func (m *BTreeMap[K, V]) retrace(n *btreeNode[K, V]) {
	for n != nil {
		leftHeight, rightHeight := btreeHeight(n.left), btreeHeight(n.right)
		switch {
		case leftHeight > rightHeight+1:
			if btreeHeight(n.left.left) < btreeHeight(n.left.right) {
				m.rotateLeft(n.left)
			}
			n = m.rotateRight(n)
		case rightHeight > leftHeight+1:
			if btreeHeight(n.right.right) < btreeHeight(n.right.left) {
				m.rotateRight(n.right)
			}
			n = m.rotateLeft(n)
		default:
			height := max(leftHeight, rightHeight) + 1
			if n.height == height {
				return
			}
			n.height = height
		}
		n = n.parent
	}
}

// link attaches node, which is not in a tree, at the place locate reported.
func (m *BTreeMap[K, V]) link(node, parent *btreeNode[K, V], right bool) {
	node.left, node.right, node.parent, node.height = nil, nil, parent, 1
	switch {
	case parent == nil:
		m.root = node
	case right:
		parent.right = node
	default:
		parent.left = node
	}
	m.length++
	m.rooted = true
	m.retrace(parent)
}

// unlink removes node from the tree. The node keeps its key and value, so a
// pointer to the value that a caller still holds stays readable, but it no
// longer leads anywhere.
func (m *BTreeMap[K, V]) unlink(node *btreeNode[K, V]) {
	var from *btreeNode[K, V]
	switch {
	case node.left == nil:
		from = node.parent
		m.replaceChild(node.parent, node, node.right)
	case node.right == nil:
		from = node.parent
		m.replaceChild(node.parent, node, node.left)
	default:
		// The successor has no left child. It takes node's place, so that
		// every other entry stays in the node it has always had.
		successor := node.right.first()
		if successor.parent == node {
			from = successor
		} else {
			from = successor.parent
			m.replaceChild(successor.parent, successor, successor.right)
			successor.right = node.right
			successor.right.parent = successor
		}
		successor.left = node.left
		successor.left.parent = successor
		successor.height = node.height
		m.replaceChild(node.parent, node, successor)
	}
	node.left, node.right, node.parent, node.height = nil, nil, nil, 0
	m.length--
	m.retrace(from)
}

// nodes returns every node in key order.
func (m BTreeMap[K, V]) nodes() []*btreeNode[K, V] {
	nodes := make([]*btreeNode[K, V], 0, m.length)
	for n := m.firstNode(); n != nil; n = n.next() {
		nodes = append(nodes, n)
	}
	return nodes
}

// btreeBuild links nodes, which are in key order, into a balanced tree below
// parent and returns its root. It is what bulk_push in append.rs is for.
func btreeBuild[K, V any](nodes []*btreeNode[K, V], parent *btreeNode[K, V]) *btreeNode[K, V] {
	if len(nodes) == 0 {
		return nil
	}
	middle := len(nodes) / 2
	n := nodes[middle]
	n.parent = parent
	n.left = btreeBuild(nodes[:middle], n)
	n.right = btreeBuild(nodes[middle+1:], n)
	n.fixHeight()
	return n
}

// ascend calls yield with each node from first to last in key order. A nil
// last means the end of the map. The node after the current one is found
// before yield runs, so yield may remove the node it was given.
func btreeAscend[K, V any](first, last *btreeNode[K, V], yield func(*btreeNode[K, V]) bool) {
	for n := first; n != nil; {
		if n.height == 0 || (last != nil && last.height == 0) {
			rs.PanicAt(2, btreeIterationMessage)
		}
		following := n.next()
		final := n == last
		if !yield(n) || final {
			return
		}
		n = following
	}
}

// descend calls yield with each node from last down to first. A nil first
// means the start of the map.
func btreeDescend[K, V any](first, last *btreeNode[K, V], yield func(*btreeNode[K, V]) bool) {
	for n := last; n != nil; {
		if n.height == 0 || (first != nil && first.height == 0) {
			rs.PanicAt(2, btreeIterationMessage)
		}
		preceding := n.prev()
		final := n == first
		if !yield(n) || final {
			return
		}
		n = preceding
	}
}

// btreeIter is the double-ended, exact-size iterator Rust's Iter is, for the
// algorithms of set.rs that step two sets against each other. Iteration for
// callers goes through btreeAscend and btreeDescend instead.
type btreeIter[K, V any] struct {
	front  *btreeNode[K, V]
	back   *btreeNode[K, V]
	length int
}

func (m BTreeMap[K, V]) iter() btreeIter[K, V] {
	return btreeIter[K, V]{front: m.firstNode(), back: m.lastNode(), length: m.length}
}

// next is Iter::next: the next node from the front, or nil.
func (it *btreeIter[K, V]) next() *btreeNode[K, V] {
	if it.length == 0 {
		return nil
	}
	it.length--
	n := it.front
	it.front = n.next()
	return n
}

// nextBack is Iter::next_back: the next node from the back, or nil.
func (it *btreeIter[K, V]) nextBack() *btreeNode[K, V] {
	if it.length == 0 {
		return nil
	}
	it.length--
	n := it.back
	it.back = n.prev()
	return n
}
