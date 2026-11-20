// Port of pulldown-cmark 0.10.3 src/tree.rs

// Copyright 2018 Google LLC
//
// Use of this source code is governed by an MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT.

package pulldowncmark

import "github.com/BrokkAi/mjolnir/go/rs"

// A Vec-based container for a tree structure.

// treeTreeIndex is the index of a node. Rust's TreeIndex wraps a
// NonZeroUsize, so that `Option<TreeIndex>` is one word with zero for None;
// this type is that word: zero is None, and every `Option<TreeIndex>` of the
// Rust source is a treeTreeIndex here.
type treeTreeIndex int

// treeNone is `Option::<TreeIndex>::None`.
const treeNone treeTreeIndex = 0

func treeTreeIndexNew(i int) treeTreeIndex {
	if i == 0 {
		// NonZeroUsize::new(i).unwrap()
		rs.PanicAt(1, optionUnwrapNone)
	}
	return treeTreeIndex(i)
}

func (ix treeTreeIndex) get() int {
	return int(ix)
}

// unwrap is `Option::<TreeIndex>::unwrap`.
func (ix treeTreeIndex) unwrap() treeTreeIndex {
	if ix == treeNone {
		rs.PanicAt(1, optionUnwrapNone)
	}
	return ix
}

// add is `impl Add<usize> for TreeIndex`.
func (ix treeTreeIndex) add(rhs int) treeTreeIndex {
	inner := int(ix) + rhs
	return treeTreeIndexNew(inner)
}

// sub is `impl Sub<usize> for TreeIndex`.
func (ix treeTreeIndex) sub(rhs int) treeTreeIndex {
	if rhs > int(ix) {
		// checked_sub(rhs).unwrap()
		rs.PanicAt(1, optionUnwrapNone)
	}
	inner := int(ix) - rhs
	return treeTreeIndexNew(inner)
}

type treeNode struct {
	child treeTreeIndex
	next  treeTreeIndex
	item  parseItem
}

// treeTree is a tree abstraction, intended for fast building as a preorder
// traversal. Rust's Tree is generic over the item; the crate only ever holds
// parse::Item in it, and so does this type.
//
// `tree[ix]` in the Rust source is `tree.nodes[ix]` here. A pointer into
// nodes must not be kept across append or createNode, which may move them.
type treeTree struct {
	nodes []treeNode
	spine []treeTreeIndex // indices of nodes on path to current node
	cur_  treeTreeIndex
}

// Indices start at one, so we place a dummy value at index zero.
// The alternative would be subtracting one from every TreeIndex
// every time we convert it to usize to index our nodes.
func treeTreeWithCapacity(cap_ int) treeTree {
	nodes := make([]treeNode, 0, cap_)
	nodes = append(nodes, treeNode{
		child: treeNone,
		next:  treeNone,
		item:  parseItemDefault(),
	})
	return treeTree{
		nodes: nodes,
		spine: nil,
		cur_:  treeNone,
	}
}

// cur returns the index of the element currently in focus.
func (tree *treeTree) cur() treeTreeIndex {
	return tree.cur_
}

// append appends one item to the current position in the tree.
func (tree *treeTree) append(item parseItem) treeTreeIndex {
	ix := tree.createNode(item)
	this := ix

	if ix := tree.cur_; ix != treeNone {
		tree.nodes[ix].next = this
	} else if len(tree.spine) > 0 {
		parent := tree.spine[len(tree.spine)-1]
		tree.nodes[parent].child = this
	}
	tree.cur_ = this
	return ix
}

// createNode creates an isolated node.
func (tree *treeTree) createNode(item parseItem) treeTreeIndex {
	this := len(tree.nodes)
	tree.nodes = append(tree.nodes, treeNode{
		child: treeNone,
		next:  treeNone,
		item:  item,
	})
	return treeTreeIndexNew(this)
}

// push pushes down one level, so that new items become children of the
// current node. The new focus index is returned.
func (tree *treeTree) push() treeTreeIndex {
	curIx := tree.cur_.unwrap()
	tree.spine = append(tree.spine, curIx)
	tree.cur_ = tree.nodes[curIx].child
	return curIx
}

// pop pops back up a level.
func (tree *treeTree) pop() treeTreeIndex {
	if len(tree.spine) == 0 {
		return treeNone
	}
	ix := tree.spine[len(tree.spine)-1]
	tree.spine = tree.spine[:len(tree.spine)-1]
	tree.cur_ = ix
	return ix
}

