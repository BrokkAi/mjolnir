// Port of pulldown-cmark 0.10.3 src/lib.rs

// Copyright 2015 Google Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

package pulldowncmark

import (
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// CodeBlockKind is the kind of a code block.
type CodeBlockKind interface {
	isCodeBlockKind()
	// IsIndented is CodeBlockKind::is_indented.
	IsIndented() bool
	// IsFenced is CodeBlockKind::is_fenced.
	IsFenced() bool
}

// CodeBlockKind_Indented is an indented code block.
type CodeBlockKind_Indented struct{}

// CodeBlockKind_Fenced is a fenced code block. The value contained in the tag
// describes the language of the code, which may be empty.
type CodeBlockKind_Fenced struct{ F0 string }

func (*CodeBlockKind_Indented) isCodeBlockKind() {}
func (*CodeBlockKind_Fenced) isCodeBlockKind()   {}

func (*CodeBlockKind_Indented) IsIndented() bool { return true }
func (*CodeBlockKind_Fenced) IsIndented() bool   { return false }

func (*CodeBlockKind_Indented) IsFenced() bool { return false }
func (*CodeBlockKind_Fenced) IsFenced() bool   { return true }

var _ = rs.RegisterEnum[CodeBlockKind]("CodeBlockKind",
	rs.Variant[CodeBlockKind_Indented]("Indented", rs.ShapeUnit),
	rs.Variant[CodeBlockKind_Fenced]("Fenced", rs.ShapeTuple),
)

// MetadataBlockKind is the style of a metadata block.
type MetadataBlockKind uint8

const (
	MetadataBlockKind_YamlStyle MetadataBlockKind = iota
	MetadataBlockKind_PlusesStyle
)

var _ = rs.RegisterEnum[MetadataBlockKind]("MetadataBlockKind",
	rs.Const(MetadataBlockKind_YamlStyle, "YamlStyle"),
	rs.Const(MetadataBlockKind_PlusesStyle, "PlusesStyle"),
)

// Tag is a tag for an element that can contain other elements.
type Tag interface {
	isTag()
	// ToEnd is Tag::to_end.
	ToEnd() TagEnd
}

// Tag_Paragraph is a paragraph of text and other inline elements.
type Tag_Paragraph struct{}

// Tag_Heading is a heading, with optional identifier, classes and custom
// attributes. The identifier is prefixed with `#` and the last one in the
// attributes list is chosen, classes are prefixed with `.` and custom
// attributes have no prefix and can optionally have a value (`myattr` or
// `myattr=myvalue`).
type Tag_Heading struct {
	Level   HeadingLevel      `rs:"level"`
	Id      rs.Option[string] `rs:"id"`
	Classes []string          `rs:"classes"`
	// The first item of the tuple is the attr and second one the value.
	Attrs []rs.Tuple2[string, rs.Option[string]] `rs:"attrs"`
}

type Tag_BlockQuote struct{}

// Tag_CodeBlock is a code block.
type Tag_CodeBlock struct{ F0 CodeBlockKind }

// Tag_HtmlBlock is a HTML block.
type Tag_HtmlBlock struct{}

// Tag_List is a list. If the list is ordered the field indicates the number
// of the first item. Contains only list items.
type Tag_List struct{ F0 rs.Option[uint64] } // TODO: add delim and tight for ast (not needed for html)

// Tag_Item is a list item.
type Tag_Item struct{}

// Tag_FootnoteDefinition is a footnote definition. The value contained is the
// footnote's label by which it can be referred to.
type Tag_FootnoteDefinition struct{ F0 string }

// Tag_Table is a table. Contains a vector describing the text-alignment for
// each of its columns.
type Tag_Table struct{ F0 []Alignment }

// Tag_TableHead is a table header. Contains only `TableCell`s. Note that the
// table body starts immediately after the closure of the `TableHead` tag.
// There is no `TableBody` tag.
type Tag_TableHead struct{}

// Tag_TableRow is a table row. Is used both for header rows as body rows.
// Contains only `TableCell`s.
type Tag_TableRow struct{}
type Tag_TableCell struct{}

// span-level tags
type Tag_Emphasis struct{}
type Tag_Strong struct{}
type Tag_Strikethrough struct{}

// Tag_Link is a link.
type Tag_Link struct {
	LinkType LinkType `rs:"link_type"`
	DestUrl  string   `rs:"dest_url"`
	Title    string   `rs:"title"`
	// Identifier of reference links, e.g. `world` in the link `[hello][world]`.
	Id string `rs:"id"`
}

// Tag_Image is an image. The first field is the link type, the second the
// destination URL and the third is a title, the fourth is the link
// identifier.
type Tag_Image struct {
	LinkType LinkType `rs:"link_type"`
	DestUrl  string   `rs:"dest_url"`
	Title    string   `rs:"title"`
	// Identifier of reference links, e.g. `world` in the link `[hello][world]`.
	Id string `rs:"id"`
}

// Tag_MetadataBlock is a metadata block.
type Tag_MetadataBlock struct{ F0 MetadataBlockKind }

func (*Tag_Paragraph) isTag()          {}
func (*Tag_Heading) isTag()            {}
func (*Tag_BlockQuote) isTag()         {}
func (*Tag_CodeBlock) isTag()          {}
func (*Tag_HtmlBlock) isTag()          {}
func (*Tag_List) isTag()               {}
func (*Tag_Item) isTag()               {}
func (*Tag_FootnoteDefinition) isTag() {}
func (*Tag_Table) isTag()              {}
func (*Tag_TableHead) isTag()          {}
func (*Tag_TableRow) isTag()           {}
func (*Tag_TableCell) isTag()          {}
func (*Tag_Emphasis) isTag()           {}
func (*Tag_Strong) isTag()             {}
func (*Tag_Strikethrough) isTag()      {}
func (*Tag_Link) isTag()               {}
func (*Tag_Image) isTag()              {}
func (*Tag_MetadataBlock) isTag()      {}

var _ = rs.RegisterEnum[Tag]("Tag",
	rs.Variant[Tag_Paragraph]("Paragraph", rs.ShapeUnit),
	rs.Variant[Tag_Heading]("Heading", rs.ShapeStruct),
	rs.Variant[Tag_BlockQuote]("BlockQuote", rs.ShapeUnit),
	rs.Variant[Tag_CodeBlock]("CodeBlock", rs.ShapeTuple),
	rs.Variant[Tag_HtmlBlock]("HtmlBlock", rs.ShapeUnit),
	rs.Variant[Tag_List]("List", rs.ShapeTuple),
	rs.Variant[Tag_Item]("Item", rs.ShapeUnit),
	rs.Variant[Tag_FootnoteDefinition]("FootnoteDefinition", rs.ShapeTuple),
	rs.Variant[Tag_Table]("Table", rs.ShapeTuple),
	rs.Variant[Tag_TableHead]("TableHead", rs.ShapeUnit),
	rs.Variant[Tag_TableRow]("TableRow", rs.ShapeUnit),
	rs.Variant[Tag_TableCell]("TableCell", rs.ShapeUnit),
	rs.Variant[Tag_Emphasis]("Emphasis", rs.ShapeUnit),
	rs.Variant[Tag_Strong]("Strong", rs.ShapeUnit),
	rs.Variant[Tag_Strikethrough]("Strikethrough", rs.ShapeUnit),
	rs.Variant[Tag_Link]("Link", rs.ShapeStruct),
	rs.Variant[Tag_Image]("Image", rs.ShapeStruct),
	rs.Variant[Tag_MetadataBlock]("MetadataBlock", rs.ShapeTuple),
)

func (tag *Tag_Paragraph) ToEnd() TagEnd          { return &TagEnd_Paragraph{} }
func (tag *Tag_Heading) ToEnd() TagEnd            { return &TagEnd_Heading{F0: tag.Level} }
func (tag *Tag_BlockQuote) ToEnd() TagEnd         { return &TagEnd_BlockQuote{} }
func (tag *Tag_CodeBlock) ToEnd() TagEnd          { return &TagEnd_CodeBlock{} }
func (tag *Tag_HtmlBlock) ToEnd() TagEnd          { return &TagEnd_HtmlBlock{} }
func (tag *Tag_List) ToEnd() TagEnd               { return &TagEnd_List{F0: tag.F0.IsSome()} }
func (tag *Tag_Item) ToEnd() TagEnd               { return &TagEnd_Item{} }
func (tag *Tag_FootnoteDefinition) ToEnd() TagEnd { return &TagEnd_FootnoteDefinition{} }
func (tag *Tag_Table) ToEnd() TagEnd              { return &TagEnd_Table{} }
func (tag *Tag_TableHead) ToEnd() TagEnd          { return &TagEnd_TableHead{} }
func (tag *Tag_TableRow) ToEnd() TagEnd           { return &TagEnd_TableRow{} }
func (tag *Tag_TableCell) ToEnd() TagEnd          { return &TagEnd_TableCell{} }
func (tag *Tag_Emphasis) ToEnd() TagEnd           { return &TagEnd_Emphasis{} }
func (tag *Tag_Strong) ToEnd() TagEnd             { return &TagEnd_Strong{} }
func (tag *Tag_Strikethrough) ToEnd() TagEnd      { return &TagEnd_Strikethrough{} }
func (tag *Tag_Link) ToEnd() TagEnd               { return &TagEnd_Link{} }
func (tag *Tag_Image) ToEnd() TagEnd              { return &TagEnd_Image{} }
func (tag *Tag_MetadataBlock) ToEnd() TagEnd      { return &TagEnd_MetadataBlock{F0: tag.F0} }

// TagEnd is the end of a `Tag`.
type TagEnd interface {
	isTagEnd()
}

type TagEnd_Paragraph struct{}
type TagEnd_Heading struct{ F0 HeadingLevel }

type TagEnd_BlockQuote struct{}
type TagEnd_CodeBlock struct{}

type TagEnd_HtmlBlock struct{}

// TagEnd_List is the end of a list, `true` for ordered lists.
type TagEnd_List struct{ F0 bool }
type TagEnd_Item struct{}
type TagEnd_FootnoteDefinition struct{}

type TagEnd_Table struct{}
type TagEnd_TableHead struct{}
type TagEnd_TableRow struct{}
type TagEnd_TableCell struct{}

type TagEnd_Emphasis struct{}
type TagEnd_Strong struct{}
type TagEnd_Strikethrough struct{}

type TagEnd_Link struct{}
type TagEnd_Image struct{}

type TagEnd_MetadataBlock struct{ F0 MetadataBlockKind }

func (*TagEnd_Paragraph) isTagEnd()          {}
func (*TagEnd_Heading) isTagEnd()            {}
func (*TagEnd_BlockQuote) isTagEnd()         {}
func (*TagEnd_CodeBlock) isTagEnd()          {}
func (*TagEnd_HtmlBlock) isTagEnd()          {}
func (*TagEnd_List) isTagEnd()               {}
func (*TagEnd_Item) isTagEnd()               {}
func (*TagEnd_FootnoteDefinition) isTagEnd() {}
func (*TagEnd_Table) isTagEnd()              {}
func (*TagEnd_TableHead) isTagEnd()          {}
func (*TagEnd_TableRow) isTagEnd()           {}
func (*TagEnd_TableCell) isTagEnd()          {}
func (*TagEnd_Emphasis) isTagEnd()           {}
func (*TagEnd_Strong) isTagEnd()             {}
func (*TagEnd_Strikethrough) isTagEnd()      {}
func (*TagEnd_Link) isTagEnd()               {}
func (*TagEnd_Image) isTagEnd()              {}
func (*TagEnd_MetadataBlock) isTagEnd()      {}

var _ = rs.RegisterEnum[TagEnd]("TagEnd",
	rs.Variant[TagEnd_Paragraph]("Paragraph", rs.ShapeUnit),
	rs.Variant[TagEnd_Heading]("Heading", rs.ShapeTuple),
	rs.Variant[TagEnd_BlockQuote]("BlockQuote", rs.ShapeUnit),
	rs.Variant[TagEnd_CodeBlock]("CodeBlock", rs.ShapeUnit),
	rs.Variant[TagEnd_HtmlBlock]("HtmlBlock", rs.ShapeUnit),
	rs.Variant[TagEnd_List]("List", rs.ShapeTuple),
	rs.Variant[TagEnd_Item]("Item", rs.ShapeUnit),
	rs.Variant[TagEnd_FootnoteDefinition]("FootnoteDefinition", rs.ShapeUnit),
	rs.Variant[TagEnd_Table]("Table", rs.ShapeUnit),
	rs.Variant[TagEnd_TableHead]("TableHead", rs.ShapeUnit),
	rs.Variant[TagEnd_TableRow]("TableRow", rs.ShapeUnit),
	rs.Variant[TagEnd_TableCell]("TableCell", rs.ShapeUnit),
	rs.Variant[TagEnd_Emphasis]("Emphasis", rs.ShapeUnit),
	rs.Variant[TagEnd_Strong]("Strong", rs.ShapeUnit),
	rs.Variant[TagEnd_Strikethrough]("Strikethrough", rs.ShapeUnit),
	rs.Variant[TagEnd_Link]("Link", rs.ShapeUnit),
	rs.Variant[TagEnd_Image]("Image", rs.ShapeUnit),
	rs.Variant[TagEnd_MetadataBlock]("MetadataBlock", rs.ShapeTuple),
)

// TagEndFromTag is `impl From<Tag> for TagEnd`.
func TagEndFromTag(value Tag) TagEnd {
	return value.ToEnd()
}

// HeadingLevel is the level of a heading. The discriminants are the levels,
// as in Rust (`H1 = 1`).
type HeadingLevel uint8

const (
	HeadingLevel_H1 HeadingLevel = iota + 1
	HeadingLevel_H2
	HeadingLevel_H3
	HeadingLevel_H4
	HeadingLevel_H5
	HeadingLevel_H6
)

var _ = rs.RegisterEnum[HeadingLevel]("HeadingLevel",
	rs.Const(HeadingLevel_H1, "H1"),
	rs.Const(HeadingLevel_H2, "H2"),
	rs.Const(HeadingLevel_H3, "H3"),
	rs.Const(HeadingLevel_H4, "H4"),
	rs.Const(HeadingLevel_H5, "H5"),
	rs.Const(HeadingLevel_H6, "H6"),
)

// String holds the logic of `impl Display for HeadingLevel`.
func (level HeadingLevel) String() string {
	switch level {
	case HeadingLevel_H1:
		return "h1"
	case HeadingLevel_H2:
		return "h2"
	case HeadingLevel_H3:
		return "h3"
	case HeadingLevel_H4:
		return "h4"
	case HeadingLevel_H5:
		return "h5"
	case HeadingLevel_H6:
		return "h6"
	default:
		panic(rs.Unreachable(level))
	}
}

// FmtDisplay is `impl Display for HeadingLevel`. The Rust impl is
// `write!(f, "h1")`, which writes the text as it is and does not look at the
// formatter's width or precision, so this does not pad either.
func (level HeadingLevel) FmtDisplay(f *rs.Formatter) error {
	return f.WriteStr(level.String())
}

// InvalidHeadingLevel is returned when trying to convert a `usize` into a
// `Heading` but it fails because the usize isn't a valid heading level
type InvalidHeadingLevel int

var _ = rs.RegisterNewtype[InvalidHeadingLevel]("InvalidHeadingLevel")

// Error makes InvalidHeadingLevel usable as a Go error. Rust's type has no
// Display; the text is its derived Debug form.
func (e InvalidHeadingLevel) Error() string {
	return "InvalidHeadingLevel(" + strconv.Itoa(int(e)) + ")"
}

// HeadingLevelTryFromUsize is `impl TryFrom<usize> for HeadingLevel`.
func HeadingLevelTryFromUsize(value int) (HeadingLevel, error) {
	switch value {
	case 1:
		return HeadingLevel_H1, nil
	case 2:
		return HeadingLevel_H2, nil
	case 3:
		return HeadingLevel_H3, nil
	case 4:
		return HeadingLevel_H4, nil
	case 5:
		return HeadingLevel_H5, nil
	case 6:
		return HeadingLevel_H6, nil
	default:
		return 0, InvalidHeadingLevel(value)
	}
}

// LinkType is the type specifier for inline links. See Tag_Link for more
// information.
type LinkType uint8

const (
	// Inline link like `[foo](bar)`
	LinkType_Inline LinkType = iota
	// Reference link like `[foo][bar]`
	LinkType_Reference
	// Reference without destination in the document, but resolved by the broken_link_callback
	LinkType_ReferenceUnknown
	// Collapsed link like `[foo][]`
	LinkType_Collapsed
	// Collapsed link without destination in the document, but resolved by the broken_link_callback
	LinkType_CollapsedUnknown
	// Shortcut link like `[foo]`
	LinkType_Shortcut
	// Shortcut without destination in the document, but resolved by the broken_link_callback
	LinkType_ShortcutUnknown
	// Autolink like `<http://foo.bar/baz>`
	LinkType_Autolink
	// Email address in autolink like `<john@example.org>`
	LinkType_Email
)

var _ = rs.RegisterEnum[LinkType]("LinkType",
	rs.Const(LinkType_Inline, "Inline"),
	rs.Const(LinkType_Reference, "Reference"),
	rs.Const(LinkType_ReferenceUnknown, "ReferenceUnknown"),
	rs.Const(LinkType_Collapsed, "Collapsed"),
	rs.Const(LinkType_CollapsedUnknown, "CollapsedUnknown"),
	rs.Const(LinkType_Shortcut, "Shortcut"),
	rs.Const(LinkType_ShortcutUnknown, "ShortcutUnknown"),
	rs.Const(LinkType_Autolink, "Autolink"),
	rs.Const(LinkType_Email, "Email"),
)

// toUnknown maps the link type to an equivalent _Unknown link type.
func (linkType LinkType) toUnknown() LinkType {
	switch linkType {
	case LinkType_Reference:
		return LinkType_ReferenceUnknown
	case LinkType_Collapsed:
		return LinkType_CollapsedUnknown
	case LinkType_Shortcut:
		return LinkType_ShortcutUnknown
	default:
		panic(rs.Unreachable())
	}
}

// Event is a Markdown event. Events are generated in a preorder traversal of
// the document tree, with additional `End` events whenever all of an inner
// node's children have been visited.
type Event interface {
	isEvent()
}

// Event_Start is the start of a tagged element. Events that are yielded after
// this event and before its corresponding `End` event are inside this
// element. Start and end events are guaranteed to be balanced.
type Event_Start struct{ F0 Tag }

// Event_End is the end of a tagged element.
type Event_End struct{ F0 TagEnd }

// Event_Text is a text node.
type Event_Text struct{ F0 string }

// Event_Code is an inline code node.
type Event_Code struct{ F0 string }

// Event_Html is an HTML node.
type Event_Html struct{ F0 string }

// Event_InlineHtml is an inline HTML node.
type Event_InlineHtml struct{ F0 string }

// Event_FootnoteReference is a reference to a footnote with given label,
// which may or may not be defined by an event with a
// `Tag::FootnoteDefinition` tag. Definitions and references to them may occur
// in any order.
type Event_FootnoteReference struct{ F0 string }

// Event_SoftBreak is a soft line break.
type Event_SoftBreak struct{}

// Event_HardBreak is a hard line break.
type Event_HardBreak struct{}

// Event_Rule is a horizontal ruler.
type Event_Rule struct{}

// Event_TaskListMarker is a task list marker, rendered as a checkbox in HTML.
// Contains a true when it is checked.
type Event_TaskListMarker struct{ F0 bool }

func (*Event_Start) isEvent()             {}
func (*Event_End) isEvent()               {}
func (*Event_Text) isEvent()              {}
func (*Event_Code) isEvent()              {}
func (*Event_Html) isEvent()              {}
func (*Event_InlineHtml) isEvent()        {}
func (*Event_FootnoteReference) isEvent() {}
func (*Event_SoftBreak) isEvent()         {}
func (*Event_HardBreak) isEvent()         {}
func (*Event_Rule) isEvent()              {}
func (*Event_TaskListMarker) isEvent()    {}

var _ = rs.RegisterEnum[Event]("Event",
	rs.Variant[Event_Start]("Start", rs.ShapeTuple),
	rs.Variant[Event_End]("End", rs.ShapeTuple),
	rs.Variant[Event_Text]("Text", rs.ShapeTuple),
	rs.Variant[Event_Code]("Code", rs.ShapeTuple),
	rs.Variant[Event_Html]("Html", rs.ShapeTuple),
	rs.Variant[Event_InlineHtml]("InlineHtml", rs.ShapeTuple),
	rs.Variant[Event_FootnoteReference]("FootnoteReference", rs.ShapeTuple),
	rs.Variant[Event_SoftBreak]("SoftBreak", rs.ShapeUnit),
	rs.Variant[Event_HardBreak]("HardBreak", rs.ShapeUnit),
	rs.Variant[Event_Rule]("Rule", rs.ShapeUnit),
	rs.Variant[Event_TaskListMarker]("TaskListMarker", rs.ShapeTuple),
)

// Alignment is the table column text alignment.
type Alignment uint8

const (
	// Default text alignment.
	Alignment_None Alignment = iota
	Alignment_Left
	Alignment_Center
	Alignment_Right
)

var _ = rs.RegisterEnum[Alignment]("Alignment",
	rs.Const(Alignment_None, "None"),
	rs.Const(Alignment_Left, "Left"),
	rs.Const(Alignment_Center, "Center"),
	rs.Const(Alignment_Right, "Right"),
)

// Options is the option struct containing flags for enabling extra features
// that are not part of the CommonMark spec.
//
// It is the `bitflags!` type of the crate: a set of bits with the flag
// constants below, combined with Go's own `|`, `&` and `&^` or with the
// methods bitflags generates (Insert, Remove, Contains, Union, ...). The
// constants are named by the port's rule for associated constants:
// `Options::ENABLE_TABLES` is OptionsEnableTables.
type Options uint32

const (
	OptionsEnableTables Options = 1 << 1
	// GitHub-compatible footnote syntax.
	//
	// Footnotes are referenced with the syntax `[^IDENT]`,
	// and defined with an identifier followed by a colon at top level.
	//
	// ---
	//
	// ```markdown
	// Footnote referenced [^1].
	//
	// [^1]: footnote defined
	// ```
	//
	// Footnote referenced [^1].
	//
	// [^1]: footnote defined
	OptionsEnableFootnotes        Options = 1 << 2
	OptionsEnableStrikethrough    Options = 1 << 3
	OptionsEnableTasklists        Options = 1 << 4
	OptionsEnableSmartPunctuation Options = 1 << 5
	// Extension to allow headings to have ID and classes.
	//
	// `# text { #id .class1 .class2 myattr other_attr=myvalue }`
	// is interpreted as a level 1 heading
	// with the content `text`, ID `id`, classes `class1` and `class2` and
	// custom attributes `myattr` (without value) and
	// `other_attr` with value `myvalue`.
	// Note that ID, classes, and custom attributes should be space-separated.
	OptionsEnableHeadingAttributes Options = 1 << 6
	// Metadata blocks in YAML style, i.e.:
	// - starting with a `---` line
	// - ending with a `---` or `...` line
	OptionsEnableYamlStyleMetadataBlocks Options = 1 << 7
	// Metadata blocks delimited by:
	// - `+++` line at start
	// - `+++` line at end
	OptionsEnablePlusesDelimitedMetadataBlocks Options = 1 << 8
	// Older footnote syntax. This flag implies `ENABLE_FOOTNOTES`, changing it to use an
	// older syntax instead of the new, default, GitHub-compatible syntax.
	//
	// New syntax is different from the old syntax regarding
	// indentation, nesting, and footnote references with no definition:
	//
	// ```markdown
	// [^1]: In new syntax, this is two footnote definitions.
	// [^2]: In old syntax, this is a single footnote definition with two lines.
	//
	// [^3]:
	//
	//     In new syntax, this is a footnote with two paragraphs.
	//
	//     In old syntax, this is a footnote followed by a code block.
	//
	// In new syntax, this undefined footnote definition renders as
	// literal text [^4]. In old syntax, it creates a dangling link.
	// ```
	OptionsEnableOldFootnotes Options = (1 << 9) | (1 << 2)
)

var _ = rs.RegisterNewtype[Options]("Options")

// optionsFlags lists the named flags in declaration order, as bitflags'
// iteration and Debug output visit them.
var optionsFlags = [...]struct {
	name  string
	value Options
}{
	{"ENABLE_TABLES", OptionsEnableTables},
	{"ENABLE_FOOTNOTES", OptionsEnableFootnotes},
	{"ENABLE_STRIKETHROUGH", OptionsEnableStrikethrough},
	{"ENABLE_TASKLISTS", OptionsEnableTasklists},
	{"ENABLE_SMART_PUNCTUATION", OptionsEnableSmartPunctuation},
	{"ENABLE_HEADING_ATTRIBUTES", OptionsEnableHeadingAttributes},
	{"ENABLE_YAML_STYLE_METADATA_BLOCKS", OptionsEnableYamlStyleMetadataBlocks},
	{"ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS", OptionsEnablePlusesDelimitedMetadataBlocks},
	{"ENABLE_OLD_FOOTNOTES", OptionsEnableOldFootnotes},
}

// OptionsEmpty is Options::empty: no flag set.
func OptionsEmpty() Options { return 0 }

// OptionsAll is Options::all: every defined flag set.
func OptionsAll() Options {
	var all Options
	for _, flag := range optionsFlags {
		all |= flag.value
	}
	return all
}

// OptionsFromBits is Options::from_bits: the flags for bits, or None when
// bits holds a bit that no flag defines.
func OptionsFromBits(bits uint32) rs.Option[Options] {
	truncated := OptionsFromBitsTruncate(bits)
	if uint32(truncated) == bits {
		return rs.Some(truncated)
	}
	return rs.None[Options]()
}

// OptionsFromBitsTruncate is Options::from_bits_truncate: the flags for bits,
// without the bits no flag defines.
func OptionsFromBitsTruncate(bits uint32) Options {
	return Options(bits) & OptionsAll()
}

// OptionsFromBitsRetain is Options::from_bits_retain: bits as they are.
func OptionsFromBitsRetain(bits uint32) Options { return Options(bits) }

// OptionsFromName is Options::from_name: the flag with the Rust name
// (`"ENABLE_TABLES"`).
func OptionsFromName(name string) rs.Option[Options] {
	for _, flag := range optionsFlags {
		if flag.name == name {
			return rs.Some(flag.value)
		}
	}
	return rs.None[Options]()
}

// Bits is Options::bits.
func (options Options) Bits() uint32 { return uint32(options) }

// IsEmpty is Options::is_empty.
func (options Options) IsEmpty() bool { return options == 0 }

// IsAll is Options::is_all.
func (options Options) IsAll() bool { return OptionsAll()|options == options }

// Intersects is Options::intersects: whether any flag of other is set.
func (options Options) Intersects(other Options) bool { return options&other != 0 }

// Contains is Options::contains: whether every flag of other is set.
func (options Options) Contains(other Options) bool { return options&other == other }

// Insert is Options::insert.
func (options *Options) Insert(other Options) { *options |= other }

// Remove is Options::remove.
func (options *Options) Remove(other Options) { *options &^= other }

// Toggle is Options::toggle.
func (options *Options) Toggle(other Options) { *options ^= other }

// Set is Options::set: Insert when value holds, Remove otherwise.
func (options *Options) Set(other Options, value bool) {
	if value {
		options.Insert(other)
	} else {
		options.Remove(other)
	}
}

// Union is Options::union (`a | b`).
func (options Options) Union(other Options) Options { return options | other }

// Intersection is Options::intersection (`a & b`).
func (options Options) Intersection(other Options) Options { return options & other }

// Difference is Options::difference (`a - b`).
func (options Options) Difference(other Options) Options { return options &^ other }

// SymmetricDifference is Options::symmetric_difference (`a ^ b`).
func (options Options) SymmetricDifference(other Options) Options { return options ^ other }

// Complement is Options::complement (`!a`): the defined flags that are not
// set.
func (options Options) Complement() Options { return OptionsFromBitsTruncate(^uint32(options)) }

// optionsInternalBitFlags is the private type bitflags wraps the bits in. Its
// Debug impl writes the set flags by name, separated by " | ", then any bits
// that no flag names in hexadecimal; the empty set is written as `0x0`.
type optionsInternalBitFlags Options

func (internal optionsInternalBitFlags) debugString() string {
	options := Options(internal)
	if options.IsEmpty() {
		return "0x0"
	}
	var out strings.Builder
	first := true
	remaining := options
	for _, flag := range optionsFlags {
		// Short-circuit if our state is empty
		if remaining.IsEmpty() {
			break
		}
		// If the flag is set in the original source _and_ it has bits that
		// haven't been covered by a previous flag yet then yield it.
		if !options.Contains(flag.value) || !remaining.Intersects(flag.value) {
			continue
		}
		if !first {
			out.WriteString(" | ")
		}
		first = false
		out.WriteString(flag.name)
		remaining.Remove(flag.value)
	}
	// Append any extra bits that correspond to flags to the end of the format
	if !remaining.IsEmpty() {
		if !first {
			out.WriteString(" | ")
		}
		out.WriteString("0x")
		out.WriteString(strconv.FormatUint(uint64(remaining), 16))
	}
	return out.String()
}

// FmtDebug is the Debug impl of bitflags' internal type.
func (internal optionsInternalBitFlags) FmtDebug(f *rs.Formatter) error {
	return f.WriteStr(internal.debugString())
}

// debugString is the `{:?}` text of Options: the Debug that bitflags derives
// for the tuple struct around its internal type, as in
// `Options(ENABLE_TABLES | ENABLE_STRIKETHROUGH)` and `Options(0x0)`.
func (options Options) debugString() string {
	return "Options(" + optionsInternalBitFlags(options).debugString() + ")"
}

// FmtDebug is the Debug impl that bitflags derives for Options.
func (options Options) FmtDebug(f *rs.Formatter) error {
	return f.DebugTuple("Options").Field(optionsInternalBitFlags(options)).Finish()
}

func (options Options) hasGfmFootnotes() bool {
	return options.Contains(OptionsEnableFootnotes) && !options.Contains(OptionsEnableOldFootnotes)
}
