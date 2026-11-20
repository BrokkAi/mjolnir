package similar

import (
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// The crate's tests compare values with insta snapshots: the `{:#?}` text of
// a value, kept in a .snap file. The files are copied unchanged from the
// crate into testdata/snapshots, and this file holds the two things needed to
// use them: reading one, and writing a value the way Rust's pretty Debug does.
// (rs.PrettyDebugString will do the second once the formatter is ported.)

// snapshotAssert is `insta::assert_snapshot!` and
// `insta::assert_debug_snapshot!` against testdata/snapshots/<name>.snap.
func snapshotAssert(t *testing.T, name string, actual string) {
	t.Helper()
	raw, err := os.ReadFile(filepath.Join("testdata", "snapshots", name+".snap"))
	if err != nil {
		t.Fatal(err)
	}
	// The file is a `---` delimited header followed by the contents.
	text := string(raw)
	text = strings.TrimPrefix(text, "---\n")
	_, contents, found := strings.Cut(text, "\n---\n")
	if !found {
		t.Fatalf("snapshot %s has no header", name)
	}
	if snapshotNormalize(contents) != snapshotNormalize(actual) {
		t.Fatalf("snapshot %s differs\nexpected:\n%s\nactual:\n%s", name, contents, actual)
	}
}

// snapshotNormalize is the normalization insta applies to both sides before
// comparing: leading newlines and trailing whitespace are not significant.
func snapshotNormalize(text string) string {
	text = strings.TrimLeft(text, "\r\n")
	text = strings.TrimRight(text, " \t\r\n")
	return strings.ReplaceAll(text, "\r\n", "\n")
}

// debugNode is a value as Rust's Debug sees it.
type debugNode struct {
	// atom is the text of a leaf: a number, a unit variant, a quoted string.
	atom string
	// name is the struct or tuple name; brackets say which builder wrote it.
	name     string
	brackets string // "[]", "{}" or "()"
	fields   []debugField
}

type debugField struct {
	name  string // empty for list and tuple entries
	value debugNode
}

func debugAtom(text string) debugNode { return debugNode{atom: text} }

func debugList(items []debugNode) debugNode {
	node := debugNode{brackets: "[]"}
	for _, item := range items {
		node.fields = append(node.fields, debugField{value: item})
	}
	return node
}

func debugTuple(name string, items ...debugNode) debugNode {
	node := debugNode{name: name, brackets: "()"}
	for _, item := range items {
		node.fields = append(node.fields, debugField{value: item})
	}
	return node
}

func debugStruct(name string, fields ...debugField) debugNode {
	return debugNode{name: name, brackets: "{}", fields: fields}
}

// debugPretty writes node as `{:#?}` does.
func debugPretty(node debugNode) string {
	var out strings.Builder
	debugWrite(&out, node, "")
	return out.String()
}

func debugWrite(out *strings.Builder, node debugNode, indent string) {
	if node.brackets == "" {
		out.WriteString(node.atom)
		return
	}
	out.WriteString(node.name)
	if len(node.fields) == 0 {
		out.WriteString(node.brackets)
		return
	}
	if node.brackets == "{}" {
		out.WriteString(" ")
	}
	out.WriteString(node.brackets[:1])
	out.WriteString("\n")
	for _, field := range node.fields {
		out.WriteString(indent + "    ")
		if field.name != "" {
			out.WriteString(field.name + ": ")
		}
		debugWrite(out, field.value, indent+"    ")
		out.WriteString(",\n")
	}
	out.WriteString(indent)
	out.WriteString(node.brackets[1:])
}

func debugInt(value int) debugNode { return debugAtom(strconv.Itoa(value)) }

// debugStr is the Debug form of a string, for the characters the crate's
// tests use.
func debugStr(value string) debugNode {
	replacer := strings.NewReplacer("\\", "\\\\", "\"", "\\\"", "\n", "\\n", "\r", "\\r", "\t", "\\t")
	return debugAtom("\"" + replacer.Replace(value) + "\"")
}

func debugOption(value rs.Option[int]) debugNode {
	if value, ok := value.Get(); ok {
		return debugTuple("Some", debugInt(value))
	}
	return debugAtom("None")
}

func debugRange(value Range) debugNode {
	return debugAtom(strconv.Itoa(value.Start) + ".." + strconv.Itoa(value.End))
}

func debugDiffTag(tag DiffTag) debugNode {
	return debugAtom([...]string{"Equal", "Delete", "Insert", "Replace"}[tag])
}

func debugChangeTag(tag ChangeTag) debugNode {
	return debugAtom([...]string{"Equal", "Delete", "Insert"}[tag])
}

func debugOp(op DiffOp) debugNode {
	switch op := op.(type) {
	case *DiffOp_Equal:
		return debugStruct("Equal",
			debugField{"old_index", debugInt(op.OldIndex)},
			debugField{"new_index", debugInt(op.NewIndex)},
			debugField{"len", debugInt(op.Len)})
	case *DiffOp_Delete:
		return debugStruct("Delete",
			debugField{"old_index", debugInt(op.OldIndex)},
			debugField{"old_len", debugInt(op.OldLen)},
			debugField{"new_index", debugInt(op.NewIndex)})
	case *DiffOp_Insert:
		return debugStruct("Insert",
			debugField{"old_index", debugInt(op.OldIndex)},
			debugField{"new_index", debugInt(op.NewIndex)},
			debugField{"new_len", debugInt(op.NewLen)})
	case *DiffOp_Replace:
		return debugStruct("Replace",
			debugField{"old_index", debugInt(op.OldIndex)},
			debugField{"old_len", debugInt(op.OldLen)},
			debugField{"new_index", debugInt(op.NewIndex)},
			debugField{"new_len", debugInt(op.NewLen)})
	default:
		panic("unknown op")
	}
}

// debugOps is `format!("{:#?}", ops)` for a `&[DiffOp]`.
func debugOps(ops []DiffOp) string {
	return debugPretty(debugOpsNode(ops))
}

func debugOpsNode(ops []DiffOp) debugNode {
	nodes := []debugNode{}
	for _, op := range ops {
		nodes = append(nodes, debugOp(op))
	}
	return debugList(nodes)
}

// debugGroupedOps is `format!("{:#?}", groups)` for a `Vec<Vec<DiffOp>>`.
func debugGroupedOps(groups [][]DiffOp) string {
	nodes := []debugNode{}
	for _, group := range groups {
		nodes = append(nodes, debugOpsNode(group))
	}
	return debugPretty(debugList(nodes))
}

// debugChanges is `format!("{:#?}", changes)` for a `Vec<Change<&str>>`.
func debugChanges(changes []Change[string]) string {
	nodes := []debugNode{}
	for _, change := range changes {
		nodes = append(nodes, debugStruct("Change",
			debugField{"tag", debugChangeTag(change.Tag())},
			debugField{"old_index", debugOption(change.OldIndex())},
			debugField{"new_index", debugOption(change.NewIndex())},
			debugField{"value", debugStr(change.Value())}))
	}
	return debugPretty(debugList(nodes))
}

// hasRunFinish is the hook of the crate's `test_finish_called` tests: it
// implements only `finish` and leaves the rest to the trait's defaults.
type hasRunFinish struct {
	F0 bool
}

func (self *hasRunFinish) Equal(oldIndex int, newIndex int, len_ int) error {
	return AlgorithmsDiffHookDefaultEqual(self, oldIndex, newIndex, len_)
}

func (self *hasRunFinish) Delete(oldIndex int, oldLen int, newIndex int) error {
	return AlgorithmsDiffHookDefaultDelete(self, oldIndex, oldLen, newIndex)
}

func (self *hasRunFinish) Insert(oldIndex int, newIndex int, newLen int) error {
	return AlgorithmsDiffHookDefaultInsert(self, oldIndex, newIndex, newLen)
}

func (self *hasRunFinish) Replace(oldIndex int, oldLen int, newIndex int, newLen int) error {
	return AlgorithmsDiffHookDefaultReplace(self, oldIndex, oldLen, newIndex, newLen)
}

func (self *hasRunFinish) Finish() error {
	self.F0 = true
	return nil
}

// assertEq is `assert_eq!` for values Go compares with ==.
func assertEq[T comparable](t *testing.T, left T, right T) {
	t.Helper()
	if left != right {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

// assertSliceEq is `assert_eq!` for two slices of comparable values.
func assertSliceEq[T comparable](t *testing.T, left []T, right []T) {
	t.Helper()
	equal := len(left) == len(right)
	for i := 0; equal && i < len(left); i++ {
		equal = left[i] == right[i]
	}
	if !equal {
		t.Fatalf("assertion `left == right` failed\n  left: %#v\n right: %#v", left, right)
	}
}

// fullRange is `0..slice.len()`.
func fullRange[T any](slice []T) Range {
	return Range{Start: 0, End: len(slice)}
}