// removeNode removes the last node, as `pop` but removing it.
func (tree *treeTree) removeNode() treeTreeIndex {
	if len(tree.spine) == 0 {
		return treeNone
	}
	ix := tree.spine[len(tree.spine)-1]
	tree.spine = tree.spine[:len(tree.spine)-1]
	tree.cur_ = ix
	if len(tree.nodes) == 0 {
		return treeNone
	}
	tree.nodes = tree.nodes[:len(tree.nodes)-1]
	tree.nodes[ix].child = treeNone
	return ix
}

// peekUp looks at the parent node.
func (tree *treeTree) peekUp() treeTreeIndex {
	if len(tree.spine) == 0 {
		return treeNone
	}
	return tree.spine[len(tree.spine)-1]
}

// peekGrandparent looks at grandparent node.
func (tree *treeTree) peekGrandparent() treeTreeIndex {
	if len(tree.spine) >= 2 {
		return tree.spine[len(tree.spine)-2]
	}
	return treeNone
}

// isEmpty returns true when there are no nodes other than the root node
// in the tree, false otherwise.
func (tree *treeTree) isEmpty() bool {
	return len(tree.nodes) <= 1
}

// spineLen returns the length of the spine.
func (tree *treeTree) spineLen() int {
	return len(tree.spine)
}

// reset resets the focus to the first node added to the tree, if it exists.
func (tree *treeTree) reset() {
	if tree.isEmpty() {
		tree.cur_ = treeNone
	} else {
		tree.cur_ = treeTreeIndexNew(1)
	}
	tree.spine = tree.spine[:0]
}

// walkSpine walks the spine from a root node up to, but not including, the
// current node.
func (tree *treeTree) walkSpine() []treeTreeIndex {
	return tree.spine
}

// nextSibling moves focus to the next sibling of the given node.
func (tree *treeTree) nextSibling(curIx treeTreeIndex) treeTreeIndex {
	tree.cur_ = tree.nodes[curIx].next
	return tree.cur_
}

// truncateSiblings truncates the preceding siblings to the given end
// position, and returns the new current node.
func (tree *treeTree) truncateSiblings(endByteIx int) {
	parentIx := tree.peekUp().unwrap()
	nextChildIx := tree.nodes[parentIx].child
	prevChildIx := treeNone

	// drop or truncate children based on its range
	for nextChildIx != treeNone {
		childIx := nextChildIx
		childEnd := tree.nodes[childIx].item.end
		if childEnd < endByteIx {
			// preserve this node, and go to the next
			prevChildIx = childIx
			nextChildIx = tree.nodes[childIx].next
			continue
		} else if childEnd == endByteIx {
			// this will be the last node
			tree.nodes[childIx].next = treeNone
			// focus to the new last child (this node)
			tree.cur_ = childIx
		} else if tree.nodes[childIx].item.start == endByteIx {
			// check whether the previous character is a backslash
			isPreviousCharBackslashEscape := false
			if body := tree.nodes[childIx].item.body; body.kind == parseItemBody_Text {
				isPreviousCharBackslashEscape = body.b1
			}
			if isPreviousCharBackslashEscape {
				// rescue the backslash as a plain text content
				lastByteIx := endByteIx - 1
				tree.nodes[childIx].item.start = lastByteIx
				tree.nodes[childIx].item.end = endByteIx
				tree.cur_ = childIx
			} else if prevChildIx != treeNone {
				// the node will become empty. drop the node
				// a preceding sibling exists
				tree.nodes[prevChildIx].next = treeNone
				tree.cur_ = prevChildIx
			} else {
				// no preceding siblings. remove the node from the parent
				tree.nodes[parentIx].child = treeNone
				tree.cur_ = treeNone
			}
		} else {
			debugAssert(tree.nodes[childIx].item.start < endByteIx, "assertion failed: self[child_ix].item.start < end_byte_ix")
			debugAssert(endByteIx < childEnd, "assertion failed: end_byte_ix < child_end")
			// truncate the node
			tree.nodes[childIx].item.end = endByteIx
			tree.nodes[childIx].next = treeNone
			// focus to the new last child
			tree.cur_ = childIx
		}
		break
	}
}
