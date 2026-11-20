// Port of the tree-sitter-bash parsing in mj-transcript/src/transcript.rs.
//
// The Rust crate parses shell commands with tree-sitter 0.25 and the
// tree-sitter-bash 0.25.1 grammar. The Go build uses the same C runtime and
// grammar through their official Go bindings, so trees, field names and
// error recovery are identical; a pure-Go reimplementation of tree-sitter
// was measured against them and parses some valid scripts differently (for
// example `((i++))`). These bindings use cgo.
//
// The tree is copied into Go values and released before parseShell returns,
// so no node outlives the C tree it came from.

package transcript

import (
	"sync"

	sitter "github.com/tree-sitter/go-tree-sitter"
	bash "github.com/tree-sitter/tree-sitter-bash/bindings/go"
)

var bashLanguage = sync.OnceValue(func() *sitter.Language {
	return sitter.NewLanguage(bash.Language())
})

// bashNode is one node of a parsed tree, copied out of tree-sitter.
type bashNode struct {
	kind     string
	start    int
	end      int
	hasError bool
	named    bool
	parent   *bashNode
	children []*bashNode
	// fields holds each child's field name, "" when it has none.
	fields []string
}

func (n *bashNode) Kind() string   { return n.kind }
func (n *bashNode) StartByte() int { return n.start }
func (n *bashNode) EndByte() int   { return n.end }
func (n *bashNode) HasError() bool { return n.hasError }

func (n *bashNode) Parent() (shellNode, bool) {
	if n.parent == nil {
		return nil, false
	}
	return n.parent, true
}

func (n *bashNode) Children() []shellNode {
	children := make([]shellNode, len(n.children))
	for i, child := range n.children {
		children[i] = child
	}
	return children
}

func (n *bashNode) NamedChildCount() int {
	count := 0
	for _, child := range n.children {
		if child.named {
			count++
		}
	}
	return count
}

func (n *bashNode) NamedChild(index int) (shellNode, bool) {
	for _, child := range n.children {
		if !child.named {
			continue
		}
		if index == 0 {
			return child, true
		}
		index--
	}
	return nil, false
}

func (n *bashNode) ChildByFieldName(name string) (shellNode, bool) {
	for i, child := range n.children {
		if n.fields[i] == name {
			return child, true
		}
	}
	return nil, false
}

func (n *bashNode) ChildrenByFieldName(name string) []shellNode {
	var children []shellNode
	for i, child := range n.children {
		if n.fields[i] == name {
			children = append(children, child)
		}
	}
	return children
}

// parseShell parses source with the bash grammar and returns the root node.
// ok is false when no tree could be built (Rust's Parser::set_language or
// Parser::parse failing).
func parseShell(source string) (root shellNode, ok bool) {
	parser := sitter.NewParser()
	defer parser.Close()
	if err := parser.SetLanguage(bashLanguage()); err != nil {
		return nil, false
	}
	tree := parser.Parse([]byte(source), nil)
	if tree == nil {
		return nil, false
	}
	defer tree.Close()
	cursor := tree.Walk()
	defer cursor.Close()
	return copyBashNode(cursor, nil), true
}

// copyBashNode copies the cursor's node and its descendants, walking with
// the cursor so each child carries the field the grammar gave it, as Rust's
// children_by_field_name reads it.
func copyBashNode(cursor *sitter.TreeCursor, parent *bashNode) *bashNode {
	node := cursor.Node()
	copied := &bashNode{
		kind:     node.Kind(),
		start:    int(node.StartByte()),
		end:      int(node.EndByte()),
		hasError: node.HasError(),
		named:    node.IsNamed(),
		parent:   parent,
	}
	if cursor.GotoFirstChild() {
		for {
			copied.fields = append(copied.fields, cursor.FieldName())
			copied.children = append(copied.children, copyBashNode(cursor, copied))
			if !cursor.GotoNextSibling() {
				break
			}
		}
		cursor.GotoParent()
	}
	return copied
}
